//! Where the tools ask `git` ([`GitAt`]) and the one question every tool asks: the files of a
//! slot ([`list_files`]), and what `git status` says ([`status_text`]). The coder asks more of the
//! same `GitAt` (`adam_coder`'s `tools::gitcli`).
//!
//! A question is asked **pinned** to the slot's repository as the workspace's metadata names it:
//! never to wherever the slot's `.git` file, a `commondir` or a `gitdir` says, which a run's commands
//! write. No credentials are involved.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use adam_workspace::{GitPin, LocalGit, Slot, SlotDir, Worktree};

/// Where a git question is asked: a directory, and the repository `git` works on there.
///
/// A slot's repository is pinned (`GIT_DIR`, `GIT_COMMON_DIR`, `GIT_WORK_TREE`, from the coder's
/// metadata; see [`GitPin`]). A worktree whose repository the metadata cannot tell is **refused**: no
/// `git` is started, and every question about it is "unknown" (`None`), as for a repository that
/// cannot be read. A plain directory is discovered as `git` always did (the tests').
#[derive(Debug, Clone)]
pub struct GitAt {
    /// The slot's directory, as the coder opened it: what the files of the slot are read and
    /// written relative to, and what `git`'s name for it is checked against.
    root: Arc<SlotDir>,
    pin: Pin,
}

#[derive(Debug, Clone)]
enum Pin {
    /// Discover the repository from the directory.
    ///
    /// For a slot this is behind `test-util` on purpose — a run's directory must not discover a
    /// repository of its own — and it is always available through [`GitAt::directory`], which is the
    /// person's own directory ([ADR 0047], slice 7): no pin of a run exists there to honour.
    ///
    /// [ADR 0047]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0047-adam-rs-can-be-embedded-in-an-application.md
    Plain,
    Pinned(GitPin),
    Refused,
}

impl GitAt {
    /// A directory whose repository `git` finds by itself: for tests and for directories that are
    /// no slot.
    #[cfg(any(test, feature = "test-util"))]
    #[allow(
        clippy::expect_used,
        reason = "a test helper: a directory that cannot be opened ends the test"
    )]
    pub fn plain(dir: &Path) -> Self {
        Self {
            root: Arc::new(SlotDir::open(dir).expect("a plain directory")),
            pin: Pin::Plain,
        }
    }

    /// A directory the person owns, whose repository `git` finds by itself: the person's own
    /// directory as a workspace ([ADR 0047], slice 7).
    ///
    /// Unlike a slot, nothing pins which repository this is: nobody's metadata describes a person's
    /// directory, and a run never owns it, so `git` is asked there as it would be in a terminal.
    ///
    /// # Errors
    ///
    /// The directory cannot be opened: it does not exist, is not a directory, or is not readable.
    ///
    /// [ADR 0047]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0047-adam-rs-can-be-embedded-in-an-application.md
    pub fn directory(dir: &Path) -> std::io::Result<Self> {
        Ok(Self {
            root: Arc::new(SlotDir::open(dir)?),
            pin: Pin::Plain,
        })
    }

    /// A directory whose repository is not known: every question is "unknown", as for a repository
    /// the coder's metadata cannot tell.
    #[cfg(any(test, feature = "test-util"))]
    #[allow(
        clippy::expect_used,
        reason = "a test helper: a directory that cannot be opened ends the test"
    )]
    pub fn refused(dir: &Path) -> Self {
        Self::of(
            Arc::new(SlotDir::open(dir).expect("a plain directory")),
            None,
        )
    }

    /// A slot: a worktree of a repository or a scratch project.
    pub fn slot(slot: &Slot) -> Self {
        Self::of(Arc::clone(slot.root()), slot.git_pin())
    }

    /// A repository's worktree.
    pub fn worktree(worktree: &Worktree) -> Self {
        Self::of(Arc::clone(worktree.root()), worktree.git_pin())
    }

    /// A slot's directory with no pin: the repository is **refused**, as for a slot whose metadata
    /// cannot say (a test's). Only with the feature `test-util`.
    #[cfg(any(test, feature = "test-util"))]
    pub fn refused_at(root: Arc<SlotDir>) -> Self {
        Self::of(root, None)
    }

    // Private: a root paired with another slot's pin would ask `git` about one repository from the
    // directory of another.
    fn of(root: Arc<SlotDir>, pin: Option<GitPin>) -> Self {
        Self {
            root,
            pin: pin.map_or(Pin::Refused, Pin::Pinned),
        }
    }

    /// The directory the questions are asked in: the slot's.
    pub fn path(&self) -> &Path {
        self.root.path()
    }

    /// Where a copy of the index is made: a directory of the work volume when git runs in the
    /// sandbox, `None` (the process's temporary directory) when it runs here.
    pub fn private_dir(&self) -> Option<&Path> {
        match &self.pin {
            Pin::Pinned(pin) => pin.sandbox().map(adam_workspace::GitSandbox::private_dir),
            _ => None,
        }
    }

    /// The slot's directory, held open.
    pub fn root(&self) -> &Arc<SlotDir> {
        &self.root
    }

    /// `path` relative to the slot's directory, when it is below it (a scratch project's git
    /// directory is; a worktree's, in the mirror, is not).
    pub fn below_slot(&self, path: &Path) -> Option<PathBuf> {
        path.strip_prefix(self.path())
            .ok()
            .map(Path::to_path_buf)
            .filter(|rel| !rel.as_os_str().is_empty())
    }

    /// What the slot's `.git` file says, read as a regular file relative to the slot's directory
    /// and not through a link; `None` when it cannot be read that way.
    pub async fn dot_git(&self) -> Option<String> {
        self.root
            .blocking(|root| {
                root.read_regular_text_below(
                    Path::new(".git"),
                    adam_workspace::safe_fs::MAX_SMALL_FILE,
                )
            })
            .await
            .ok()
    }

    /// Put the slot's `.git` file back as `text`: written next to it and renamed over it, in the
    /// slot's directory, so a link or a pipe a command put at `.git` is replaced and never written
    /// through, and a slot that was swapped for a link meanwhile is not followed.
    pub async fn put_dot_git(&self, text: String) -> bool {
        self.root
            .blocking(move |root| root.replace_file_below(Path::new(".git"), text.as_bytes()))
            .await
            .is_ok()
    }

    /// The repository's common directory when the coder's metadata names it (a worktree's mirror,
    /// a scratch project's `.git`): the pin already says what `git rev-parse --git-common-dir`
    /// would answer, and asking it would be a process for a value that cannot change. `None` for a
    /// directory `git` finds by itself (the tests'), which has to be asked.
    pub fn pinned_common_dir(&self) -> Option<&Path> {
        match &self.pin {
            Pin::Pinned(pin) => Some(pin.common_dir()),
            // A directory `git` finds by itself (a test's, or a person's through
            // [`GitAt::directory`]) has to be asked.
            Pin::Plain => None,
            Pin::Refused => None,
        }
    }

    /// The repository's index: the file in the git directory the pin names (not what `git` is
    /// asked, which reads files a run writes), or, for a directory `git` finds by itself, what it
    /// says. `None` for a refused repository.
    pub async fn index_path(&self) -> Option<PathBuf> {
        match &self.pin {
            Pin::Plain => {
                let path = git_stdout(self, &["rev-parse", "--git-path", "index"]).await?;
                Some(self.path().join(path))
            }
            Pin::Pinned(pin) => Some(pin.git_dir().join("index")),
            Pin::Refused => None,
        }
    }

    /// `git`, from an empty environment ([`adam_workspace::confine_git_env`]: a filter the
    /// repository configures runs in it), in the directory and pinned. It runs in the git sandbox
    /// when the deployment has one (`GIT_SANDBOX_SOCKET`; a filter then runs where no secret is
    /// readable), else in this container. `None` when the repository is refused or its pin cannot be
    /// trusted (see [`GitPin::check`]).
    pub async fn command(&self) -> Option<LocalGit> {
        let cmd = match &self.pin {
            Pin::Plain => LocalGit::new(self.path(), None),
            Pin::Pinned(pin) => {
                if let Err(why) = pin.check().await {
                    tracing::warn!(dir = %self.path().display(), %why, "git is not run here");
                    return None;
                }
                adam_workspace::safe_fs::race::checkpoint(self.path(), "git");
                // Once more, as near to the start as the coder can look: `git` is given a name,
                // not the descriptor.
                if let Err(why) = pin.still_here().await {
                    tracing::warn!(dir = %self.path().display(), %why, "git is not run here");
                    return None;
                }
                LocalGit::new(self.path(), pin.sandbox()).pinned(pin)
            }
            Pin::Refused => {
                tracing::warn!(
                    dir = %self.path().display(),
                    "git is not run here: which directory of the mirror is this worktree's is not known"
                );
                return None;
            }
        };
        Some(cmd)
    }
}

/// Run `git <args>` where `at` says; `Some(stdout)` (trimmed) on exit 0.
pub async fn git_stdout(at: &GitAt, args: &[&str]) -> Option<String> {
    let out = at.command().await?.args(args).output().await.ok()?;
    out.success
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// What `git status` says, exactly as it says it (`--porcelain=v1 -z`, every untracked file): a
/// cheap picture of the worktree that needs no commit and no index copy. Not trimmed, unlike
/// [`git_stdout`]: an entry's status starts with a space when nothing is staged (` M a`), and that
/// space is part of what it says.
pub async fn status_text(at: &GitAt) -> Option<String> {
    let out = at
        .command()
        .await?
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .output()
        .await
        .ok()?;
    out.success
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Most paths [`list_files`] gives.
pub const MAX_LISTED_PATHS: usize = 200_000;
/// Most bytes of `git`'s answer [`list_files`] reads.
pub const MAX_LISTED_BYTES: usize = 16 * 1024 * 1024;

/// Why [`list_files`] has no list.
#[derive(Debug, PartialEq, Eq)]
pub enum ListError {
    /// `git` cannot be run for this slot (a refused repository, a pin that cannot be trusted, no
    /// sandbox): which files are ignored is not known, and the caller must not guess.
    Unavailable,
    /// More than the most paths or bytes the caller allows.
    TooMany,
    /// `git` ran and failed; what it said, with no path of this machine in it.
    Failed(String),
}

/// What `git` said, for the model: the slot's directory is `.` and any other absolute path is
/// `<path>`, so that no name of this machine's file system reaches the conversation.
fn without_host_paths(said: &str, slot: &Path) -> String {
    let slot = slot.to_string_lossy();
    let said = if slot.is_empty() {
        said.to_owned()
    } else {
        said.replace(slot.as_ref(), ".")
    };
    said.split_inclusive(|c: char| c.is_whitespace() || matches!(c, '\'' | '"' | ':' | ','))
        .map(|word| {
            let end = word
                .find(|c: char| c.is_whitespace() || matches!(c, '\'' | '"' | ':' | ','))
                .unwrap_or(word.len());
            if word.starts_with('/') {
                format!("<path>{}", &word[end..])
            } else {
                word.to_owned()
            }
        })
        .collect()
}

/// The files of the slot below `dir` (a path relative to the slot; empty for all of it) that git
/// shows: tracked ones and untracked ones that no ignore rule hides (`ls-files --cached --others
/// --exclude-standard`), as paths from the slot's root (**bytes**: a name need not be UTF-8), sorted
/// by bytes and each once.
///
/// This is the **only** way the read tools enumerate files: it asks `git` through [`GitAt`], so the
/// git sandbox applies when there is one, and nested `.gitignore` files, `info/exclude` and the
/// global rules are `git`'s own and agree with what a commit of the slot sees. It never walks the
/// directory by its name. Left out: a path under a `.git` (any case) and a directory (an embedded
/// repository is listed by `git` with a trailing `/`). A tracked file that was deleted is still
/// listed: the caller looks at each file it uses.
///
/// `git`'s answer is read as it comes and held to [`MAX_LISTED_BYTES`]: past it the command is
/// stopped, and a list of more than [`MAX_LISTED_PATHS`] paths is refused too ([`ListError::TooMany`]).
pub async fn list_files(at: &GitAt, dir: &Path) -> Result<Vec<Vec<u8>>, ListError> {
    list_files_within(at, dir, MAX_LISTED_PATHS, MAX_LISTED_BYTES).await
}

/// [`list_files`] with its two limits given.
pub async fn list_files_within(
    at: &GitAt,
    dir: &Path,
    max_paths: usize,
    max_bytes: usize,
) -> Result<Vec<Vec<u8>>, ListError> {
    let Some(git) = at.command().await else {
        return Err(ListError::Unavailable);
    };
    let mut git = git.args([
        "--literal-pathspecs",
        "ls-files",
        "-z",
        "--cached",
        "--others",
        "--exclude-standard",
    ]);
    if !dir.as_os_str().is_empty() {
        git = git.arg("--").arg(dir);
    }
    let out = git
        .output_capped(max_bytes)
        .await
        .map_err(|e| ListError::Failed(without_host_paths(&e.to_string(), at.path())))?;
    if out.exceeded {
        return Err(ListError::TooMany);
    }
    if !out.success {
        return Err(ListError::Failed(without_host_paths(
            String::from_utf8_lossy(&out.stderr).trim(),
            at.path(),
        )));
    }
    sorted_paths(&out.stdout, max_paths)
}

/// The paths of `git ls-files -z` output, filtered, sorted by bytes and each once; at most
/// `max_paths` of them.
fn sorted_paths(stdout: &[u8], max_paths: usize) -> Result<Vec<Vec<u8>>, ListError> {
    let mut paths: Vec<Vec<u8>> = Vec::new();
    for path in stdout.split(|&b| b == 0) {
        if path.is_empty() || path.ends_with(b"/") {
            continue;
        }
        if path
            .split(|&b| b == b'/')
            .any(|part| part.eq_ignore_ascii_case(b".git"))
        {
            continue;
        }
        paths.push(path.to_vec());
    }
    paths.sort_unstable();
    paths.dedup();
    if paths.len() > max_paths {
        return Err(ListError::TooMany);
    }
    Ok(paths)
}

#[cfg(test)]
mod list_tests {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt as _;
    use std::process::Command;

    use super::*;

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let out = Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(out.status.success());
        dir
    }

    fn touch(dir: &Path, name: &[u8]) {
        let path = dir.join(OsStr::from_bytes(name));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "x\n").unwrap();
    }

    #[tokio::test]
    async fn an_unsorted_untracked_set_is_given_sorted_by_bytes() {
        let dir = repo();
        for name in ["z.txt", "B.txt", "a/b.txt", "a.txt", "a-b.txt", "_u.txt"] {
            touch(dir.path(), name.as_bytes());
        }
        let got = list_files(&GitAt::plain(dir.path()), Path::new(""))
            .await
            .unwrap();
        let mut want = got.clone();
        want.sort();
        assert_eq!(got, want);
        let names: Vec<String> = got
            .iter()
            .map(|p| String::from_utf8_lossy(p).into())
            .collect();
        assert_eq!(
            names,
            ["B.txt", "_u.txt", "a-b.txt", "a.txt", "a/b.txt", "z.txt"]
        );
    }

    /// `ls-files --cached --others` gives the tracked files and then the untracked ones: the union
    /// is sorted here, by bytes, and each path is given once.
    #[tokio::test]
    async fn tracked_then_untracked_files_are_merged_into_one_sorted_list() {
        let dir = repo();
        touch(dir.path(), b"z-tracked.txt");
        touch(dir.path(), b"m-tracked.txt");
        let out = Command::new("git")
            .args(["add", "z-tracked.txt", "m-tracked.txt"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(out.status.success());
        touch(dir.path(), b"a-untracked.txt");
        let got = list_files(&GitAt::plain(dir.path()), Path::new(""))
            .await
            .unwrap();
        let names: Vec<String> = got
            .iter()
            .map(|p| String::from_utf8_lossy(p).into())
            .collect();
        assert_eq!(names, ["a-untracked.txt", "m-tracked.txt", "z-tracked.txt"]);
    }

    #[test]
    fn the_paths_of_git_are_sorted_filtered_and_given_once() {
        let out = b"z\0b/.GIT/f\0a\0\0dir/\0b\0a\0bad-\xff\0";
        let got = sorted_paths(out, 10).unwrap();
        assert_eq!(
            got,
            [
                b"a".to_vec(),
                b"b".to_vec(),
                b"bad-\xff".to_vec(),
                b"z".to_vec()
            ]
        );
        assert_eq!(sorted_paths(out, 3), Err(ListError::TooMany));
    }

    #[tokio::test]
    async fn a_dot_git_of_any_case_below_a_directory_is_left_out() {
        let dir = repo();
        touch(dir.path(), b"x/.GIT/f");
        touch(dir.path(), b"x/.Git/g");
        touch(dir.path(), b"x/ok.txt");
        let got = list_files(&GitAt::plain(dir.path()), Path::new(""))
            .await
            .unwrap();
        assert_eq!(got, [b"x/ok.txt".to_vec()]);
    }

    #[tokio::test]
    async fn a_name_that_is_not_utf8_is_listed_as_the_bytes_it_is() {
        let dir = repo();
        touch(dir.path(), b"bad-\xff.txt");
        touch(dir.path(), b"good.txt");
        let got = list_files(&GitAt::plain(dir.path()), Path::new(""))
            .await
            .unwrap();
        assert_eq!(got, [b"bad-\xff.txt".to_vec(), b"good.txt".to_vec()]);
    }

    #[tokio::test]
    async fn more_paths_or_bytes_than_allowed_is_too_many_and_git_is_stopped() {
        let dir = repo();
        for i in 0..50 {
            touch(dir.path(), format!("f{i:02}.txt").as_bytes());
        }
        let at = GitAt::plain(dir.path());
        assert_eq!(
            list_files_within(&at, Path::new(""), 49, MAX_LISTED_BYTES).await,
            Err(ListError::TooMany)
        );
        assert_eq!(
            list_files_within(&at, Path::new(""), 50, MAX_LISTED_BYTES)
                .await
                .unwrap()
                .len(),
            50
        );
        // 50 paths of 8 bytes are 400 bytes: a cap of 100 stops the command before it is read.
        assert_eq!(
            list_files_within(&at, Path::new(""), usize::MAX, 100).await,
            Err(ListError::TooMany)
        );
    }

    #[test]
    fn what_git_says_names_no_path_of_this_machine() {
        let slot = Path::new("/work/workspaces/run-1/repo");
        let said = "fatal: cannot open '/work/workspaces/run-1/repo/.git/index': denied; \
                    also /mirror/objects/ab: bad";
        let shown = without_host_paths(said, slot);
        assert!(
            !shown.contains("/work") && !shown.contains("/mirror"),
            "{shown}"
        );
        assert!(
            shown.contains("./.git/index") || shown.contains("<path>"),
            "{shown}"
        );
        assert_eq!(without_host_paths("nothing here", slot), "nothing here");
    }
}
