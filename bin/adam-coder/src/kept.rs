//! Kept output on the workspace volume: what `read_output` reads.
//!
//! A command's long output, and a long result of an MCP server, are kept whole under
//! `<WORKSPACE_ROOT>/outputs/<root run>/<id>.txt` ([`FileKept`], an [`adam_llm_agent::KeptOutputs`]),
//! and the model is shown the first page or the head and the tail with an output id.
//!
//! * **Not in the journal.** The files are on the volume, beside the notes (`coder/<run>.json`) and
//!   the workspaces. A worker that takes the run over reads the same files, whatever environment the
//!   run's commands use: the output is captured by this process from the command's pipes, and
//!   `read_output` reads it here, so a run pod never sees `outputs/` (a pod mounts
//!   only the slots of its run).
//! * **Scrubbed before it is written.** [`FileKept::keep`](adam_llm_agent::KeptOutputs::keep) scrubs
//!   the whole text with the process's [`Redactor`] and then cuts and folds it, so no secret it knows
//!   is ever on the volume, and a secret is never split by a cut, a fold or a page boundary (the
//!   pages are made from the scrubbed file). A text that was cut by its producer (`omitted` not 0)
//!   ends wherever the limit fell, perhaps in the first half of a secret, and the scrubbed length says
//!   nothing about where a raw offset falls: its end is cut back by [`adam_llm_agent::scrub_open_end`]
//!   (scrubbed whole and without its last 64 KiB; what the two agree on is kept). A secret registered **after** the keep is not in the
//!   file's text, and the tools' `Redacting` layer scrubs every page on its way out: that layer,
//!   not the file, is the read-time guarantee.
//! * **Private, and no link is followed on either side.** The directories are opened one component
//!   at a time below the workspace root without following a link (`SlotDir::walk`), made `0700`, and
//!   the file is made in the held run directory with `O_EXCL` and `O_NOFOLLOW`, `0600`, written
//!   under a temporary name and renamed; a link or a file in the way is refused (`NOT kept`), the
//!   volume's paths are never in a message, and a file with more than one hard link is not read. A
//!   command of the **local** environment runs as the same user and
//!   **can rewrite a kept file** (or fill the directory), as it can already rewrite the notes and the
//!   other slots: what `read_output` returns is no more trusted than the output of a command was.
//!   Nothing decides on it: the verdict of `run_checks` is its exit code, never the kept text.
//! * **Bounded.** One output keeps at most [`MAX_KEPT_BYTES`](adam_llm_agent::MAX_KEPT_BYTES) (16 MiB;
//!   a longer one is cut, and its last line says how much is kept) and a run at most [`RUN_KEPT_MAX`]
//!   (64 MiB) of the files **this process names** (`o-` and 16 hex characters and `.txt`: a file of
//!   another name in the directory is not counted); the check and the write are made one at a time
//!   in this process (two workers on one volume can overshoot by one output each). Temporary files
//!   left by a crash are removed when they are over ten minutes old.
//! * **Removed with the run.** [`Janitor`](crate::Janitor) removes `outputs/<run>` when the run's
//!   workspace goes, and the directories of runs that have no workspace any more. It lists and removes
//!   through the same no-link directory handles (`SlotDir::walk`, entries unlinked relative to a held
//!   descriptor, a link unlinked and never followed), so a command that made `outputs` a link to the
//!   workspaces makes the janitor remove nothing (and the log says "not removed", with no path).

use std::collections::HashSet;
use std::io::{self, BufReader, Write as _};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use adam_llm_agent::paging::{PageError, page_lines};
use adam_llm_agent::{KeptError, KeptInfo, KeptOutputs, count_lines, is_output_id, prepare};
use adam_workspace::safe_fs::{Dir, SlotDir, WalkError};
use async_trait::async_trait;

use crate::redact::Redactor;

/// The most bytes one run keeps on the volume.
pub const RUN_KEPT_MAX: u64 = 64 * 1024 * 1024;

/// A size for a message: whole MiB when there are some, else KiB, else bytes.
pub fn size_text(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{} MiB", bytes / (1024 * 1024))
    } else if bytes >= 1024 {
        format!("{} KiB", bytes / 1024)
    } else {
        format!("{bytes} bytes")
    }
}

/// The length of the part of `file` (of `size` bytes) that ends with a newline: what a reader of a
/// file that is still being written may count and show, since the last block may be half there.
fn complete_len(file: &std::fs::File, size: u64) -> io::Result<u64> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut file = file;
    let mut end = size;
    let mut buf = [0u8; 8192];
    while end > 0 {
        let start = end.saturating_sub(buf.len() as u64);
        let n = usize::try_from(end - start).unwrap_or(buf.len());
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buf[..n])?;
        if let Some(at) = buf[..n].iter().rposition(|b| *b == b'\n') {
            file.seek(SeekFrom::Start(0))?;
            return Ok(start + at as u64 + 1);
        }
        end = start;
    }
    file.seek(SeekFrom::Start(0))?;
    Ok(0)
}

/// A leftover temporary file is removed when it is older than this.
const STALE_TMP: Duration = Duration::from_secs(600);

/// One check-then-write at a time in this process.
static KEEP_LOCK: Mutex<()> = Mutex::new(());

/// Kept outputs as files under `<root>/outputs`.
#[derive(Debug, Clone)]
pub struct FileKept {
    root: std::path::PathBuf,
    redactor: Redactor,
    /// The outputs this process is writing now (`(run, id)`): a page of one ends with
    /// `[running: ...]` and not `[end: ...]`, because more lines are coming.
    live: Arc<Mutex<HashSet<(String, String)>>>,
    /// The most bytes a run keeps ([`RUN_KEPT_MAX`] unless [`with_run_max`](Self::with_run_max)).
    run_max: u64,
}

/// A run id that cannot leave the directory.
fn usable_run(run: &str) -> bool {
    !run.is_empty()
        && run.len() <= 128
        && run.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// What the model is told of a volume problem: never a path, never the system's words.
fn io_error(e: &io::Error) -> KeptError {
    KeptError::Io(format!("the volume refused it ({})", e.kind()))
}

fn walk_error(e: &WalkError) -> KeptError {
    match e {
        WalkError::Link(_) | WalkError::NotDirectory(_) => KeptError::Refused(
            "the place for kept output is not usable (something else is where its directory \
             should be)"
                .to_owned(),
        ),
        WalkError::Missing(_) => KeptError::NotFound(String::new()),
        WalkError::Io(e) => io_error(e),
    }
}

/// How the removal of a run's outputs ended.
enum Removal {
    Done,
    /// `outputs` is a link or a file: nothing was removed.
    Hijacked,
    /// Something nested too deep was left.
    TooDeep,
}

/// Where `outputs` is.
enum Place {
    Dir(Dir),
    Missing,
    /// A link or a file is where the directory should be.
    Hijacked,
}

/// Said when `outputs` is a link or a file: a command of the local environment can make it one.
fn warn_hijacked() {
    tracing::warn!(
        "the place for kept output is not a directory of its own (a link or a file is where it \
         should be): kept output not removed"
    );
}

/// How deep a run's directory may nest before removing it is refused: nothing of ours does.
const MAX_REMOVE_DEPTH: usize = 32;

/// Remove what is in `dir`, below it only: a directory is opened without following a link and
/// emptied first, a link or a file is unlinked.
///
/// A directory that cannot be opened (a command made it `000`) is made private through the handle
/// that holds it and opened again, once. `false` when something was left because it is nested too
/// deep: what could be removed is.
fn remove_contents(dir: &Dir, depth: usize) -> io::Result<bool> {
    let mut complete = true;
    for (name, meta) in dir.entries()? {
        if !meta.is_dir() {
            dir.unlink(&name)?;
        } else if depth >= MAX_REMOVE_DEPTH {
            complete = false;
        } else {
            let inner = match dir.open_dir(&name) {
                Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                    dir.make_private(&name)?;
                    dir.open_dir(&name)?
                }
                other => other?,
            };
            if remove_contents(&inner, depth + 1)? {
                dir.rmdir(&name)?;
            } else {
                complete = false;
            }
        }
    }
    Ok(complete)
}

/// The runs whose removal went wrong, so that the log says it once for a run in a process and not at
/// every sweep.
static WARNED: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn warn_once(run: &str, what: &str) {
    let mut warned = WARNED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !warned.iter().any(|r| r == run) {
        if warned.len() >= 1024 {
            warned.clear();
        }
        warned.push(run.to_owned());
        tracing::warn!(%run, problem = %what, "a kept output could not be written");
    }
}

/// A blocking task that did not finish: a fixed text, since the text of a panic can hold anything.
fn join_failed() -> KeptError {
    KeptError::Io("the volume could not be reached".to_owned())
}

fn not_regular() -> KeptError {
    KeptError::Refused("that output file is not usable (it is not a regular file)".to_owned())
}

/// `o-<16 hex>.txt`: a file this process makes.
fn is_ours(name: &str) -> bool {
    name.strip_suffix(".txt").is_some_and(is_output_id)
}

/// `.o-<16 hex>.<pid>.<n>.tmp`
fn is_tmp(name: &str) -> bool {
    name.starts_with(".o-") && name.ends_with(".tmp")
}

/// What the files this process names in `dir` take, the file `except` not counted.
fn used_by_others(dir: &Dir, except: &str) -> Result<u64, KeptError> {
    let mut used = 0u64;
    for (entry, meta) in dir.entries().map_err(|e| io_error(&e))? {
        let entry_name = entry.to_string_lossy();
        if is_ours(&entry_name) && *entry_name != *except && meta.is_file() {
            used += meta.size();
        }
    }
    Ok(used)
}

/// The most a record of a command may be.
const MAX_RECORD_BYTES: u64 = 64 * 1024;

fn read_record(dir: &Dir, name: &str) -> Result<Option<Vec<u8>>, KeptError> {
    use std::io::Read as _;
    let (file, meta) = match dir.open_regular(std::ffi::OsStr::new(name)) {
        Ok(opened) => opened,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) if e.kind() == io::ErrorKind::InvalidInput => return Err(not_regular()),
        Err(e) => return Err(io_error(&e)),
    };
    if meta.size() > MAX_RECORD_BYTES {
        return Err(KeptError::Refused(
            "a record of a command is too large".to_owned(),
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_RECORD_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|e| io_error(&e))?;
    Ok(Some(bytes))
}

impl FileKept {
    /// Outputs under `<workspace_root>/outputs`, scrubbed with `redactor` (clones of a redactor
    /// share what they know, so a secret added later is scrubbed too).
    pub fn new(workspace_root: &Path, redactor: Redactor) -> Self {
        Self {
            root: workspace_root.to_path_buf(),
            redactor,
            live: Arc::default(),
            run_max: RUN_KEPT_MAX,
        }
    }

    /// Keep at most `max` bytes per run, not [`RUN_KEPT_MAX`]: a deployment's choice, and how a test
    /// reaches the bound with a little text.
    #[must_use]
    pub fn with_run_max(mut self, max: u64) -> Self {
        self.run_max = max;
        self
    }

    /// The most bytes one run keeps.
    pub fn run_max(&self) -> u64 {
        self.run_max
    }

    /// Whether this process is writing the output `id` of `run` now.
    pub fn is_live(&self, run: &str, id: &str) -> bool {
        self.live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&(run.to_owned(), id.to_owned()))
    }

    /// Make the file of the output `id` of `run` (replacing the one an earlier try left: a start step
    /// that runs again begins its output again) for a writer that appends to it as the output
    /// arrives, and mark it live until [`end_live`](Self::end_live). `room` is what the run must have
    /// left of [`RUN_KEPT_MAX`] for it.
    ///
    /// # Errors
    ///
    /// [`KeptError::Refused`] when the run has less than `room` left or the place is not usable,
    /// [`KeptError::Io`].
    pub fn open_live(&self, run: &str, id: &str, room: u64) -> Result<std::fs::File, KeptError> {
        if !is_output_id(id) {
            return Err(KeptError::Malformed(id.to_owned()));
        }
        let dir = self.dir_of(run, true)?;
        let name = format!("{id}.txt");
        let used = used_by_others(&dir, &name)?;
        if used + room > self.run_max {
            return Err(KeptError::Refused(format!(
                "a run keeps at most {} of output, and this run holds {} already",
                size_text(self.run_max),
                size_text(used)
            )));
        }
        let name = std::ffi::OsString::from(name);
        match dir.unlink(&name) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_error(&e)),
        }
        let file = dir.create_new(&name).map_err(|e| io_error(&e))?;
        #[cfg(unix)]
        file.set_permissions(
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600),
        )
        .map_err(|e| io_error(&e))?;
        self.live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert((run.to_owned(), id.to_owned()));
        Ok(file)
    }

    /// The output `id` of `run` is not being written any more.
    pub fn end_live(&self, run: &str, id: &str) {
        self.live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&(run.to_owned(), id.to_owned()));
    }

    /// What the run's files take now, the output `id` not counted: for a writer that checks the
    /// run's room as it appends.
    ///
    /// # Errors
    ///
    /// [`KeptError::Io`] and the errors of finding the run's directory.
    pub fn run_used(&self, run: &str, id: &str) -> Result<u64, KeptError> {
        let dir = self.dir_of(run, false)?;
        used_by_others(&dir, &format!("{id}.txt"))
    }

    /// Write the record `<id>.cmd` of a command that keeps running: private (`0600`), written under a
    /// temporary name and renamed, no link followed.
    ///
    /// # Errors
    ///
    /// [`KeptError::Io`], and the errors of finding the run's directory.
    pub fn put_record(&self, run: &str, id: &str, bytes: &[u8]) -> Result<(), KeptError> {
        if !is_output_id(id) {
            return Err(KeptError::Malformed(id.to_owned()));
        }
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = self.dir_of(run, true)?;
        let tmp = std::ffi::OsString::from(format!(
            ".{id}.{}.{}.tmp",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let written = dir.create_new(&tmp).and_then(|mut file| {
            #[cfg(unix)]
            file.set_permissions(
                <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600),
            )?;
            file.write_all(bytes)?;
            file.flush()
        });
        if let Err(e) = written {
            let _ = dir.unlink(&tmp);
            return Err(io_error(&e));
        }
        if let Err(e) = dir.rename(&tmp, std::ffi::OsStr::new(&format!("{id}.cmd"))) {
            let _ = dir.unlink(&tmp);
            return Err(io_error(&e));
        }
        Ok(())
    }

    /// The record `<id>.cmd`, at most 64 KiB, or `None` when there is none.
    ///
    /// # Errors
    ///
    /// [`KeptError::Io`] and [`KeptError::Refused`] for a file that is not a regular one.
    pub fn get_record(&self, run: &str, id: &str) -> Result<Option<Vec<u8>>, KeptError> {
        let dir = match self.dir_of(run, false) {
            Ok(dir) => dir,
            Err(KeptError::NotFound(_)) => return Ok(None),
            Err(other) => return Err(other),
        };
        read_record(&dir, &format!("{id}.cmd"))
    }

    /// Every record of `run`: `(id, bytes)`, in the order of the ids.
    ///
    /// # Errors
    ///
    /// As [`get_record`](Self::get_record).
    pub fn records(&self, run: &str) -> Result<Vec<(String, Vec<u8>)>, KeptError> {
        let dir = match self.dir_of(run, false) {
            Ok(dir) => dir,
            Err(KeptError::NotFound(_)) => return Ok(Vec::new()),
            Err(other) => return Err(other),
        };
        let mut names: Vec<String> = dir
            .entries()
            .map_err(|e| io_error(&e))?
            .into_iter()
            .filter_map(|(name, meta)| {
                let name = name.into_string().ok()?;
                let id = name.strip_suffix(".cmd")?;
                (is_output_id(id) && meta.is_file()).then(|| id.to_owned())
            })
            .collect();
        names.sort();
        let mut out = Vec::new();
        for id in names {
            if let Some(bytes) = read_record(&dir, &format!("{id}.cmd"))? {
                out.push((id, bytes));
            }
        }
        Ok(out)
    }

    /// The directory that holds every run's outputs.
    pub fn root(&self) -> std::path::PathBuf {
        self.root.join("outputs")
    }

    /// The directory of `run`'s outputs (not made).
    pub fn run_dir(&self, run: &str) -> Option<std::path::PathBuf> {
        usable_run(run).then(|| self.root().join(run))
    }

    /// Remove every output of `run`. Idempotent. A link in the way is removed, not followed, and
    /// nothing is removed through one: the directories are opened one component at a time without
    /// following a link, and the entries are removed relative to the directory held. When `outputs`
    /// itself is not a directory of its own, nothing is removed and the log says so.
    ///
    /// # Errors
    ///
    /// I/O errors other than the directory not being there.
    pub async fn remove_run(&self, run: &str) -> io::Result<()> {
        if !usable_run(run) {
            return Ok(());
        }
        let (this, owned) = (self.clone(), run.to_owned());
        let removed = tokio::task::spawn_blocking(move || this.remove_run_blocking(&owned))
            .await
            .map_err(io::Error::other)?;
        match removed {
            Ok(Removal::Done) => {}
            Ok(Removal::Hijacked) => warn_hijacked(),
            Ok(Removal::TooDeep) => warn_once(
                run,
                "kept output is nested too deep to remove it all: what could be removed was",
            ),
            // Said once: a directory that cannot be removed would be said at every sweep.
            Err(e) => warn_once(
                run,
                &format!("cannot remove the kept output of a run ({})", e.kind()),
            ),
        }
        Ok(())
    }

    fn remove_run_blocking(&self, run: &str) -> io::Result<Removal> {
        let outputs = match self.outputs_dir()? {
            Place::Dir(outputs) => outputs,
            Place::Missing => return Ok(Removal::Done),
            Place::Hijacked => return Ok(Removal::Hijacked),
        };
        let name = std::ffi::OsStr::new(run);
        match outputs.lstat(name) {
            Ok(meta) if meta.is_dir() => {
                let dir = match outputs.open_dir(name) {
                    Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                        outputs.make_private(name)?;
                        outputs.open_dir(name)?
                    }
                    other => other?,
                };
                if !remove_contents(&dir, 0)? {
                    return Ok(Removal::TooDeep);
                }
                outputs.rmdir(name)?;
            }
            Ok(_) => outputs.unlink(name)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        Ok(Removal::Done)
    }

    /// The directory `outputs`, opened below the root without following a link.
    fn outputs_dir(&self) -> io::Result<Place> {
        let root = match SlotDir::open(&self.root) {
            Ok(root) => root,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Place::Missing),
            Err(e) => return Err(e),
        };
        match root.walk(Path::new("outputs"), false) {
            Ok(dir) => Ok(Place::Dir(dir)),
            Err(WalkError::Missing(_)) => Ok(Place::Missing),
            Err(WalkError::Link(_) | WalkError::NotDirectory(_)) => Ok(Place::Hijacked),
            Err(WalkError::Io(e)) => Err(e),
        }
    }

    /// The runs that have outputs on the volume.
    ///
    /// # Errors
    ///
    /// I/O errors listing the directory.
    pub async fn runs(&self) -> io::Result<Vec<String>> {
        let this = self.clone();
        let listed = tokio::task::spawn_blocking(move || -> io::Result<Option<Vec<String>>> {
            let outputs = match this.outputs_dir()? {
                Place::Dir(outputs) => outputs,
                Place::Missing => return Ok(Some(Vec::new())),
                Place::Hijacked => return Ok(None),
            };
            // A link is listed too: it is removed, not followed, with its run.
            let mut runs: Vec<String> = outputs
                .entries()?
                .into_iter()
                .filter_map(|(name, _)| name.into_string().ok())
                .filter(|name| usable_run(name))
                .collect();
            runs.sort();
            Ok(Some(runs))
        })
        .await
        .map_err(io::Error::other)??;
        Ok(listed.unwrap_or_else(|| {
            warn_hijacked();
            Vec::new()
        }))
    }

    /// The directory `outputs/<run>` below the root, one component at a time, no link followed.
    fn dir_of(&self, run: &str, create: bool) -> Result<Dir, KeptError> {
        if !usable_run(run) {
            return Err(KeptError::Refused(format!("unusable run id {run:?}")));
        }
        if create {
            std::fs::create_dir_all(&self.root).map_err(|e| io_error(&e))?;
        }
        let root = SlotDir::open(&self.root).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound if !create => KeptError::NotFound(String::new()),
            _ => io_error(&e),
        })?;
        if create {
            let outputs = root
                .walk(Path::new("outputs"), true)
                .map_err(|e| walk_error(&e))?;
            outputs.set_private().map_err(|e| io_error(&e))?;
        }
        let dir = root
            .walk(&Path::new("outputs").join(run), create)
            .map_err(|e| walk_error(&e))?;
        if create {
            dir.set_private().map_err(|e| io_error(&e))?;
        }
        Ok(dir)
    }

    fn keep_blocking(
        &self,
        run: &str,
        id: &str,
        text: &str,
        omitted: u64,
    ) -> Result<KeptInfo, KeptError> {
        #[cfg(test)]
        assert!(text != tests::PANIC_SEAM, "payload of a panic");
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        // Scrubbed first, then cut and folded: a secret is never split by either.
        let (scrubbed, omitted) = adam_llm_agent::scrubbed_for_keeping(
            text,
            omitted,
            |t| self.redactor.scrub(t).into_owned(),
            self.redactor.margin(),
        );
        let (prepared, cut) = prepare(&scrubbed, omitted);
        let _one_at_a_time = KEEP_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = self.dir_of(run, true)?;
        let name = format!("{id}.txt");
        // What the run holds already: the files this process names, not counting what this id held
        // (a call that runs again replaces its own file). Leftover temporary files are cleaned.
        let mut used = 0u64;
        for (entry, meta) in dir.entries().map_err(|e| io_error(&e))? {
            let entry_name = entry.to_string_lossy();
            if is_ours(&entry_name) && *entry_name != *name && meta.is_file() {
                used += meta.size();
            } else if is_tmp(&entry_name) && meta.is_file() {
                let now = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                let age = now.saturating_sub(u64::try_from(meta.mtime_secs()).unwrap_or(now));
                if age > STALE_TMP.as_secs() {
                    let _ = dir.unlink(&entry);
                }
            }
        }
        if used + prepared.len() as u64 > self.run_max {
            return Err(KeptError::Refused(format!(
                "a run keeps at most {} of output, and this run holds {} already",
                size_text(self.run_max),
                size_text(used)
            )));
        }
        let tmp = format!(
            ".{id}.{}.{}.tmp",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let tmp_name = std::ffi::OsString::from(&tmp);
        let written = dir.create_new(&tmp_name).and_then(|mut file| {
            #[cfg(unix)]
            file.set_permissions(
                <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600),
            )?;
            file.write_all(prepared.as_bytes())?;
            file.flush()
        });
        if let Err(e) = written {
            let _ = dir.unlink(&tmp_name);
            return Err(io_error(&e));
        }
        if let Err(e) = dir.rename(&tmp_name, std::ffi::OsStr::new(&name)) {
            let _ = dir.unlink(&tmp_name);
            return Err(io_error(&e));
        }
        Ok(KeptInfo {
            bytes: prepared.len() as u64,
            lines: count_lines(&prepared),
            cut,
        })
    }

    fn page_blocking(
        &self,
        run: &str,
        id: &str,
        offset: i64,
        limit: u32,
    ) -> Result<String, KeptError> {
        if !is_output_id(id) {
            return Err(KeptError::Malformed(id.to_owned()));
        }
        let missing = || KeptError::NotFound(id.to_owned());
        let dir = match self.dir_of(run, false) {
            Ok(dir) => dir,
            Err(KeptError::NotFound(_)) => return Err(missing()),
            Err(other) => return Err(other),
        };
        let name = format!("{id}.txt");
        let (file, _) = match dir.open_regular(std::ffi::OsStr::new(&name)) {
            Ok(opened) => opened,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(missing()),
            Err(e) if e.kind() == io::ErrorKind::InvalidInput => return Err(not_regular()),
            Err(e) => return Err(io_error(&e)),
        };
        let meta = file.metadata().map_err(|e| io_error(&e))?;
        if !meta.is_file() {
            return Err(not_regular());
        }
        // A hard link planted by a command: not ours.
        #[cfg(unix)]
        if std::os::unix::fs::MetadataExt::nlink(&meta) != 1 {
            return Err(KeptError::Refused(
                "that output file is not usable (it has been linked)".to_owned(),
            ));
        }
        let live = self.is_live(run, id);
        // A file that is still written may end in half a line: only whole lines are shown and counted.
        let reader: Box<dyn io::Read> = if live {
            let whole = complete_len(&file, meta.len()).map_err(|e| io_error(&e))?;
            Box::new(io::Read::take(file, whole))
        } else {
            Box::new(file)
        };
        match page_lines(BufReader::new(reader), offset, limit, false) {
            Ok(page) if live => Ok(page.render_live()),
            Ok(page) => Ok(page.render()),
            // A command that is still running: the line after the last one is where the next page
            // starts, and nothing is there yet.
            Err(PageError::PastEnd { offset, total })
                if live && i64::try_from(total).is_ok_and(|t| offset == t + 1) =>
            {
                Ok(adam_llm_agent::paging::running_line(total))
            }
            Err(PageError::PastEnd { offset, total }) => Err(KeptError::Window(format!(
                "output `{id}` has {total} lines: offset {offset} is past the end"
            ))),
            Err(PageError::Io(e)) => Err(io_error(&e)),
            Err(other) => Err(KeptError::Window(other.to_string())),
        }
    }
}

#[async_trait]
impl KeptOutputs for FileKept {
    async fn keep(
        &self,
        run: &str,
        id: &str,
        text: &str,
        omitted: u64,
    ) -> Result<KeptInfo, KeptError> {
        if !is_output_id(id) {
            return Err(KeptError::Malformed(id.to_owned()));
        }
        let (this, run, id, text) = (self.clone(), run.to_owned(), id.to_owned(), text.to_owned());
        tokio::task::spawn_blocking(move || this.keep_blocking(&run, &id, &text, omitted))
            .await
            .map_err(|_| join_failed())?
    }

    async fn page(
        &self,
        run: &str,
        id: &str,
        offset: i64,
        limit: u32,
    ) -> Result<String, KeptError> {
        let (this, run, id) = (self.clone(), run.to_owned(), id.to_owned());
        tokio::task::spawn_blocking(move || this.page_blocking(&run, &id, offset, limit))
            .await
            .map_err(|_| join_failed())?
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;

    const RUN: &str = "00000000-0000-4000-8000-000000000001";

    fn id(call: &str) -> String {
        adam_llm_agent::output_id(RUN, call)
    }

    async fn keep(
        store: &FileKept,
        run: &str,
        id: &str,
        text: &str,
    ) -> Result<KeptInfo, KeptError> {
        store.keep(run, id, text, 0).await
    }

    /// A file that is still written may end in half a line: the page shows and counts whole lines,
    /// and the half line appears when it is finished.
    #[tokio::test]
    async fn a_live_page_never_shows_or_counts_bytes_after_the_last_newline() {
        use std::io::Write as _;
        let root = tempfile::tempdir().unwrap();
        let store = FileKept::new(root.path(), Redactor::default());
        let id = id("live");
        let mut file = store.open_live(RUN, &id, 1024).unwrap();
        file.write_all(b"one\ntwo\nthr").unwrap();
        let page = store.page(RUN, &id, 1, 100).await.unwrap();
        assert_eq!(
            page,
            "one\ntwo\n[running: 2 lines so far; next page: offset=3]"
        );
        // Nothing but half a line yet: nothing is there.
        let mut second = store
            .open_live(RUN, &adam_llm_agent::output_id(RUN, "b"), 1024)
            .unwrap();
        second.write_all(b"half").unwrap();
        let none = store
            .page(RUN, &adam_llm_agent::output_id(RUN, "b"), 1, 100)
            .await;
        assert!(none.as_ref().is_ok_and(|p| !p.contains("half")), "{none:?}");
        file.write_all(b"ee\n").unwrap();
        let page = store.page(RUN, &id, 1, 100).await.unwrap();
        assert_eq!(
            page,
            "one\ntwo\nthree\n[running: 3 lines so far; next page: offset=4]"
        );
        store.end_live(RUN, &id);
    }

    #[tokio::test]
    async fn a_run_keeps_at_most_what_it_is_given() {
        let root = tempfile::tempdir().unwrap();
        let store = FileKept::new(root.path(), Redactor::default()).with_run_max(4096);
        let one = id("one");
        let refused = store.open_live(RUN, &one, 8192);
        assert!(
            matches!(&refused, Err(KeptError::Refused(why)) if why.contains("at most 4 KiB")),
            "{refused:?}"
        );
        assert!(store.open_live(RUN, &one, 1024).is_ok());
        let big = "x".repeat(5000);
        let kept = store.keep(RUN, &id("two"), &big, 0).await;
        assert!(matches!(kept, Err(KeptError::Refused(_))), "{kept:?}");
        store.end_live(RUN, &one);
    }

    #[tokio::test]
    async fn the_file_keeper_passes_the_conformance() {
        let root = tempfile::tempdir().unwrap();
        let store = FileKept::new(root.path(), Redactor::default());
        adam_llm_agent::kept_outputs_conformance(&store).await;
    }

    #[tokio::test]
    async fn a_secret_is_nowhere_on_the_volume_and_a_page_boundary_cannot_split_it() {
        let root = tempfile::tempdir().unwrap();
        let secret = "sk-live-0123456789abcdef";
        let store = FileKept::new(root.path(), Redactor::new([secret]));
        let id = id("c1");
        let mut text: String = (1..2000).map(|i| format!("l{i}\n")).collect();
        text.push_str(&format!("key={secret}\n"));
        text.push_str("after\n");
        keep(&store, RUN, &id, &text).await.unwrap();
        let on_disk = std::fs::read_to_string(
            root.path()
                .join("outputs")
                .join(RUN)
                .join(format!("{id}.txt")),
        )
        .unwrap();
        assert!(
            !on_disk.contains(secret)
                && on_disk.contains("key=[redacted: secret removed, do not reuse]")
        );
        for offset in [1, 1999, 2000, 2001] {
            let page = store.page(RUN, &id, offset, 2000).await.unwrap();
            assert!(!page.contains(secret), "{page}");
        }
        for entry in walk(root.path()) {
            let bytes = std::fs::read(&entry).unwrap();
            assert!(
                !String::from_utf8_lossy(&bytes).contains(secret),
                "{}",
                entry.display()
            );
        }
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk(&path));
            } else {
                out.push(path);
            }
        }
        out
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_file_is_0600_in_a_0700_directory_outside_any_worktree() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let store = FileKept::new(root.path(), Redactor::default());
        let id = id("c1");
        keep(&store, RUN, &id, "hello\n").await.unwrap();
        let outputs = root.path().join("outputs");
        let dir = outputs.join(RUN);
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&outputs), 0o700);
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join(format!("{id}.txt"))), 0o600);
        assert!(!dir.starts_with(root.path().join("workspaces")));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_planted_link_is_never_followed_by_a_write_or_a_read() {
        let root = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let store = FileKept::new(root.path(), Redactor::default());
        let id = id("c1");

        // `outputs` is a link to somewhere else.
        std::os::unix::fs::symlink(elsewhere.path(), root.path().join("outputs")).unwrap();
        let refused = keep(&store, RUN, &id, "x\n").await.unwrap_err();
        assert!(matches!(refused, KeptError::Refused(_)), "{refused:?}");
        assert!(!refused.to_string().contains(root.path().to_str().unwrap()));
        assert_eq!(std::fs::read_dir(elsewhere.path()).unwrap().count(), 0);
        std::fs::remove_file(root.path().join("outputs")).unwrap();

        // `outputs/<run>` is a link to somewhere else.
        std::fs::create_dir(root.path().join("outputs")).unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), root.path().join("outputs").join(RUN))
            .unwrap();
        let refused = keep(&store, RUN, &id, "x\n").await.unwrap_err();
        assert!(matches!(refused, KeptError::Refused(_)), "{refused:?}");
        assert_eq!(std::fs::read_dir(elsewhere.path()).unwrap().count(), 0);
        assert!(store.page(RUN, &id, 1, 10).await.is_err());
        std::fs::remove_file(root.path().join("outputs").join(RUN)).unwrap();

        // The output file itself is a link; and a hard link to a file of another name.
        keep(&store, RUN, &id, "x\n").await.unwrap();
        let file = root
            .path()
            .join("outputs")
            .join(RUN)
            .join(format!("{id}.txt"));
        std::fs::remove_file(&file).unwrap();
        let target = elsewhere.path().join("target");
        std::fs::write(&target, "secret elsewhere\n").unwrap();
        std::os::unix::fs::symlink(&target, &file).unwrap();
        assert!(store.page(RUN, &id, 1, 10).await.is_err());
        std::fs::remove_file(&file).unwrap();
        std::fs::hard_link(&target, &file).unwrap();
        let refused = store.page(RUN, &id, 1, 10).await.unwrap_err();
        assert!(refused.to_string().contains("linked"), "{refused}");
    }

    #[tokio::test]
    async fn a_run_keeps_at_most_its_bound_and_an_output_is_cut_at_its_own() {
        let root = tempfile::tempdir().unwrap();
        let store = FileKept::new(root.path(), Redactor::default());
        let big = "x".repeat(99) + "\n";
        let text = big.repeat(adam_llm_agent::MAX_KEPT_BYTES / big.len() + 5);
        let info = keep(&store, RUN, &id("a"), &text).await.unwrap();
        assert!(info.cut);
        for call in ["b", "c"] {
            keep(&store, RUN, &id(call), &text).await.unwrap();
        }
        let refused = keep(&store, RUN, &id("d"), &text).await;
        assert!(
            matches!(&refused, Err(KeptError::Refused(why)) if why.contains("at most 64 MiB")),
            "{refused:?}"
        );
        // Keeping an id again replaces it: the same call is not counted twice.
        keep(&store, RUN, &id("a"), &text).await.unwrap();
    }

    /// A file a command writes into the run's directory (of any name, however big) does not use
    /// the run's budget, and a temporary file left behind is not counted.
    #[tokio::test]
    async fn a_foreign_file_does_not_use_the_runs_budget() {
        let root = tempfile::tempdir().unwrap();
        let store = FileKept::new(root.path(), Redactor::default());
        keep(&store, RUN, &id("a"), "x\n").await.unwrap();
        let dir = root.path().join("outputs").join(RUN);
        for name in ["big.bin", ".o-0123456789abcdef.1.1.tmp"] {
            let f = std::fs::File::create(dir.join(name)).unwrap();
            f.set_len(RUN_KEPT_MAX + 1).unwrap(); // sparse
        }
        keep(&store, RUN, &id("b"), "y\n").await.unwrap();
        assert_eq!(
            store.page(RUN, &id("b"), 1, 10).await.unwrap(),
            "y\n[end: 1 lines]"
        );
    }

    /// The file appears whole or not at all: a reader that polls while a big output is written never
    /// sees a part of it.
    #[tokio::test]
    async fn a_reader_never_sees_a_partial_file() {
        let root = tempfile::tempdir().unwrap();
        let store = FileKept::new(root.path(), Redactor::default());
        let text = "0123456789abcdef\n".repeat(900_000);
        let path = root
            .path()
            .join("outputs")
            .join(RUN)
            .join(format!("{}.txt", id("big")));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = {
            let (path, stop) = (path.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut lengths = std::collections::BTreeSet::new();
                while !stop.load(Ordering::Relaxed) {
                    if let Ok(meta) = std::fs::metadata(&path) {
                        lengths.insert(meta.len());
                    }
                }
                lengths
            })
        };
        keep(&store, RUN, &id("big"), &text).await.unwrap();
        stop.store(true, Ordering::Relaxed);
        let lengths = seen.join().unwrap();
        assert!(
            lengths.iter().all(|&l| l == text.len() as u64),
            "a reader saw a partial file: {lengths:?}"
        );
    }

    #[tokio::test]
    async fn another_worker_on_the_same_volume_reads_what_one_kept() {
        let root = tempfile::tempdir().unwrap();
        let id = id("c1");
        keep(
            &FileKept::new(root.path(), Redactor::default()),
            RUN,
            &id,
            "one\ntwo\n",
        )
        .await
        .unwrap();
        let worker_b = FileKept::new(root.path(), Redactor::default());
        assert_eq!(
            worker_b.page(RUN, &id, 1, 10).await.unwrap(),
            "one\ntwo\n[end: 2 lines]"
        );
    }

    #[tokio::test]
    async fn an_id_of_another_run_is_a_miss_that_says_what_to_do() {
        let root = tempfile::tempdir().unwrap();
        let store = FileKept::new(root.path(), Redactor::default());
        let other = "00000000-0000-4000-8000-000000000002";
        let id = adam_llm_agent::output_id(other, "c1");
        keep(&store, other, &id, "private\n").await.unwrap();
        let refused = store.page(RUN, &id, 1, 10).await.unwrap_err();
        assert!(matches!(refused, KeptError::NotFound(_)), "{refused}");
        assert!(refused.to_string().contains("earlier task"), "{refused}");
        assert!(
            refused.to_string().contains("Run the command again"),
            "{refused}"
        );
        let malformed = store
            .page(RUN, "../../etc/passwd", 1, 10)
            .await
            .unwrap_err();
        assert!(matches!(malformed, KeptError::Malformed(_)));
    }

    /// A text that makes `keep_blocking` panic (a seam for the test of a join failure).
    pub(super) const PANIC_SEAM: &str = "\0\0panic-seam";
    const SECRET: &str = "ghp_0123456789abcdefghijklmnopqrstuvwxyz"; // 40 bytes: 30 more than its mark
    const CAPTURE: usize = adam_llm_agent::MAX_KEPT_BYTES + adam_llm_agent::SCRUB_OVERLAP;

    /// No 8-byte window of the secret is in `text`.
    fn assert_no_part(text: &str, what: &str) {
        for w in SECRET.as_bytes().windows(8) {
            let w = std::str::from_utf8(w).unwrap();
            assert!(!text.contains(w), "{what}: `{w}` is left");
        }
    }

    fn file_of(root: &Path, id: &str) -> PathBuf {
        root.join("outputs").join(RUN).join(format!("{id}.txt"))
    }

    /// Nothing of the secret in the file, nor on any page of it.
    async fn assert_clean(store: &FileKept, root: &Path, id: &str, what: &str) {
        let kept = std::fs::read_to_string(file_of(root, id)).unwrap();
        assert_no_part(&kept, what);
        // Something is kept (the secrets that are whole are replaced, and the filler is there). A
        // fold puts a line break wherever the mark falls, so the breaks are not part of the mark.
        assert!(
            kept.replace('\n', "")
                .contains("[redacted: secret removed, do not reuse]"),
            "{what}: the secret is not replaced"
        );
        let mut offset = 1;
        loop {
            let page = store.page(RUN, id, offset, 2000).await.unwrap();
            assert_no_part(&page, what);
            let last = page.rsplit_once('\n').map_or(page.as_str(), |(_, l)| l);
            match last.split("offset=").nth(1) {
                Some(next) => offset = next.trim_end_matches(']').parse().unwrap(),
                None => return,
            }
        }
    }

    /// The file, and the first and the last page (a page reads the file from its start: not all of
    /// a file of many thousand lines).
    async fn assert_clean_ends(store: &FileKept, root: &Path, id: &str, what: &str) {
        let kept = std::fs::read_to_string(file_of(root, id)).unwrap();
        assert_no_part(&kept, what);
        assert!(
            kept.contains("[redacted: secret removed, do not reuse]")
                && kept.len() > 8 * 1024 * 1024,
            "{what}: little is kept"
        );
        for offset in [1, -30] {
            assert_no_part(&store.page(RUN, id, offset, 2000).await.unwrap(), what);
        }
    }

    /// 3,000 secrets in the first lines shrink the scrubbed text under the bound, so nothing is cut
    /// at 16 MiB of it, and the first 20 characters of the secret end the capture: the first half
    /// that scrubbing cannot recognise must not be kept.
    #[tokio::test]
    async fn the_prefix_of_a_secret_at_the_end_of_the_capture_is_not_kept_when_scrubbing_shrinks_the_text()
     {
        let root = tempfile::tempdir().unwrap();
        let store = FileKept::new(root.path(), Redactor::new([SECRET]));
        let id = id("c1");
        let mut text = format!("{SECRET}\n").repeat(3000);
        text.push_str(&"a".repeat(CAPTURE - 20 - text.len()));
        text.push_str(&SECRET[..20]);
        assert_eq!(text.len(), CAPTURE);
        let info = store.keep(RUN, &id, &text, 1_000_000).await.unwrap();
        assert!(info.cut);
        assert_clean(&store, root.path(), &id, "the capture's end").await;
        let kept = std::fs::metadata(file_of(root.path(), &id)).unwrap().len();
        assert!(kept > 15 * 1024 * 1024, "the filler is kept: {kept}");
    }

    /// The same where the scrubbed text grows (a 4-character secret becomes 10), and where the
    /// secret straddles the 16 MiB mark or the capture's end, with and without newlines about.
    #[tokio::test]
    async fn no_cut_leaves_a_part_of_a_secret_whatever_the_scrubbing_does_to_the_length() {
        let root = tempfile::tempdir().unwrap();
        let store = FileKept::new(root.path(), Redactor::new([SECRET, "wxyz"]));
        let id = id("c1");
        let end = CAPTURE - 20;
        let cut = adam_llm_agent::MAX_KEPT_BYTES - 10;
        // (the scrubbed text grows, newlines about, where the secret starts)
        for (grow, lines, at) in [
            (false, false, cut),
            (false, true, end),
            (true, false, end),
            (true, true, cut),
        ] {
            let mut text = if grow {
                "wxyz".repeat(900_000)
            } else {
                format!("{SECRET}\n").repeat(3000)
            };
            let unit = if lines {
                "ab cd ".repeat(33) + "\n"
            } else {
                "abcdef".to_owned()
            };
            let room = at - text.len();
            text.push_str(&unit.repeat(room / unit.len() + 1)[..room]);
            text.push_str(&SECRET[..(CAPTURE - at).min(SECRET.len())]);
            text.push_str(&"b".repeat(CAPTURE.saturating_sub(text.len())));
            store.keep(RUN, &id, &text, 5).await.unwrap();
            assert_clean_ends(&store, root.path(), &id, &format!("{grow} {lines} {at}")).await;
        }
    }

    /// A line over the fold's column holds a secret across it: the keeper scrubs before it folds,
    /// even when nothing before it scrubbed.
    #[tokio::test]
    async fn the_keeper_scrubs_before_it_folds_on_its_own() {
        let root = tempfile::tempdir().unwrap();
        let store = FileKept::new(root.path(), Redactor::new([SECRET]));
        let id = id("c1");
        let text = format!("{}{SECRET}{}\n", "a".repeat(1990), "b".repeat(3000));
        store.keep(RUN, &id, &text, 0).await.unwrap();
        assert_clean(&store, root.path(), &id, "the fold").await;
    }

    #[tokio::test]
    async fn an_id_that_is_not_an_id_is_malformed_for_a_keep_and_a_read() {
        let root = tempfile::tempdir().unwrap();
        let store = FileKept::new(root.path(), Redactor::default());
        for bad in [
            "o-../../../etc/pa",
            "o-../../../etc/pas",
            "o-./aaaaaaaaaaaaaa",
            "o-\0aaaaaaaaaaaaaaa",
        ] {
            assert!(matches!(
                store.keep(RUN, bad, "x\n", 0).await,
                Err(KeptError::Malformed(_))
            ));
            assert!(matches!(
                store.page(RUN, bad, 1, 10).await,
                Err(KeptError::Malformed(_))
            ));
        }
        assert!(!root.path().join("outputs").exists());
    }

    /// A command plants a hard link to a file of its own at every temporary name the next keep could
    /// use: the keep refuses (the name is taken), and writes through none of them.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_planted_file_at_the_temporary_name_is_refused_and_never_written() {
        let root = tempfile::tempdir().unwrap();
        let store = FileKept::new(root.path(), Redactor::default());
        let id = id("c1");
        keep(&store, RUN, &id, "first\n").await.unwrap();
        let victim = root.path().join("victim");
        std::fs::write(&victim, "precious\n").unwrap();
        let dir = root.path().join("outputs").join(RUN);
        let pid = std::process::id();
        for n in 0..5000 {
            std::fs::hard_link(&victim, dir.join(format!(".{id}.{pid}.{n}.tmp"))).unwrap();
        }
        let refused = keep(&store, RUN, &id, "second\n").await;
        assert!(refused.is_err(), "{refused:?}");
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious\n");
        assert_eq!(
            store.page(RUN, &id, 1, 10).await.unwrap(),
            "first\n[end: 1 lines]"
        );
    }

    /// `outputs` is swapped for a link between a keep and a read: the read refuses.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_link_planted_for_outputs_between_a_keep_and_a_read_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let store = FileKept::new(root.path(), Redactor::default());
        let id = id("c1");
        keep(&store, RUN, &id, "kept\n").await.unwrap();
        let real = root.path().join("elsewhere");
        std::fs::rename(root.path().join("outputs"), &real).unwrap();
        std::os::unix::fs::symlink(&real, root.path().join("outputs")).unwrap();
        let refused = store.page(RUN, &id, 1, 10).await.unwrap_err();
        assert!(matches!(refused, KeptError::Refused(_)), "{refused:?}");
    }

    /// A FIFO or a directory where the file should be is refused, and the read returns.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_pipe_or_a_directory_at_the_files_name_is_refused_and_does_not_block() {
        let root = tempfile::tempdir().unwrap();
        let store = FileKept::new(root.path(), Redactor::default());
        keep(&store, RUN, &id("a"), "x\n").await.unwrap();
        let (fifo, dir) = (id("fifo"), id("dir"));
        let made = std::process::Command::new("mkfifo")
            .arg(file_of(root.path(), &fifo))
            .status()
            .unwrap();
        assert!(made.success());
        std::fs::create_dir(file_of(root.path(), &dir)).unwrap();
        for bad in [fifo, dir] {
            let read = tokio::time::timeout(Duration::from_secs(10), store.page(RUN, &bad, 1, 10))
                .await
                .expect("the read returns");
            assert!(matches!(read, Err(KeptError::Refused(_))), "{read:?}");
        }
    }

    /// Many keeps at once near the run's bound: the check and the write are one step, so what is
    /// kept never passes the bound.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_keeps_never_pass_the_runs_bound() {
        let root = tempfile::tempdir().unwrap();
        let store = FileKept::new(root.path(), Redactor::default());
        let text = Arc::new(("x".repeat(99) + "\n").repeat(95_000)); // about 9 MiB
        let start = Arc::new(tokio::sync::Barrier::new(12));
        let mut tasks = Vec::new();
        for n in 0..12 {
            let (store, text, start) = (store.clone(), text.clone(), start.clone());
            tasks.push(tokio::spawn(async move {
                start.wait().await;
                store.keep(RUN, &id(&format!("call-{n}")), &text, 0).await
            }));
        }
        let mut kept = 0;
        for task in tasks {
            kept += usize::from(task.await.unwrap().is_ok());
        }
        let total: u64 = std::fs::read_dir(root.path().join("outputs").join(RUN))
            .unwrap()
            .flatten()
            .map(|e| e.metadata().unwrap().len())
            .sum();
        assert!(total <= RUN_KEPT_MAX, "{total} bytes kept by {kept}");
        assert!((1..12).contains(&kept), "{kept}");
    }

    /// The check of the bound and the write are one step: a keep waits while another holds the lock.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // holding it across the wait is the test
    async fn a_keep_waits_for_the_lock_that_makes_the_check_and_the_write_one_step() {
        let root = tempfile::tempdir().unwrap();
        let store = FileKept::new(root.path(), Redactor::default());
        let held = KEEP_LOCK.lock().unwrap();
        let task = {
            let store = store.clone();
            tokio::spawn(async move { store.keep(RUN, &id("a"), "x\n", 0).await })
        };
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(!task.is_finished(), "the keep did not wait for the lock");
        drop(held);
        task.await.unwrap().unwrap();
    }

    /// A temporary file a crash left is removed by the next keep when it is old, and left when it is
    /// fresh (a keep in another worker may be writing it).
    #[tokio::test]
    async fn an_old_temporary_file_is_removed_by_a_keep_and_a_fresh_one_is_left() {
        let root = tempfile::tempdir().unwrap();
        let store = FileKept::new(root.path(), Redactor::default());
        keep(&store, RUN, &id("a"), "x\n").await.unwrap();
        let dir = root.path().join("outputs").join(RUN);
        let (old, fresh) = (
            dir.join(".o-0123456789abcdef.1.1.tmp"),
            dir.join(".o-0123456789abcdee.1.2.tmp"),
        );
        for path in [&old, &fresh] {
            std::fs::write(path, "left\n").unwrap();
        }
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(SystemTime::now() - STALE_TMP - Duration::from_secs(60))
            .unwrap();
        keep(&store, RUN, &id("b"), "y\n").await.unwrap();
        assert!(!old.exists(), "an old temporary file stays");
        assert!(fresh.exists(), "a fresh one is removed");
    }

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
        type Writer = Sink;
        fn make_writer(&'a self) -> Sink {
            self.clone()
        }
    }

    fn capture_logs() -> (Sink, tracing::subscriber::DefaultGuard) {
        let sink = Sink::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(sink.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .finish();
        (sink, tracing::subscriber::set_default(subscriber))
    }

    fn logged(sink: &Sink) -> String {
        String::from_utf8_lossy(&sink.0.lock().unwrap()).into_owned()
    }

    /// A secret of 150 KiB, longer than any margin that is only assumed: it starts 100 KiB before the
    /// end of the capture and goes beyond it. The margin is the longest value the redactor holds, so
    /// none of it is kept (and a whole one earlier is replaced, and the filler is kept).
    #[tokio::test]
    async fn a_secret_longer_than_64_kib_is_not_kept_in_part_at_the_end_of_the_capture() {
        let root = tempfile::tempdir().unwrap();
        let big = format!("BiG-{}", "0123456789ABCDEF".repeat(9400)); // 150,404 bytes
        let store = FileKept::new(root.path(), Redactor::new([big.as_str()]));
        let id = id("c1");
        let len = adam_llm_agent::MAX_KEPT_BYTES + 250_000;
        let mut text = format!("early {big} done\n");
        let filler = "xyxzyzx".repeat(len / 7 + 1);
        text.push_str(&filler[..len - 100_000 - text.len()]);
        text.push_str(&big[..100_000]);
        assert_eq!(text.len(), len);
        let info = store.keep(RUN, &id, &text, 1_000_000).await.unwrap();
        assert!(info.cut);
        let kept = std::fs::read_to_string(file_of(root.path(), &id)).unwrap();
        assert!(
            !kept.contains("BiG-01234567"),
            "the start of the secret is kept"
        );
        assert!(
            kept.starts_with("early [redacted: secret removed, do not reuse] done\nxyxz"),
            "{}",
            &kept[..60]
        );
        assert!(
            kept.len() > 15 * 1024 * 1024,
            "the filler is kept: {}",
            kept.len()
        );
        let page = store.page(RUN, &id, -5, 10).await.unwrap();
        assert!(
            !page.contains("BiG-0123") && page.contains("xyxz"),
            "{page}"
        );
    }

    /// A blocking task that panics is a fixed text for the model: the text of a panic can hold anything.
    #[tokio::test]
    async fn a_keep_that_panics_says_nothing_of_the_panic() {
        let root = tempfile::tempdir().unwrap();
        let store = FileKept::new(root.path(), Redactor::default());
        let err = store
            .keep(RUN, &id("c1"), PANIC_SEAM, 0)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            !err.contains("payload") && !err.contains("panicked"),
            "{err}"
        );
    }

    /// A run directory nested deeper than the removal goes: what can be removed is, the rest is said
    /// once for the run in this process, and the sweep that comes again says nothing more.
    #[tokio::test]
    async fn a_run_nested_too_deep_is_removed_as_far_as_it_goes_and_said_once() {
        let (sink, _guard) = capture_logs();
        let root = tempfile::tempdir().unwrap();
        let store = FileKept::new(root.path(), Redactor::default());
        let run = "00000000-0000-4000-8000-0000000000aa";
        keep(&store, run, &adam_llm_agent::output_id(run, "c"), "x\n")
            .await
            .unwrap();
        let mut deep = root.path().join("outputs").join(run);
        std::fs::write(deep.join("top.txt"), "top\n").unwrap();
        for _ in 0..40 {
            deep.push("d");
        }
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("bottom.txt"), "bottom\n").unwrap();
        store.remove_run(run).await.unwrap();
        let first = logged(&sink);
        assert!(first.contains("nested too deep"), "{first}");
        assert!(
            !root
                .path()
                .join("outputs")
                .join(run)
                .join("top.txt")
                .exists()
        );
        store.remove_run(run).await.unwrap();
        assert_eq!(logged(&sink), first, "said again at the next sweep");
    }

    /// A directory a command made unreadable is made private through the handle that holds it and
    /// removed (as any user but root can tell: root reads it anyway).
    #[cfg(unix)]
    #[tokio::test]
    async fn a_directory_made_unreadable_is_removed_all_the_same() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let store = FileKept::new(root.path(), Redactor::default());
        keep(&store, RUN, &id("a"), "x\n").await.unwrap();
        let dir = root.path().join("outputs").join(RUN).join("locked");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f"), "f\n").unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        store.remove_run(RUN).await.unwrap();
        assert!(!root.path().join("outputs").join(RUN).exists());
    }

    /// `outputs` is a link to the workspaces: the janitor lists and removes nothing there, and says
    /// so without a path.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_janitor_does_not_follow_a_link_for_outputs() {
        let (sink, _guard) = capture_logs();
        let root = tempfile::tempdir().unwrap();
        let victim = root.path().join("workspaces").join(RUN);
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::write(victim.join("work.txt"), "a scratch project\n").unwrap();
        std::os::unix::fs::symlink(root.path().join("workspaces"), root.path().join("outputs"))
            .unwrap();
        let store = FileKept::new(root.path(), Redactor::default());
        assert!(store.runs().await.unwrap().is_empty());
        store.remove_run(RUN).await.unwrap();
        assert!(
            victim.join("work.txt").exists(),
            "the workspace was removed"
        );
        let log = logged(&sink);
        assert!(log.contains("not removed"), "{log}");
        assert!(!log.contains(root.path().to_str().unwrap()), "{log}");
    }

    /// `outputs/<run>` is a link, and a run's directory holds a link: each is unlinked, and what it
    /// leads to is untouched.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_janitor_unlinks_a_link_and_never_follows_it() {
        let root = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::write(elsewhere.path().join("keep.txt"), "mine\n").unwrap();
        let store = FileKept::new(root.path(), Redactor::default());
        let other = "00000000-0000-4000-8000-000000000002";
        std::fs::create_dir_all(root.path().join("outputs")).unwrap();

        // `outputs/<run>` is a link.
        std::os::unix::fs::symlink(elsewhere.path(), root.path().join("outputs").join(RUN))
            .unwrap();
        assert_eq!(store.runs().await.unwrap(), vec![RUN.to_owned()]);
        store.remove_run(RUN).await.unwrap();
        assert!(!root.path().join("outputs").join(RUN).exists());
        assert!(elsewhere.path().join("keep.txt").exists());

        // A link inside a run's directory, and a directory inside it.
        keep(&store, other, &adam_llm_agent::output_id(other, "c"), "x\n")
            .await
            .unwrap();
        let dir = root.path().join("outputs").join(other);
        std::os::unix::fs::symlink(elsewhere.path(), dir.join("link")).unwrap();
        std::fs::create_dir_all(dir.join("sub").join("deeper")).unwrap();
        std::fs::write(dir.join("sub").join("deeper").join("f"), "f\n").unwrap();
        store.remove_run(other).await.unwrap();
        assert!(!dir.exists());
        assert!(elsewhere.path().join("keep.txt").exists());
    }

    #[tokio::test]
    async fn remove_run_removes_only_that_run() {
        let root = tempfile::tempdir().unwrap();
        let store = FileKept::new(root.path(), Redactor::default());
        let other = "00000000-0000-4000-8000-000000000002";
        for run in [RUN, other] {
            keep(&store, run, &adam_llm_agent::output_id(run, "c"), "x\n")
                .await
                .unwrap();
        }
        assert_eq!(
            store.runs().await.unwrap(),
            vec![RUN.to_owned(), other.to_owned()]
        );
        store.remove_run(RUN).await.unwrap();
        store.remove_run(RUN).await.unwrap();
        assert_eq!(store.runs().await.unwrap(), vec![other.to_owned()]);
    }
}
