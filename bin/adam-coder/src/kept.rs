//! Kept output on the workspace volume: what `read_output` reads.
//!
//! A command's long output, and a long result of an MCP server, are kept whole under
//! `<WORKSPACE_ROOT>/outputs/<root run>/<id>.txt` ([`FileKept`], an [`adam_llm_agent::KeptOutputs`]),
//! and the model is shown the first page or the head and the tail with an output id.
//!
//! * **Not in the journal.** The files are on the volume, beside the notes (`coder/<run>.json`) and
//!   the workspaces. A worker that takes the run over reads the same files, whatever environment the
//!   run's commands use: the output is captured by this process from the command's pipes, and
//!   `read_output` reads it here, so a run pod or a devcontainer never sees `outputs/` (a pod mounts
//!   only the slots of its run).
//! * **Scrubbed before it is written.** [`FileKept::keep`](adam_llm_agent::KeptOutputs::keep) scrubs
//!   the whole text with the process's [`Redactor`] and then cuts and folds it, so no secret it knows
//!   is ever on the volume, and a secret is never split by a cut, a fold or a page boundary (the
//!   pages are made from the scrubbed file). A secret registered **after** the keep is not in the
//!   file's text, and the tools' `Redacting` layer scrubs every page on its way out: that layer,
//!   not the file, is the read-time guarantee.
//! * **Private, and no link is followed on either side.** The directories are opened one component
//!   at a time below the workspace root without following a link (`SlotDir::walk`), made `0700`, and
//!   the file is made in the held run directory with `O_EXCL` and `O_NOFOLLOW`, `0600`, written
//!   under a temporary name and renamed; a link or a file in the way is refused (`NOT kept`), the
//!   volume's paths are never in a message, and a file with more than one hard link is not read. A
//!   command of the **local** environment (and a Podman devcontainer's) runs as the same user and
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
//!   workspace goes, and the directories of runs that have no workspace any more.

use std::io::{self, BufReader, Write as _};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use adam_llm_agent::paging::{PageError, page_lines};
use adam_llm_agent::{KeptError, KeptInfo, KeptOutputs, count_lines, is_output_id, prepare};
use adam_workspace::safe_fs::{Dir, SlotDir, WalkError};
use async_trait::async_trait;

use crate::redact::Redactor;

/// The most bytes one run keeps on the volume.
pub const RUN_KEPT_MAX: u64 = 64 * 1024 * 1024;

/// A leftover temporary file is removed when it is older than this.
const STALE_TMP: Duration = Duration::from_secs(600);

/// One check-then-write at a time in this process.
static KEEP_LOCK: Mutex<()> = Mutex::new(());

/// Kept outputs as files under `<root>/outputs`.
#[derive(Debug, Clone)]
pub struct FileKept {
    root: std::path::PathBuf,
    redactor: Redactor,
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

/// `o-<16 hex>.txt`: a file this process makes.
fn is_ours(name: &str) -> bool {
    name.strip_suffix(".txt").is_some_and(is_output_id)
}

/// `.o-<16 hex>.<pid>.<n>.tmp`
fn is_tmp(name: &str) -> bool {
    name.starts_with(".o-") && name.ends_with(".tmp")
}

impl FileKept {
    /// Outputs under `<workspace_root>/outputs`, scrubbed with `redactor` (clones of a redactor
    /// share what they know, so a secret added later is scrubbed too).
    pub fn new(workspace_root: &Path, redactor: Redactor) -> Self {
        Self {
            root: workspace_root.to_path_buf(),
            redactor,
        }
    }

    /// The directory that holds every run's outputs.
    pub fn root(&self) -> std::path::PathBuf {
        self.root.join("outputs")
    }

    /// The directory of `run`'s outputs (not made).
    pub fn run_dir(&self, run: &str) -> Option<std::path::PathBuf> {
        usable_run(run).then(|| self.root().join(run))
    }

    /// Remove every output of `run`. Idempotent. A link in the way is removed, not followed.
    ///
    /// # Errors
    ///
    /// I/O errors other than the directory not being there.
    pub async fn remove_run(&self, run: &str) -> io::Result<()> {
        let Some(dir) = self.run_dir(run) else {
            return Ok(());
        };
        match tokio::fs::symlink_metadata(&dir).await {
            Ok(meta) if meta.is_dir() => tokio::fs::remove_dir_all(&dir).await,
            Ok(_) => tokio::fs::remove_file(&dir).await,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// The runs that have outputs on the volume.
    ///
    /// # Errors
    ///
    /// I/O errors listing the directory.
    pub async fn runs(&self) -> io::Result<Vec<String>> {
        let mut runs = Vec::new();
        let mut entries = match tokio::fs::read_dir(self.root()).await {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(runs),
            Err(e) => return Err(e),
        };
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name().to_string_lossy().into_owned();
            // A link is listed too: it is removed, not followed, with its run.
            if usable_run(&name) {
                runs.push(name);
            }
        }
        runs.sort();
        Ok(runs)
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
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        // Scrubbed first, then cut and folded: a secret is never split by either.
        let scrubbed = self.redactor.scrub(text);
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
                let age = self
                    .run_dir(run)
                    .map(|d| d.join(&entry))
                    .and_then(|p| std::fs::symlink_metadata(p).ok())
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| SystemTime::now().duration_since(t).ok());
                if age.is_some_and(|age| age > STALE_TMP) {
                    let _ = dir.unlink(&entry);
                }
            }
        }
        if used + prepared.len() as u64 > RUN_KEPT_MAX {
            return Err(KeptError::Refused(format!(
                "a run keeps at most {} MiB of output, and this run holds {} MiB already",
                RUN_KEPT_MAX / (1024 * 1024),
                used / (1024 * 1024)
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
            Err(e) => return Err(io_error(&e)),
        };
        // A hard link planted by a command: not ours.
        #[cfg(unix)]
        if std::os::unix::fs::MetadataExt::nlink(&file.metadata().map_err(|e| io_error(&e))?) != 1 {
            return Err(KeptError::Refused(
                "that output file is not usable (it has been linked)".to_owned(),
            ));
        }
        match page_lines(BufReader::new(file), offset, limit, false) {
            Ok(page) => Ok(page.render()),
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
            .map_err(|e| KeptError::Io(e.to_string()))?
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
            .map_err(|e| KeptError::Io(e.to_string()))?
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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
        assert!(!on_disk.contains(secret) && on_disk.contains("key=[redacted]"));
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
