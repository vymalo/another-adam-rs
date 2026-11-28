//! The person's own directory as a [`CodeWorkspace`]: a run never owns, cleans or restores it
//! ([ADR 0047], slice 7, decision 8).
//!
//! The coder's [`ToolEnv`](https://github.com/vymalo/another-adam-rs/blob/main/bin/adam-coder/src/tools/workspace.rs)
//! owns the directory it works in: it snapshots the slot before a command and **restores** it after.
//! A person's own directory is the opposite: the run changes it and the change stays, so
//! [`PersonDir`] takes no snapshot to undo. What it does instead is *report*: its guard records a
//! listing of the place before a command and compares it after, and
//! [`after_run`](CommandWorkspace::after_run) turns that comparison into the sentence the model
//! reads — the files a command added, changed and removed.
//!
//! The report travels through the workspace, not through the guard, because
//! [`CommandGuard::settle`] consumes the guard and
//! `after_run` is given no guard: the guard writes what it saw into a map keyed by the place, and
//! `after_run` reads that entry once and **clears** it, so a command that never reaches its report
//! cannot leak into the next one. The invariant — **one command at a time per place** — is
//! enforced, not described: [`guard`](CommandWorkspace::guard) refuses with the message the tool
//! appends to its error result when a second guard is taken for a place that already has one open,
//! and the first guard's entry is left untouched so its report settles as it would have.
//!
//! Paths stay confined to the place by the crate's rule: the place is opened once
//! ([`SlotDir::open`]) and every path a tool uses is checked against it by the tools themselves.
//! The listing here reads names only — it follows no symbolic link, skips `.git`, and stops at
//! [`MAX_LISTED`] files so a person's home directory cannot make a command's report cost more than
//! the command.
//!
//! [ADR 0047]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0047-adam-rs-can-be-embedded-in-an-application.md

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use adam_llm_agent::{KeptOutputs, MemoryKept, ToolCtx, ToolError, ToolOutput};
use adam_workspace::{EnvSession, LocalSession, SlotDir};
use async_trait::async_trait;

use crate::commands::{CommandGuard, CommandLimits, CommandWorkspace, Guarding, Settled};
use crate::git::GitAt;
use crate::sandbox::Meter;
use crate::{CodeWorkspace, Intent, Outcome, Place, Redactor};

/// The limits of a [`PersonDir`]'s commands unless it is given others: 30 seconds, and 16 KiB of
/// output shown.
pub const DEFAULT_LIMITS: CommandLimits = CommandLimits {
    timeout: Duration::from_secs(30),
    output_tail: 16 * 1024,
};

/// The place a [`PersonDir`] gives when a tool names none, unless it is given another name.
pub const PLACE: &str = "person";

/// The most files one listing keeps: past it the report says the place is larger than it lists.
pub const MAX_LISTED: usize = 20_000;

/// What one file looked like when a guard listed it: its length, and its modification time when the
/// file system gives one. A change of either is a change of the file.
type Fingerprint = (u64, Option<SystemTime>);

/// Every file below a place's root, by its path relative to the root with `/` as the separator.
type Listing = BTreeMap<String, Fingerprint>;

/// What a guard saw of a place, waiting for the report that reads it.
#[derive(Debug)]
struct Seen {
    before: Listing,
    after: Option<Listing>,
    /// The listing stopped at [`MAX_LISTED`], so what it says is a lower bound.
    cut: bool,
}

type SeenByPlace = Arc<Mutex<BTreeMap<PathBuf, Seen>>>;

/// A workspace of a person's own directory: one place, which a tool may write, that a run never
/// owns, cleans or restores.
pub struct PersonDir {
    places: Vec<Place>,
    redactor: Redactor,
    limits: CommandLimits,
    kept: MemoryKept,
    seen: SeenByPlace,
}

impl fmt::Debug for PersonDir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PersonDir")
            .field("places", &self.places)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl PersonDir {
    /// The directory `dir` as a place named [`PLACE`], which a tool may write, whose redactor holds
    /// `secrets`.
    ///
    /// # Errors
    ///
    /// The directory cannot be opened: it does not exist, is not a directory, or is not readable.
    pub fn new<S: AsRef<str>>(
        dir: &Path,
        secrets: impl IntoIterator<Item = S>,
    ) -> std::io::Result<Self> {
        Self::named(PLACE, dir, secrets)
    }

    /// The same, with a place name of its own (the name a model writes to work in it).
    ///
    /// # Errors
    ///
    /// As [`new`](Self::new).
    pub fn named<S: AsRef<str>>(
        name: &str,
        dir: &Path,
        secrets: impl IntoIterator<Item = S>,
    ) -> std::io::Result<Self> {
        // `GitAt::directory` opens the directory once and holds it: this is where it says, as an
        // error, when it cannot.
        SlotDir::open(dir)?;
        let redactor = Redactor::new(secrets);
        // A clone shares the secrets: one added later is scrubbed from what is kept too.
        let scrubber = redactor.clone();
        let kept = MemoryKept::new()
            .with_margin(redactor.margin())
            .scrubbing(move |text| scrubber.scrub(text).into_owned());
        Ok(Self {
            places: vec![Place::new(name, GitAt::directory(dir)?)],
            redactor,
            limits: DEFAULT_LIMITS,
            kept,
            seen: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }

    /// The same workspace, whose commands run within `limits`.
    #[must_use]
    pub fn with_limits(mut self, limits: CommandLimits) -> Self {
        self.limits = limits;
        self
    }

    /// The redactor, for a test or a host that scrubs what it shows.
    pub fn redactor_ref(&self) -> &Redactor {
        &self.redactor
    }
}

#[async_trait]
impl CodeWorkspace for PersonDir {
    async fn place(
        &self,
        _ctx: &ToolCtx,
        repo: Option<&str>,
        intent: Intent,
    ) -> Result<Place, Outcome> {
        let place = match repo.map(str::trim).filter(|r| !r.is_empty()) {
            Some(name) => self.places.iter().find(|p| p.name() == name),
            None => {
                let mut writable = self.places.iter().filter(|p| p.is_writable());
                match (writable.next(), writable.next()) {
                    (Some(only), None) => Some(only),
                    _ => None,
                }
            }
        };
        let Some(place) = place else {
            let names: Vec<&str> = self.places.iter().map(Place::name).collect();
            return Err(Ok(ToolOutput::error(format!(
                "no such place; the places are: {}",
                names.join(", ")
            ))));
        };
        if intent == Intent::Write && !place.is_writable() {
            return Err(Ok(ToolOutput::error(format!(
                "{} can be read, not written",
                place.name()
            ))));
        }
        Ok(place.clone())
    }

    fn redactor(&self) -> &Redactor {
        &self.redactor
    }

    /// Nothing follows a write here: the person sees the change, and the run does not restore it.
    async fn after_write(
        &self,
        _ctx: &ToolCtx,
        _place: &Place,
        _paths: &[String],
    ) -> Result<String, ToolError> {
        Ok(String::new())
    }
}

#[async_trait]
impl CommandWorkspace for PersonDir {
    async fn session(&self, ctx: &ToolCtx) -> Result<Arc<dyn EnvSession>, ToolError> {
        let session = LocalSession::shared();
        if let Some(meter) = Meter::of(&*session) {
            meter.report_session(ctx);
        }
        Ok(session)
    }

    fn limits(&self) -> CommandLimits {
        self.limits
    }

    fn kept(&self) -> &dyn KeptOutputs {
        &self.kept
    }

    /// A guard that watches the place **to report it, never to undo it**: a person's directory is
    /// never restored ([ADR 0047], decision 8).
    ///
    /// The guard is **exclusive**: when another guard for the same place is still open, this
    /// returns the refusal the model reads (the tool appends `refusal_note`) and leaves the first
    /// guard's entry untouched, so its report settles as it would have. The first guard is taken
    /// until [`Recorded::settle`] (its drop) runs, which is exactly one command's life — two guards
    /// for the same place would race over the listing's `Seen` and corrupt the report.
    ///
    /// [ADR 0047]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0047-adam-rs-can-be-embedded-in-an-application.md
    async fn guard(
        &self,
        _ctx: &ToolCtx,
        place: &Place,
        _guarding: Guarding,
    ) -> Result<Box<dyn CommandGuard>, String> {
        let key = place.path().to_path_buf();
        let (before, cut) = match listing(&key) {
            Ok(listed) => listed,
            Err(e) => return Err(format!("{} cannot be listed: {e}", place.name())),
        };
        {
            let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
            if seen.contains_key(&key) {
                return Err(format!(
                    "a command is already running in `{}`; wait for it to finish",
                    place.name()
                ));
            }
            seen.insert(
                key.clone(),
                Seen {
                    before,
                    after: None,
                    cut,
                },
            );
        }
        Ok(Box::new(Recorded {
            key,
            seen: Arc::clone(&self.seen),
        }))
    }

    /// What the command changed: the files it added, changed and removed, read from the listing its
    /// guard took before it ran. Nothing is undone, and nothing is restored.
    async fn after_run(&self, _ctx: &ToolCtx, place: &Place) -> Result<String, ToolError> {
        let key = place.path();
        let seen = {
            let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
            seen.remove(key)
        };
        let Some(seen) = seen else {
            return Ok(String::new());
        };
        let Some(after) = seen.after else {
            return Ok(String::new());
        };
        Ok(report(&seen.before, &after, seen.cut))
    }
}

/// The guard of one command in a person's directory: it lists the place again and says whether
/// anything changed. It **undoes nothing**, on purpose, and its verdict is that of the files.
struct Recorded {
    key: PathBuf,
    seen: SeenByPlace,
}

#[async_trait]
impl CommandGuard for Recorded {
    async fn settle(self: Box<Self>) -> Settled {
        let (after, cut) = match listing(&self.key) {
            Ok(listed) => listed,
            // The place cannot be read now: whether it changed is not known, and nothing is undone.
            Err(_) => return Settled::Unwatched,
        };
        let changed = {
            let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
            match seen.get_mut(&self.key) {
                Some(entry) => {
                    entry.cut = entry.cut || cut;
                    let changed = entry.before != after;
                    entry.after = Some(after);
                    changed
                }
                None => false,
            }
        };
        if changed {
            Settled::FilesChanged
        } else {
            Settled::Unchanged
        }
    }
}

/// What a listing read: the files, and whether it stopped at [`MAX_LISTED`].
///
/// # Errors
///
/// The root cannot be read. A directory below it that cannot be read is **skipped**, not an error:
/// one unreadable corner of a person's home must not refuse every command.
fn listing(root: &Path) -> std::io::Result<(Listing, bool)> {
    let mut out = Listing::new();
    let mut cut = false;
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if dir == root => return Err(e),
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            // The run's own bookkeeping is not the person's change, and a repository's `.git` moves
            // with every commit; both are left out of the report.
            if name == ".git" {
                continue;
            }
            let rel = if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}/{name}")
            };
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                stack.push((entry.path(), rel));
                continue;
            }
            // A symbolic link is left out: what it points at is another file's change, and a report
            // that followed one would name a path outside the place.
            if kind.is_symlink() {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if out.len() >= MAX_LISTED {
                cut = true;
                break;
            }
            out.insert(rel, (meta.len(), meta.modified().ok()));
        }
        if cut {
            break;
        }
    }
    Ok((out, cut))
}

/// The sentence a run's report adds for a command that changed the person's directory: empty when
/// nothing changed, else one line per kind, each naming at most `PER_KIND` files.
fn report(before: &Listing, after: &Listing, cut: bool) -> String {
    const PER_KIND: usize = 8;
    let mut added = Vec::new();
    let mut changed = Vec::new();
    let mut removed = Vec::new();
    for (path, fingerprint) in after {
        match before.get(path) {
            None => added.push(path.as_str()),
            Some(was) if was != fingerprint => changed.push(path.as_str()),
            Some(_) => {}
        }
    }
    for path in before.keys() {
        if !after.contains_key(path) {
            removed.push(path.as_str());
        }
    }
    if added.is_empty() && changed.is_empty() && removed.is_empty() {
        return String::new();
    }
    let mut out = String::from("\nThe person's own directory changed and is not restored");
    if cut {
        out.push_str(" (the listing stopped at its limit, so this is a lower bound)");
    }
    out.push(':');
    for (label, files) in [
        ("added", &added),
        ("changed", &changed),
        ("removed", &removed),
    ] {
        if files.is_empty() {
            continue;
        }
        let shown: Vec<&str> = files.iter().take(PER_KIND).copied().collect();
        let rest = files.len().saturating_sub(shown.len());
        out.push_str(&format!(
            "\n- {label} {}: {}",
            files.len(),
            shown.join(", ")
        ));
        if rest > 0 {
            out.push_str(&format!(" and {rest} more"));
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::Arc;

    use adam_llm_agent::ToolCtx;
    use adam_runtime::NoopSink;

    use super::*;
    use crate::commands::Guarding;

    /// A person's directory on disk, the one place it owns, returned as the [`Place`] the tools get.
    fn rig() -> (tempfile::TempDir, Place) {
        let dir = tempfile::tempdir().unwrap();
        let place = Place::new(PLACE, GitAt::directory(dir.path()).unwrap());
        (dir, place)
    }

    fn ctx() -> ToolCtx {
        ToolCtx::detached("guard-test", "c1", Arc::new(NoopSink))
    }

    /// Two guards for the same place: the first settles `Unchanged`, the second is refused.
    /// The first guard's report is not lost — `after_run` settles it `Unchanged`.
    #[tokio::test]
    async fn a_second_guard_for_the_same_place_is_refused() {
        let (_dir, place) = rig();
        let person = PersonDir::new(place.path(), std::iter::empty::<&str>()).unwrap();

        let first = person
            .guard(&ctx(), &place, Guarding::Look)
            .await
            .expect("the first guard is taken");
        let second = match person.guard(&ctx(), &place, Guarding::Look).await {
            Ok(_) => panic!("the second guard is refused"),
            Err(reason) => reason,
        };
        assert!(
            second.contains("already running"),
            "the refusal names the reason: {second}"
        );

        // The first guard settles `Unchanged`: nothing changed in the directory between guard and
        // settle, so the listing it took still matches.
        let settled = first.settle().await;
        assert_eq!(settled, Settled::Unchanged);

        // The refusal left the entry untouched, so `after_run` reads it once, settles the report,
        // and clears it.
        let report = person.after_run(&ctx(), &place).await.unwrap();
        assert!(
            report.is_empty(),
            "the report is empty: nothing changed: {report:?}"
        );

        // A fresh guard is taken now: the entry was cleared, so a second command can start.
        let third = person
            .guard(&ctx(), &place, Guarding::Look)
            .await
            .expect("a guard after `after_run` is taken");
        let settled = third.settle().await;
        assert_eq!(settled, Settled::Unchanged);
    }

    /// A change between guard and settle settles `FilesChanged`; the report names the file the
    /// command added, the entry is cleared, and a second guard after that is taken.
    #[tokio::test]
    async fn a_changing_command_settles_files_changed_and_a_second_guard_is_taken() {
        let (dir, place) = rig();
        let person = PersonDir::new(place.path(), std::iter::empty::<&str>()).unwrap();

        let guard = person
            .guard(&ctx(), &place, Guarding::Make)
            .await
            .expect("the guard is taken");
        // The command runs and adds a file.
        std::fs::write(dir.path().join("by_test.txt"), "hello\n").unwrap();
        let settled = guard.settle().await;
        assert_eq!(settled, Settled::FilesChanged);
        let report = person.after_run(&ctx(), &place).await.unwrap();
        assert!(
            report.contains("added 1") && report.contains("by_test.txt"),
            "the report names the file the command added: {report:?}"
        );

        let again = person
            .guard(&ctx(), &place, Guarding::Look)
            .await
            .expect("after the report, a new guard is taken");
        let settled = again.settle().await;
        assert_eq!(settled, Settled::Unchanged);
    }
}
