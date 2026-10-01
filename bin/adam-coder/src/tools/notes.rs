//! What the coder remembers about a run besides the conversation: how many
//! check cycles were spent, what the checks said (the last one, and a short history of them), what
//! was pushed and which pull request was opened.
//!
//! The rules "at most N check cycles" and "no pull request on red checks" are
//! enforced by the tools, so their inputs must survive a restart and be safe
//! against a tool call that runs twice (a crash between the side effect and its
//! journal entry). The notes live in a small JSON file per run next to the
//! worktree (`<workspace root>/coder/<run>.json`), on the same persistent
//! volume as the worktree they describe. Writes are atomic (temp file +
//! rename). Counting is keyed by tool call id, so replaying a call never counts
//! a failure twice.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use super::checks::ChecksReport;

/// The outcome of one `run_checks` call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckRecord {
    /// The tool call that ran it.
    pub call_id: String,
    /// The command.
    pub command: String,
    /// Exit code 0 and no timeout.
    pub passed: bool,
    /// Exit code, `None` on timeout or signal.
    pub exit_code: Option<i32>,
    /// The output tail the model saw (the "findings" of a failed run).
    pub tail: String,
    /// The tree id of the code the command ran on (see
    /// `gitcli::working_tree_id`); `None` if it could not be determined.
    #[serde(default)]
    pub tree: Option<String>,
    /// What the run reported as its `checks` artifact, so `commit_and_push` can bind it to the
    /// commit it makes when the tree is the same.
    #[serde(default)]
    pub report: Option<ChecksReport>,
    /// The slot of the workspace the command ran in (its directory name). Absent in notes written
    /// before a workspace had several.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
}

/// Most check runs [`ChecksNotes::history`] keeps.
pub const MAX_CHECK_HISTORY: usize = 32;

/// State of the check/fix cycle.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChecksNotes {
    /// Failed `run_checks` calls so far.
    pub failures: u32,
    /// Call ids already counted in `failures`.
    pub counted: Vec<String>,
    /// The most recent run, in any slot.
    pub last: Option<CheckRecord>,
    /// The last [`MAX_CHECK_HISTORY`] runs, oldest first, in every slot of the workspace: what binds
    /// a pushed commit to the check that ran on its tree, whichever slot ran it (see
    /// [`RunNotes::checked`]). `last` is its newest; it stays beside it for notes written before
    /// the history existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<CheckRecord>,
}

/// The pull request the run opened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestNote {
    /// Browser URL.
    pub url: String,
    /// Number in the repository.
    pub number: u64,
    /// The checks were red and the user accepted that.
    pub red_checks_accepted: bool,
    /// The pushed commit for which the comment that says so was posted on an already open pull
    /// request: a repeated call does not post it again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commented_sha: Option<String>,
}

/// A call that found a tool missing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissingHit {
    /// The tool call.
    pub call_id: String,
    /// The command word the shell could not find.
    pub tool: String,
}

impl RunNotes {
    /// Record that call `call_id` found `tool` missing and return how many **earlier** calls found
    /// the same tool missing. Recording the same call again changes nothing and returns the same
    /// count, so a replayed call gets the answer it got.
    pub fn record_missing_tool(&mut self, call_id: &str, tool: &str) -> usize {
        let at = match self.missing_tools.iter().position(|h| h.call_id == call_id) {
            Some(at) => at,
            None => {
                self.missing_tools.push(MissingHit {
                    call_id: call_id.to_owned(),
                    tool: tool.to_owned(),
                });
                self.missing_tools.len() - 1
            }
        };
        self.missing_tools[..at]
            .iter()
            .filter(|h| h.tool == tool)
            .count()
    }
}

/// A branch that work of this conversation was pushed for: the branch a pull request for that work
/// is (or will be) opened from, and what a later task may continue. For a run that continued a
/// branch it is that branch (the line of work), not the run's own `agent/<run>` the commits were
/// pushed to first.
///
/// Written by `commit_and_push` itself, in the notes of the run that pushed; a run that continues
/// another inherits the ones of the run it continues (see `CoderAgent`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushedBranch {
    /// The repository, as a [`named`](super::named) key.
    pub repo: String,
    /// The branch name, `agent/...`.
    pub branch: String,
    /// The base branch its pull request is (or will be) against, when it was known: a run that
    /// continues the branch works against the same one, so it finds that pull request and does
    /// not open a second against another base. Absent in notes written before it was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
}

/// What the person answered to a question a tool wrote for them (see [`consent`](super::consent)):
/// the answer to `request_repository` ("may this repository join the workspace?").
///
/// Recorded by the agent before each step from the conversation, never from what the model says,
/// and kept whether the person said yes or an explicit no: a no is remembered so that the tool does
/// not ask again. An answer that is neither (`wait`, `?`) is not recorded, and the question can be
/// asked again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Consent {
    /// The tool call that asked. A provider may send the same id again in a later turn, so a
    /// consent is told apart by its `subject` and `tool` as well.
    pub call_id: String,
    /// The tool that asked.
    pub tool: String,
    /// What the person was asked about: the repository's [`named`](super::named) key.
    pub subject: String,
    /// Whether the answer was a yes (`consent::answer_of`); an explicit no is recorded too, and any
    /// other answer is not recorded at all.
    pub agreed: bool,
}

/// A repository this run created (`create_repository`, after the person agreed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatedRepo {
    /// `owner/name`, lowercase, as the call spelled it: what a repeated call is recognised by.
    pub full_name: String,
    /// The repository as a [`named`](super::named) key **of its clone URL**: the grant, which is what
    /// `prepare_workspace` and `publish_scratch` compare their argument with.
    pub key: String,
    /// The URL to clone and push over HTTP, as the host said.
    pub clone_url: String,
    /// The browser URL.
    pub html_url: String,
    /// Whether it was created private.
    pub private: bool,
}

/// A repository creation that was begun and whose outcome this run has not noted: written by
/// `create_repository` **before** it asks the host, and removed when the creation is noted or the
/// host definitely refused it. One that is still here when `create_repository` is called again is
/// what a process that died between the host's answer and the note leaves: a name that "already
/// exists" is then the run's own repository (`create_repository` looks it up and adopts it) and not
/// somebody else's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreationIntent {
    /// `owner/name`, lowercase, as the call spelled it (the key of [`CreatedRepo::full_name`]).
    pub full_name: String,
    /// The visibility the person agreed to.
    pub private: bool,
}

/// Everything remembered about one run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunNotes {
    /// Check/fix cycle.
    #[serde(default)]
    pub checks: ChecksNotes,
    /// The commit that was last pushed.
    #[serde(default)]
    pub pushed_sha: Option<String>,
    /// The pull request, once opened.
    #[serde(default)]
    pub pull_request: Option<PullRequestNote>,
    /// Why the run cannot deliver whatever the model does: the credentials
    /// were rejected. A run that ends with this set and no pull request fails
    /// (see `CoderAgent`) instead of completing.
    #[serde(default)]
    pub blocker: Option<String>,
    /// The repositories that may be in this run's workspace, as [`named`](super::named) keys:
    /// **granted** keys. `prepare_workspace` and `publish_scratch` work on no other. A key is
    /// granted when the person named it in their own messages of this run (the task and every
    /// answer), or agreed to the question `request_repository` wrote for it ([`consents`](Self::consents)).
    /// Filled by the agent before each step from the conversation, never from what the model says.
    #[serde(default)]
    pub named_repos: Vec<String>,
    /// The answers the person gave to the questions the coder's tools wrote (`request_repository`,
    /// `create_repository`), in order. Filled by the agent before each step from the conversation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub consents: Vec<Consent>,
    /// The repositories this run created, in order. Written by `create_repository` itself, which
    /// also grants the repository's key.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub created_repos: Vec<CreatedRepo>,
    /// The repository creations begun and not settled (see [`CreationIntent`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub creating: Vec<CreationIntent>,
    /// The branches that this run and the earlier tasks of the conversation pushed work for, which
    /// `prepare_workspace` may continue with its `branch`. `commit_and_push` writes its own; the
    /// agent adds, before each step, the ones in the notes of the run this one continues (and,
    /// when those notes are not there, the ones the tool's own result text reports, see
    /// `publish::pushed_in`). Never from what the model says.
    #[serde(default)]
    pub pushed_branches: Vec<PushedBranch>,
    /// The pushed branch this run continues (`prepare_workspace`'s `branch`), once its workspace
    /// is prepared: the commits go to the run's own branch, and this one is moved to them only by
    /// `open_pull_request`, after its gate. The verdict of a run that ends without a pull request
    /// says that this branch (and so its pull request) was not updated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continues: Option<String>,
    /// Which tool each `run_checks` or `run_command` call found missing, in order (one entry per
    /// call id, so a replay counts once): the second time the same tool is reported missing the
    /// answer is to ask the person, whatever the project says.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_tools: Vec<MissingHit>,
    /// `open_pull_request` moved the continued branch to the pushed commit. From then on the
    /// branch has the run's commits, whether or not the pull request could be reported.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub published: bool,
}

impl RunNotes {
    /// The check that decides for the code with tree id `tree`: **the most recent run, in any
    /// slot, whose tree it is.** A tree id is a content address, so the same tree is the same code,
    /// whichever slot ran the check (a scratch project that lands unchanged in an empty
    /// repository is bound to the checks it passed in the scratch); and the most recent run wins, so
    /// a green followed by a red on the same code reads as red. `None` means no check ran on it.
    pub fn checked(&self, tree: &str) -> Option<&CheckRecord> {
        let checks = &self.checks;
        // Notes from before the history have only `last`.
        let legacy = checks.last.iter().filter(|_| checks.history.is_empty());
        checks
            .history
            .iter()
            .rev()
            .chain(legacy)
            .find(|record| record.tree.as_deref() == Some(tree))
    }

    /// Whether a check run passed on exactly the code with tree id `tree` (what a pull request would
    /// contain), the most recent one that ran on it deciding ([`checked`](Self::checked)).
    pub fn verified_tree(&self, tree: Option<&str>) -> bool {
        tree.and_then(|tree| self.checked(tree))
            .is_some_and(|record| record.passed)
    }

    /// Remember the repositories `keys` names; returns whether anything was new.
    pub fn name_repos(&mut self, keys: impl IntoIterator<Item = String>) -> bool {
        let mut added = false;
        for key in keys {
            if !self.named_repos.contains(&key) {
                self.named_repos.push(key);
                added = true;
            }
        }
        added
    }

    /// Remember the answers `consents` of the person, and grant the repository of every one that
    /// agreed to `request_repository`'s question; returns whether anything was new. A consent is
    /// the same as one already held when its call, tool and subject are, so recording the answers
    /// of the whole conversation again at every step changes nothing.
    pub fn record_consents(&mut self, consents: impl IntoIterator<Item = Consent>) -> bool {
        let mut added = false;
        for consent in consents {
            if consent.agreed && consent.tool == super::consent::REQUEST_REPOSITORY {
                added |= self.name_repos([consent.subject.clone()]);
            }
            let known = self.consents.iter().any(|c| {
                c.call_id == consent.call_id
                    && c.tool == consent.tool
                    && c.subject == consent.subject
            });
            if !known {
                self.consents.push(consent);
                added = true;
            }
        }
        added
    }

    /// Whether a creation of exactly this repository and visibility was begun and not settled.
    pub fn is_creating(&self, full_name: &str, private: bool) -> bool {
        self.creating
            .iter()
            .any(|c| c.full_name == full_name && c.private == private)
    }

    /// Note that a creation is about to be asked of the host; returns whether it was new.
    pub fn begin_creating(&mut self, full_name: &str, private: bool) -> bool {
        if self.is_creating(full_name, private) {
            return false;
        }
        self.creating.push(CreationIntent {
            full_name: full_name.to_owned(),
            private,
        });
        true
    }

    /// Forget the creations of `full_name` (whatever the visibility): the host refused it, or it is
    /// noted in [`created_repos`](Self::created_repos). Returns whether anything was removed.
    pub fn settle_creating(&mut self, full_name: &str) -> bool {
        let before = self.creating.len();
        self.creating.retain(|c| c.full_name != full_name);
        self.creating.len() != before
    }

    /// Whether the person agreed to `subject` when `tool` asked: the latest answer decides.
    pub fn agreed(&self, tool: &str, subject: &str) -> bool {
        self.consents
            .iter()
            .rev()
            .find(|c| c.tool == tool && c.subject == subject)
            .is_some_and(|c| c.agreed)
    }

    /// Whether the person was asked about `subject` by `tool` and said no (and has not said yes
    /// since: the latest answer decides).
    pub fn declined(&self, tool: &str, subject: &str) -> bool {
        self.consents
            .iter()
            .rev()
            .find(|c| c.tool == tool && c.subject == subject)
            .is_some_and(|c| !c.agreed)
    }

    /// Remember the branches `pushed` names; returns whether anything was new.
    pub fn name_pushed_branches(&mut self, pushed: impl IntoIterator<Item = PushedBranch>) -> bool {
        let mut added = false;
        for one in pushed {
            match self
                .pushed_branches
                .iter_mut()
                .find(|p| p.repo == one.repo && p.branch == one.branch)
            {
                // Known: only a base it did not have yet is new.
                Some(known) if known.base.is_none() && one.base.is_some() => {
                    known.base = one.base;
                    added = true;
                }
                Some(_) => {}
                None => {
                    self.pushed_branches.push(one);
                    added = true;
                }
            }
        }
        added
    }

    /// The base branch recorded for `branch` of `repo`, if one was.
    pub fn pushed_base(&self, repo: &str, branch: &str) -> Option<&str> {
        self.pushed_branches
            .iter()
            .find(|p| p.repo == repo && p.branch == branch && p.base.is_some())
            .and_then(|p| p.base.as_deref())
    }

    /// Whether an earlier task of the conversation pushed `branch` of the repository `repo`.
    pub fn has_pushed(&self, repo: &str, branch: &str) -> bool {
        self.pushed_branches
            .iter()
            .any(|p| p.repo == repo && p.branch == branch)
    }

    /// Whether the most recent check run failed.
    pub fn last_check_failed(&self) -> bool {
        self.checks.last.as_ref().is_some_and(|c| !c.passed)
    }

    /// Whether the cycle limit is used up: `max` failures and the last run red.
    pub fn cycles_exhausted(&self, max: u32) -> bool {
        self.checks.failures >= max && self.last_check_failed()
    }

    /// Record a check run. A failed call counts once, however often it is
    /// replayed. Returns the failure count afterwards.
    pub fn record_check(&mut self, record: CheckRecord) -> u32 {
        if !record.passed && !self.checks.counted.contains(&record.call_id) {
            self.checks.counted.push(record.call_id.clone());
            self.checks.failures += 1;
        }
        // A call that runs again (a replay) replaces its own record instead of crowding the history.
        self.checks.history.retain(|r| r.call_id != record.call_id);
        self.checks.history.push(record.clone());
        if self.checks.history.len() > MAX_CHECK_HISTORY {
            let extra = self.checks.history.len() - MAX_CHECK_HISTORY;
            self.checks.history.drain(..extra);
        }
        self.checks.last = Some(record);
        self.checks.failures
    }
}

/// Where the notes of every run live.
#[derive(Debug, Clone)]
pub struct NotesStore {
    dir: PathBuf,
}

impl NotesStore {
    /// Notes under `<workspace_root>/coder`.
    pub fn new(workspace_root: &std::path::Path) -> Self {
        Self {
            dir: workspace_root.join("coder"),
        }
    }

    fn path(&self, run: &str) -> io::Result<PathBuf> {
        // Run ids are UUIDs; refuse anything that could leave the directory.
        if run.is_empty() || !run.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unusable run id {run:?}"),
            ));
        }
        Ok(self.dir.join(format!("{run}.json")))
    }

    /// The notes of `run` (empty if none were written yet).
    ///
    /// # Errors
    ///
    /// I/O errors, or a file that is not valid notes.
    pub async fn load(&self, run: &str) -> io::Result<RunNotes> {
        match tokio::fs::read(self.path(run)?).await {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(RunNotes::default()),
            Err(e) => Err(e),
        }
    }

    /// The notes of `run`, or `None` when none were ever written for it (another worker's volume,
    /// a purged directory, a run that never took a step).
    ///
    /// # Errors
    ///
    /// I/O errors, or a file that is not valid notes.
    pub async fn load_existing(&self, run: &str) -> io::Result<Option<RunNotes>> {
        match tokio::fs::read(self.path(run)?).await {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Replace the notes of `run`, atomically.
    ///
    /// # Errors
    ///
    /// I/O errors.
    pub async fn save(&self, run: &str, notes: &RunNotes) -> io::Result<()> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = self.path(run)?;
        tokio::fs::create_dir_all(&self.dir).await?;
        let tmp = self.dir.join(format!(
            ".{run}.{}.{}.tmp",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let bytes = serde_json::to_vec_pretty(notes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        tokio::fs::write(&tmp, bytes).await?;
        tokio::fs::rename(&tmp, &path).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(call: &str, passed: bool) -> CheckRecord {
        CheckRecord {
            call_id: call.into(),
            command: "cargo test".into(),
            passed,
            exit_code: Some(i32::from(!passed)),
            tail: "out".into(),
            tree: Some("t1".into()),
            report: None,
            slot: None,
        }
    }

    #[test]
    fn a_replayed_failure_counts_once() {
        let mut notes = RunNotes::default();
        assert_eq!(notes.record_check(record("c1", false)), 1);
        assert_eq!(notes.record_check(record("c1", false)), 1, "same call id");
        assert_eq!(notes.record_check(record("c2", false)), 2);
        assert!(notes.last_check_failed());
        assert!(notes.cycles_exhausted(2));
        assert!(!notes.cycles_exhausted(3));
        notes.record_check(record("c3", true));
        assert!(!notes.last_check_failed());
        assert!(!notes.cycles_exhausted(2), "green now");
        assert_eq!(notes.checks.failures, 2, "cumulative");
    }

    fn on_tree(call: &str, tree: &str, passed: bool, slot: &str) -> CheckRecord {
        CheckRecord {
            tree: Some(tree.into()),
            slot: Some(slot.into()),
            ..record(call, passed)
        }
    }

    #[test]
    fn the_most_recent_check_on_a_tree_decides_whatever_slot_ran_it() {
        let mut notes = RunNotes::default();
        notes.record_check(on_tree("c1", "tree-a", true, "scratch"));
        notes.record_check(on_tree("c2", "tree-b", false, "lib"));
        // Another slot's check is the one that ran on this tree.
        assert_eq!(notes.checked("tree-a").unwrap().call_id, "c1");
        assert!(notes.verified_tree(Some("tree-a")));
        assert!(!notes.verified_tree(Some("tree-b")));
        assert!(!notes.verified_tree(Some("tree-c")), "no check ran on it");
        assert!(!notes.verified_tree(None));
        // A green, then a red on the same code: red.
        notes.record_check(on_tree("c3", "tree-a", false, "lib"));
        assert_eq!(notes.checked("tree-a").unwrap().call_id, "c3");
        assert!(!notes.verified_tree(Some("tree-a")));
        // And red, then green: green.
        notes.record_check(on_tree("c4", "tree-a", true, "lib"));
        assert!(notes.verified_tree(Some("tree-a")));
        assert_eq!(
            notes.checks.last.as_ref().unwrap().call_id,
            "c4",
            "`last` is the newest of all"
        );
    }

    #[test]
    fn the_history_keeps_the_last_runs_only() {
        let mut notes = RunNotes::default();
        for i in 0..MAX_CHECK_HISTORY + 5 {
            notes.record_check(on_tree(&format!("c{i}"), &format!("tree-{i}"), true, "a"));
        }
        assert_eq!(notes.checks.history.len(), MAX_CHECK_HISTORY);
        assert_eq!(
            notes.checks.history[0].call_id, "c5",
            "the oldest were dropped"
        );
        assert!(
            notes.checked("tree-4").is_none(),
            "a dropped run no longer decides"
        );
        assert!(notes.checked("tree-5").is_some());
        // A replayed call replaces its own record, and counts once.
        assert_eq!(notes.record_check(on_tree("c40", "tree-x", false, "a")), 1);
        assert_eq!(notes.record_check(on_tree("c40", "tree-x", false, "a")), 1);
        assert_eq!(notes.checks.history.len(), MAX_CHECK_HISTORY);
        assert_eq!(
            notes
                .checks
                .history
                .iter()
                .filter(|r| r.call_id == "c40")
                .count(),
            1
        );
    }

    #[test]
    fn notes_from_before_the_history_still_decide_by_their_last_run() {
        let notes: RunNotes = serde_json::from_value(serde_json::json!({
            "checks": {"failures": 0, "counted": [], "last": {
                "call_id": "c1", "command": "x", "passed": true, "exit_code": 0, "tail": "", "tree": "t9"
            }}
        }))
        .unwrap();
        assert!(notes.checks.history.is_empty());
        assert!(notes.verified_tree(Some("t9")));
        assert!(!notes.verified_tree(Some("t8")));
    }

    #[tokio::test]
    async fn notes_round_trip_and_default_to_empty() {
        let root = tempfile::tempdir().unwrap();
        let store = NotesStore::new(root.path());
        assert_eq!(store.load("run-1").await.unwrap(), RunNotes::default());
        let mut notes = RunNotes::default();
        notes.record_check(record("c1", false));
        notes.pushed_sha = Some("abc".into());
        store.save("run-1", &notes).await.unwrap();
        assert_eq!(store.load("run-1").await.unwrap(), notes);
        assert_eq!(store.load("run-2").await.unwrap(), RunNotes::default());
    }

    #[tokio::test]
    async fn hostile_run_ids_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let store = NotesStore::new(root.path());
        assert!(store.load("../etc/passwd").await.is_err());
        assert!(store.save("a/b", &RunNotes::default()).await.is_err());
        assert!(store.load("").await.is_err());
    }
}
