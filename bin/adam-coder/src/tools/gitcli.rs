//! The few `git` questions the tools ask that `adam-workspace` does not answer. They are asked
//! **pinned** to the slot's repository as the coder's own metadata names it ([`GitAt`]): never to
//! wherever the slot's `.git` file, a `commondir` or a `gitdir` says, which a run's commands write.
//! No credentials are involved.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use adam_workspace::{GitPin, Slot, Worktree};
use tokio::process::Command;

/// Where a git question is asked: a directory, and the repository `git` works on there.
///
/// A slot's repository is pinned (`GIT_DIR`, `GIT_COMMON_DIR`, `GIT_WORK_TREE`, from the coder's
/// metadata; see [`GitPin`]). A worktree whose repository the metadata cannot tell is **refused**: no
/// `git` is started, and every question about it is "unknown" (`None`), as for a repository that
/// cannot be read. A plain directory is discovered as `git` always did (the tests').
#[derive(Debug, Clone)]
pub(crate) struct GitAt {
    dir: PathBuf,
    pin: Pin,
}

#[derive(Debug, Clone)]
enum Pin {
    /// Discover the repository from the directory.
    #[cfg(test)]
    Plain,
    Pinned(GitPin),
    Refused,
}

impl GitAt {
    /// A directory whose repository `git` finds by itself: for tests and for directories that are
    /// no slot.
    #[cfg(test)]
    pub(crate) fn plain(dir: &Path) -> Self {
        Self {
            dir: dir.to_owned(),
            pin: Pin::Plain,
        }
    }

    /// A slot: a worktree of a repository or a scratch project.
    pub(crate) fn slot(slot: &Slot) -> Self {
        Self::of(slot.path(), slot.git_pin())
    }

    /// A repository's worktree.
    pub(crate) fn worktree(worktree: &Worktree) -> Self {
        Self::of(worktree.path(), worktree.git_pin())
    }

    fn of(dir: &Path, pin: Option<GitPin>) -> Self {
        Self {
            dir: dir.to_owned(),
            pin: pin.map_or(Pin::Refused, Pin::Pinned),
        }
    }

    /// The directory the questions are asked in: the slot's.
    pub(crate) fn path(&self) -> &Path {
        &self.dir
    }

    /// The repository's index: the file in the git directory the pin names (not what `git` is
    /// asked, which reads files a run writes), or, for a directory `git` finds by itself, what it
    /// says. `None` for a refused repository.
    pub(crate) async fn index_path(&self) -> Option<PathBuf> {
        match &self.pin {
            #[cfg(test)]
            Pin::Plain => {
                let path = git_stdout(self, &["rev-parse", "--git-path", "index"]).await?;
                Some(self.dir.join(path))
            }
            Pin::Pinned(pin) => Some(pin.git_dir().join("index")),
            Pin::Refused => None,
        }
    }

    /// Whether the slot's directory is a directory itself and not a link a command put in its
    /// place: what a caller that writes in it checks first.
    pub(crate) async fn is_plain_directory(&self) -> bool {
        adam_workspace::safe_fs::is_real_directory(&self.dir).await
    }

    /// `git`, from an empty environment ([`adam_workspace::confine_git_env`]: a filter the
    /// repository configures runs in it), in the directory and pinned. `None` when the repository
    /// is refused or its pin cannot be trusted (see [`GitPin::check`]).
    pub(crate) async fn command(&self) -> Option<Command> {
        let mut cmd = Command::new("git");
        adam_workspace::confine_git_env(&mut cmd);
        match &self.pin {
            #[cfg(test)]
            Pin::Plain => {}
            Pin::Pinned(pin) => {
                if let Err(why) = pin.check().await {
                    tracing::warn!(dir = %self.dir.display(), %why, "git is not run here");
                    return None;
                }
                pin.apply(&mut cmd);
            }
            Pin::Refused => {
                tracing::warn!(
                    dir = %self.dir.display(),
                    "git is not run here: which directory of the mirror is this worktree's is not known"
                );
                return None;
            }
        }
        cmd.args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
        ])
        .current_dir(&self.dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
        Some(cmd)
    }
}

/// Run `git <args>` where `at` says; `Some(stdout)` (trimmed) on exit 0.
pub(crate) async fn git_stdout(at: &GitAt, args: &[&str]) -> Option<String> {
    let mut cmd = at.command().await?;
    let out = cmd.args(args).output().await.ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// The commit `HEAD` points at.
pub(crate) async fn head_sha(at: &GitAt) -> Option<String> {
    git_stdout(at, &["rev-parse", "HEAD"])
        .await
        .filter(|s| !s.is_empty())
}

/// How many commits `HEAD` has beyond `origin/<base_branch>` (as fetched into
/// the mirror). `None` when it cannot be told.
pub(crate) async fn commits_ahead(at: &GitAt, base_branch: &str) -> Option<u64> {
    let range = format!("refs/remotes/origin/{base_branch}..HEAD");
    git_stdout(at, &["rev-list", "--count", &range])
        .await?
        .parse()
        .ok()
}

/// The branch `HEAD` is on (`refs/heads/...`), `None` when it is detached.
pub(crate) async fn head_ref(at: &GitAt) -> Option<String> {
    git_stdout(at, &["symbolic-ref", "-q", "HEAD"])
        .await
        .filter(|s| !s.is_empty())
}

/// Put the worktree back the way it was: `HEAD` on `head_ref` (or detached at `head`) and at
/// `head`, the files exactly as `tree` holds them (what [`working_tree_id`] returned: tracked
/// changes and untracked files, minus what `.gitignore` excludes), and the index as `HEAD`'s.
///
/// Used after a command that is for looking changed something it should not have. `reset --hard` undoes
/// what it did to tracked files and to `HEAD`, `clean` removes what it created, and `read-tree
/// --reset -u` writes back what the worktree held before (uncommitted work included, files that
/// were untracked included). `true` when every step succeeded.
pub(crate) async fn restore_worktree(
    at: &GitAt,
    head: &str,
    head_ref: Option<&str>,
    tree: &str,
) -> bool {
    let point_head = match head_ref {
        Some(branch) => git_stdout(at, &["symbolic-ref", "HEAD", branch]).await,
        None => git_stdout(at, &["update-ref", "--no-deref", "HEAD", head]).await,
    };
    point_head.is_some()
        && git_stdout(at, &["reset", "--hard", "--quiet", head])
            .await
            .is_some()
        && git_stdout(at, &["clean", "-ffdq"]).await.is_some()
        && git_stdout(at, &["read-tree", "--reset", "-u", tree])
            .await
            .is_some()
        && git_stdout(at, &["reset", "--quiet", head]).await.is_some()
}

/// What `git status` says, as it says it (`--porcelain=v1 -z`, every untracked file): a cheap
/// picture of the worktree that needs no commit and no index copy.
pub(crate) async fn status_text(at: &GitAt) -> Option<String> {
    git_stdout(
        at,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )
    .await
}

/// The parts of the repository that are shared with every run and that a model's command could
/// change without touching a file of the worktree: refs outside the namespaces that other runs
/// and git itself move, the local configuration, and `info/exclude`.
///
/// This is **a guard against accidents, not isolation**: the refs and the configuration are
/// shared by every run on the mirror, and other runs change them while a command runs, so what
/// they change is left out (a restore would undo their work). Not compared:
/// * `refs/heads/agent/*` (runs commit to them: so a command can still move or delete another
///   run's branch), `refs/remotes/*` (fetches write them), `refs/tags/*` (a fetch follows tags)
///   and `refs/stash` (a `git stash` in another worktree writes it);
/// * the configuration keys `branch.agent/*.adam-run`, `.remote` and `.merge`, which `prepare`
///   and `push` write for each run.
///
/// Compared, and undone when a command changed them: any other ref (a new branch, `refs/notes`,
/// a moved `main`), every other configuration key (`core.fsmonitor`, a `remote.origin.pushurl`, a
/// `url.*.insteadOf`, an alias) and `info/exclude` (which would hide files from what
/// `commit_and_push` commits). The credentialed commands of the workspace do not rely on this: they
/// clean the configuration themselves under the mirror lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoState {
    /// `(refname, object id)`.
    refs: Vec<(String, String)>,
    /// `(key, value)` of `git config --local`, by key, multi-valued keys repeated.
    config: Vec<(String, String)>,
    /// The content of `<common dir>/info/exclude`, `None` when there is no such file.
    exclude: Option<String>,
}

/// Whether a run other than this one changes `refname` as a matter of course.
fn moved_by_others(refname: &str) -> bool {
    refname.starts_with("refs/heads/agent/")
        || refname.starts_with("refs/remotes/")
        || refname.starts_with("refs/tags/")
        || refname == "refs/stash"
}

/// Whether `key` is one that `prepare` and `push` write for a run's own branch.
fn written_per_run(key: &str) -> bool {
    key.starts_with("branch.agent/")
        && [".adam-run", ".remote", ".merge"]
            .iter()
            .any(|suffix| key.ends_with(suffix))
}

/// `<common dir>/info/exclude`, where the repository's local ignore rules live.
async fn exclude_path(at: &GitAt) -> Option<std::path::PathBuf> {
    let common = git_stdout(at, &["rev-parse", "--git-common-dir"]).await?;
    Some(at.path().join(common).join("info/exclude"))
}

/// [`RepoState`] now, `None` when it cannot be read.
pub(crate) async fn repo_state(at: &GitAt) -> Option<RepoState> {
    let refs = git_stdout(at, &["for-each-ref", "--format=%(refname) %(objectname)"]).await?;
    let refs = refs
        .lines()
        .filter_map(|l| l.split_once(' '))
        .filter(|(name, _)| !moved_by_others(name))
        .map(|(n, id)| (n.to_owned(), id.to_owned()))
        .collect();
    let config = git_stdout(at, &["config", "--local", "--list", "-z"]).await?;
    let mut config: Vec<(String, String)> = config
        .split('\0')
        .filter(|e| !e.is_empty())
        .map(|e| match e.split_once('\n') {
            Some((k, v)) => (k.to_owned(), v.to_owned()),
            None => (e.to_owned(), String::new()),
        })
        .filter(|(k, _)| !written_per_run(k))
        .collect();
    // Where a key sits in the file does not matter (restoring it appends), what it says does; the
    // values of one key keep their order (the sort is stable).
    config.sort_by(|a, b| a.0.cmp(&b.0));
    let exclude = match tokio::fs::read_to_string(exclude_path(at).await?).await {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return None,
    };
    Some(RepoState {
        refs,
        config,
        exclude,
    })
}

/// Put the refs, the configuration and `info/exclude` that [`repo_state`] reads back as `before`
/// had them. `true` when every step succeeded. The caller holds the mirror lock
/// (`Worktree::lock_mirror`): the configuration is written by one at a time.
pub(crate) async fn restore_repo_state(at: &GitAt, before: &RepoState) -> bool {
    let Some(now) = repo_state(at).await else {
        return false;
    };
    let mut ok = true;
    // `--no-deref`: a ref that is a symbolic ref is itself what is deleted or set, never what it
    // points at.
    for (name, _) in &now.refs {
        if !before.refs.iter().any(|(n, _)| n == name) {
            ok &= git_stdout(at, &["update-ref", "--no-deref", "-d", name])
                .await
                .is_some();
        }
    }
    for (name, id) in &before.refs {
        if !now.refs.iter().any(|r| r == &(name.clone(), id.clone())) {
            ok &= git_stdout(at, &["update-ref", "--no-deref", name, id])
                .await
                .is_some();
        }
    }
    if now.config != before.config {
        let mut keys: Vec<&String> = now
            .config
            .iter()
            .chain(&before.config)
            .map(|(k, _)| k)
            .collect();
        keys.sort();
        keys.dedup();
        for key in keys {
            let was: Vec<&String> = before
                .config
                .iter()
                .filter(|(k, _)| k == key)
                .map(|(_, v)| v)
                .collect();
            let is: Vec<&String> = now
                .config
                .iter()
                .filter(|(k, _)| k == key)
                .map(|(_, v)| v)
                .collect();
            if was == is {
                continue;
            }
            // `--unset-all` fails when the key is not there, which is fine.
            let _ = git_stdout(at, &["config", "--local", "--unset-all", key]).await;
            for value in was {
                ok &= git_stdout(at, &["config", "--local", "--add", key, value])
                    .await
                    .is_some();
            }
        }
    }
    if now.exclude != before.exclude {
        let Some(path) = exclude_path(at).await else {
            return false;
        };
        ok &= match &before.exclude {
            Some(text) => tokio::fs::write(&path, text).await.is_ok(),
            None => tokio::fs::remove_file(&path).await.is_ok(),
        };
    }
    ok && repo_state(at).await.as_ref() == Some(before)
}

/// The tree id of `HEAD`: the code a pull request from this branch contains.
pub(crate) async fn head_tree(at: &GitAt) -> Option<String> {
    git_stdout(at, &["rev-parse", "HEAD^{tree}"])
        .await
        .filter(|s| !s.is_empty())
}

/// The tree id of the worktree's current content, exactly as `commit_all`
/// (`git add -A`, then commit) would record it: tracked and untracked files,
/// minus what `.gitignore` excludes. Computed in a temporary copy of the
/// index, so the real index is untouched. `None` when it cannot be computed.
///
/// The index is in a directory the run's commands write. It is read as a regular file and not
/// through a link, copied into a directory of the coder's own outside the volume (a name nobody
/// chose, made with `O_EXCL`), and `git` works on that copy (`GIT_INDEX_FILE`): nothing is made
/// in the run's directory, so no name there can be a link to somewhere else, and a link in place
/// of the index copies nothing.
pub(crate) async fn working_tree_id(at: &GitAt) -> Option<String> {
    let index = at.index_path().await?;
    let copy = match adam_workspace::safe_fs::private_copy_of(
        &index,
        adam_workspace::safe_fs::MAX_INDEX_COPY,
    )
    .await
    {
        Ok(copy) => copy,
        Err(e) => {
            tracing::warn!(index = %index.display(), error = %e, "the index is not copied");
            return None;
        }
    };
    let run = |args: &'static [&'static str]| {
        let tmp = copy.path().to_owned();
        async move {
            let mut cmd = at.command().await?;
            let out = cmd
                .args(args)
                .env("GIT_INDEX_FILE", &tmp)
                .output()
                .await
                .ok()?;
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        }
    };
    run(&["add", "-A"]).await?;
    run(&["write-tree"]).await.filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::time::SystemTime;

    use super::*;

    /// Run `git` in `dir` with the index at `index` (if any); panic with its stderr on failure.
    fn git(dir: &Path, index: Option<&Path>, args: &[&str]) -> String {
        let mut cmd = Command::new("git");
        cmd.current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com");
        if let Some(index) = index {
            cmd.env("GIT_INDEX_FILE", index);
        }
        let out = cmd.output().expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn set_mtime(path: &Path, at: SystemTime) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(at)
            .unwrap();
    }

    /// A file rewritten with the same size in the same clock tick as the index was written looks
    /// unchanged by its stat data. Git protects against that ("racily clean": an entry as new as
    /// the index is read again), and the copy of the index the tree id is computed in must keep
    /// the protection: stamped with the time it was made it would make the entry look old, and
    /// the tree would hold the old content. (Seen as a flaky test under load: the coarse clock of
    /// a busy machine makes the two writes share a tick.)
    #[tokio::test]
    async fn a_same_size_rewrite_within_the_index_tick_is_in_the_tree_id() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        git(dir, None, &["init", "--quiet", "--initial-branch=main"]);
        // The stat data of the ctime changes whatever we do; only mtime, size and the rest are
        // what a coarse clock leaves equal.
        git(dir, None, &["config", "core.trustctime", "false"]);
        std::fs::write(dir.join("README.md"), "widgets\n").unwrap();
        git(dir, None, &["add", "-A"]);
        git(dir, None, &["commit", "--quiet", "-m", "seed"]);

        // The index entry has the file's mtime of that moment. Rewrite the file with the same
        // size and put that mtime back, and let the index be as old as the entry.
        let tick = std::fs::metadata(dir.join("README.md"))
            .unwrap()
            .modified()
            .unwrap();
        std::fs::write(dir.join("README.md"), "changed\n").unwrap();
        set_mtime(&dir.join("README.md"), tick);
        set_mtime(&dir.join(".git/index"), tick);

        // What `git add -A` would record, from an index that has no stat data to trust.
        let fresh = dir.join(".git/index.fresh");
        git(dir, Some(&fresh), &["read-tree", "HEAD"]);
        git(dir, Some(&fresh), &["add", "-A"]);
        let expected = git(dir, Some(&fresh), &["write-tree"]);
        let head = head_tree(&GitAt::plain(dir)).await.unwrap();
        assert_ne!(expected, head, "the rewrite is a change");

        // Git compares these timestamps in whole seconds, so the copy has to be made in a later
        // second than the entry's to look older than it.
        tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
        assert_eq!(working_tree_id(&GitAt::plain(dir)).await.unwrap(), expected);
    }

    /// Repository code can write a `filter.*.clean` command to `.git/config` (a check script can),
    /// and a committed `.gitattributes` makes the coder's own `git add` run it. The coder's git
    /// starts from an empty environment, so what the filter sees holds none of the coder's secrets.
    #[tokio::test]
    async fn a_filter_the_repository_configures_runs_without_the_coders_secrets() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo");
        std::fs::create_dir(&dir).unwrap();
        let seen = tmp.path().join("seen-by-filter");
        git(&dir, None, &["init", "--quiet", "--initial-branch=main"]);
        std::fs::write(dir.join("README.md"), "widgets\n").unwrap();
        git(&dir, None, &["add", "-A"]);
        git(&dir, None, &["commit", "--quiet", "-m", "seed"]);
        git(
            &dir,
            None,
            &[
                "config",
                "filter.spy.clean",
                &format!("sh -c 'env > {}; cat'", seen.display()),
            ],
        );
        std::fs::write(dir.join(".gitattributes"), "*.txt filter=spy\n").unwrap();
        std::fs::write(dir.join("new.txt"), "to be filtered\n").unwrap();

        // `working_tree_id` runs `git add -A` in a copy of the index: the filter runs.
        assert!(working_tree_id(&GitAt::plain(&dir)).await.is_some());
        let env = std::fs::read_to_string(&seen).expect("the filter ran");
        // `CARGO_PKG_NAME` is set by cargo in every test process: it stands for a secret.
        assert!(!env.contains("CARGO_PKG_NAME"), "{env}");
        assert!(env.contains("PATH="), "what git needs is kept: {env}");
    }
}

#[cfg(test)]
mod pinned_tests {
    use std::sync::Arc;

    use adam_workspace::{RepoRef, StaticToken, Workspaces};

    use super::*;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// A worktree of a local repository, and a repository whose configuration runs `spy` as a clean
    /// filter (its marker is `marker`), the way a run's commands could make one.
    struct Fixture {
        _tmp: tempfile::TempDir,
        slot: adam_workspace::Slot,
        planted: PathBuf,
        marker: PathBuf,
    }

    async fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let remote = tmp.path().join("remote.git");
        let seed = tmp.path().join("seed");
        std::fs::create_dir_all(&remote).unwrap();
        std::fs::create_dir_all(&seed).unwrap();
        git(
            &remote,
            &["init", "--bare", "--quiet", "--initial-branch=main"],
        );
        git(&seed, &["init", "--quiet", "--initial-branch=main"]);
        std::fs::write(seed.join("README.md"), "widgets\n").unwrap();
        git(&seed, &["add", "-A"]);
        git(&seed, &["commit", "--quiet", "-m", "seed"]);
        git(
            &seed,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&seed, &["push", "--quiet", "origin", "main"]);
        let workspaces = Workspaces::new(
            tmp.path().join("root"),
            Arc::new(StaticToken::new("unused-in-these-tests")),
        );
        let slot = workspaces
            .run("run-aaaaaaaa-1")
            .unwrap()
            .add_repository(&RepoRef::new(remote.to_str().unwrap(), "main"))
            .await
            .unwrap();
        // The repository a command plants: a clean filter that leaves a marker.
        let marker = tmp.path().join("marker");
        let spy = tmp.path().join("spy.sh");
        std::fs::write(
            &spy,
            format!("#!/bin/sh\necho ran > {}\ncat\n", marker.display()),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&spy, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let planted = tmp.path().join("planted.git");
        git(
            tmp.path(),
            &["init", "--bare", "--quiet", planted.to_str().unwrap()],
        );
        git(
            &planted,
            &["config", "filter.spy.clean", spy.to_str().unwrap()],
        );
        std::fs::write(slot.path().join(".gitattributes"), "*.md filter=spy\n").unwrap();
        std::fs::write(slot.path().join("README.md"), "changed by the run\n").unwrap();
        Fixture {
            _tmp: tmp,
            slot,
            planted,
            marker,
        }
    }

    /// The questions the coder asks of a worktree that read the index, run `add -A` in a copy of it
    /// and read the status: all of them.
    async fn ask_everything(at: &GitAt) -> (Option<String>, Option<String>, Option<String>) {
        (
            working_tree_id(at).await,
            status_text(at).await,
            head_sha(at).await,
        )
    }

    #[tokio::test]
    async fn the_coders_questions_run_nothing_a_rewritten_commondir_or_git_file_names() {
        let f = fixture().await;
        let admin = f.slot.worktree().unwrap().git_dir().unwrap().to_owned();
        let at = GitAt::slot(&f.slot);

        // The control: a plain directory is discovered through the files, and the filter runs.
        std::fs::write(
            admin.join("commondir"),
            format!("{}\n", f.planted.display()),
        )
        .unwrap();
        assert!(
            working_tree_id(&GitAt::plain(f.slot.path()))
                .await
                .is_some()
        );
        assert!(f.marker.exists(), "the control ran the planted filter");
        std::fs::remove_file(&f.marker).unwrap();

        // The pinned slot: refused (git's reference store follows this file even when the common
        // directory is named), nothing runs, and the file is left as it is.
        assert_eq!(ask_everything(&at).await, (None, None, None));
        assert!(!f.marker.exists(), "commondir rewritten");
        assert_eq!(
            std::fs::read_to_string(admin.join("commondir")).unwrap(),
            format!("{}\n", f.planted.display()),
            "refused, not rewritten"
        );
        std::fs::write(admin.join("commondir"), "../..\n").unwrap();
        let (tree, status, head) = ask_everything(&at).await;
        assert!(tree.is_some() && head.is_some());
        assert!(status.unwrap().contains("README.md"));
        assert!(!f.marker.exists(), "commondir as git wrote it");

        // The slot's `.git` file naming a repository with a working tree of its own.
        let project = f._tmp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        git(&project, &["init", "--quiet", "--initial-branch=main"]);
        git(
            &project,
            &[
                "config",
                "filter.spy.clean",
                f.marker.parent().unwrap().join("spy.sh").to_str().unwrap(),
            ],
        );
        git(&project, &["commit", "--allow-empty", "--quiet", "-m", "x"]);
        std::fs::write(
            f.slot.path().join(".git"),
            format!("gitdir: {}\n", project.join(".git").display()),
        )
        .unwrap();
        assert!(
            working_tree_id(&GitAt::plain(f.slot.path()))
                .await
                .is_some()
        );
        assert!(f.marker.exists(), "the control ran the planted filter");
        std::fs::remove_file(&f.marker).unwrap();
        std::fs::write(
            f.slot.path().join("README.md"),
            "changed once more, longer\n",
        )
        .unwrap();
        let (tree, status, head) = ask_everything(&at).await;
        assert!(tree.is_some() && head.is_some() && status.unwrap().contains("README.md"));
        assert!(!f.marker.exists(), ".git file rewritten");
    }

    const VICTIM: &str = "not the coder's to write\n";

    fn victim(f: &Fixture, name: &str) -> PathBuf {
        let path = f._tmp.path().join(name);
        std::fs::write(&path, VICTIM).unwrap();
        path
    }

    #[tokio::test]
    async fn the_copy_of_the_index_is_made_outside_the_directory_a_run_writes() {
        let f = fixture().await;
        let admin = f.slot.worktree().unwrap().git_dir().unwrap().to_owned();
        let secret = victim(&f, "app-key.pem");
        // The names the copy used to have: `index.adam-tree-<pid>-<n>`, planted as links.
        for n in 0..256 {
            let link = admin.join(format!("index.adam-tree-{}-{n}", std::process::id()));
            std::os::unix::fs::symlink(&secret, link).unwrap();
        }
        let at = GitAt::slot(&f.slot);
        assert!(working_tree_id(&at).await.is_some());
        assert_eq!(std::fs::read_to_string(&secret).unwrap(), VICTIM);
        // And nothing new is made in the directory.
        let made = std::fs::read_dir(&admin)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("index.adam-")
            })
            .count();
        assert_eq!(made, 256, "only the planted links");
    }

    #[tokio::test]
    async fn an_index_that_is_a_link_or_a_pipe_is_not_copied_from() {
        let f = fixture().await;
        let admin = f.slot.worktree().unwrap().git_dir().unwrap().to_owned();
        let at = GitAt::slot(&f.slot);
        let secret = victim(&f, "app-key.pem");
        std::fs::remove_file(admin.join("index")).unwrap();
        std::os::unix::fs::symlink(&secret, admin.join("index")).unwrap();
        assert_eq!(working_tree_id(&at).await, None);
        assert_eq!(std::fs::read_to_string(&secret).unwrap(), VICTIM);

        std::fs::remove_file(admin.join("index")).unwrap();
        assert!(
            std::process::Command::new("mkfifo")
                .arg(admin.join("index"))
                .status()
                .unwrap()
                .success()
        );
        let asked = tokio::time::timeout(std::time::Duration::from_secs(10), working_tree_id(&at))
            .await
            .expect("the coder waited on a pipe");
        assert_eq!(asked, None);
    }

    #[tokio::test]
    async fn a_dot_git_that_became_a_link_is_replaced_and_never_written_through() {
        use super::super::inspect::Snapshot;

        let f = fixture().await;
        let at = GitAt::slot(&f.slot);
        let before = Snapshot::take(&at).await.unwrap();
        let text = std::fs::read_to_string(f.slot.path().join(".git")).unwrap();

        // A command replaces the slot's `.git` by a link to a file the coder can write.
        let secret = victim(&f, "app-key.pem");
        std::fs::remove_file(f.slot.path().join(".git")).unwrap();
        std::os::unix::fs::symlink(&secret, f.slot.path().join(".git")).unwrap();
        let during = Snapshot::take(&at).await.unwrap();
        assert!(!during.same_git_as(&before), "the change is seen");
        assert_eq!(
            std::fs::read_to_string(&secret).unwrap(),
            VICTIM,
            "and nothing is read through the link to be put back later"
        );

        before.restore(&f.slot).await;
        assert_eq!(std::fs::read_to_string(&secret).unwrap(), VICTIM);
        let dot_git = f.slot.path().join(".git");
        assert!(
            !std::fs::symlink_metadata(&dot_git)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&dot_git).unwrap(), text);
    }

    #[tokio::test]
    async fn a_worktree_whose_directory_in_the_mirror_is_not_known_is_not_asked_at_all() {
        let f = fixture().await;
        let at = GitAt::slot(&f.slot);
        assert!(head_sha(&at).await.is_some());
        // The same slot as the coder finds it when its metadata cannot say: no pin.
        let refused = GitAt::of(f.slot.path(), None);
        assert_eq!(ask_everything(&refused).await, (None, None, None));
        assert!(!f.marker.exists());
    }
}

#[cfg(test)]
mod state_tests {
    use super::*;

    #[test]
    fn what_other_runs_move_is_not_compared() {
        for moved in [
            "refs/heads/agent/abc",
            "refs/remotes/origin/main",
            "refs/tags/v1",
            "refs/stash",
        ] {
            assert!(moved_by_others(moved), "{moved}");
        }
        for ours in [
            "refs/heads/main",
            "refs/heads/keep",
            "refs/notes/x",
            "refs/stash2",
        ] {
            assert!(!moved_by_others(ours), "{ours}");
        }
        for key in [
            "branch.agent/01a0.adam-run",
            "branch.agent/01a0.remote",
            "branch.agent/01a0.merge",
        ] {
            assert!(written_per_run(key), "{key}");
        }
        // Another branch's keys are compared: a model can point `main` at another remote.
        for key in [
            "branch.main.remote",
            "branch.agent/x.pushremote",
            "remote.origin.pushurl",
        ] {
            assert!(!written_per_run(key), "{key}");
        }
    }
}
