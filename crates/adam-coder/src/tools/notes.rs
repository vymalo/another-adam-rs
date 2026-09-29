//! What the coder remembers about a run besides the conversation: how many
//! check cycles were spent, what the last check said, what was pushed and which
//! pull request was opened.
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
}

/// State of the check/fix cycle.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChecksNotes {
    /// Failed `run_checks` calls so far.
    pub failures: u32,
    /// Call ids already counted in `failures`.
    pub counted: Vec<String>,
    /// The most recent run.
    pub last: Option<CheckRecord>,
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
}

impl RunNotes {
    /// Whether the last check run passed on exactly the code with tree id
    /// `tree` (what a pull request would contain).
    pub fn verified_tree(&self, tree: Option<&str>) -> bool {
        match (&self.checks.last, tree) {
            (Some(last), Some(tree)) => last.passed && last.tree.as_deref() == Some(tree),
            _ => false,
        }
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
