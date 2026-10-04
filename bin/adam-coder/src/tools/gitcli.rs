//! The few read-only `git` questions the tools ask that `adam-workspace` does
//! not answer. The worktree's git directory is discovered from its path; no
//! credentials are involved.

use std::path::Path;
use std::process::Stdio;

use tokio::process::Command;

/// Run `git <args>` in `dir`; `Some(stdout)` (trimmed) on exit 0.
pub(crate) async fn git_stdout(dir: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("git");
    // From an empty environment: a filter the repository configures runs in it (`git add` below).
    adam_workspace::confine_git_env(&mut cmd);
    let out = cmd
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// The commit `HEAD` points at.
pub(crate) async fn head_sha(dir: &Path) -> Option<String> {
    git_stdout(dir, &["rev-parse", "HEAD"])
        .await
        .filter(|s| !s.is_empty())
}

/// How many commits `HEAD` has beyond `origin/<base_branch>` (as fetched into
/// the mirror). `None` when it cannot be told.
pub(crate) async fn commits_ahead(dir: &Path, base_branch: &str) -> Option<u64> {
    let range = format!("refs/remotes/origin/{base_branch}..HEAD");
    git_stdout(dir, &["rev-list", "--count", &range])
        .await?
        .parse()
        .ok()
}

/// The branch `HEAD` is on (`refs/heads/...`), `None` when it is detached.
pub(crate) async fn head_ref(dir: &Path) -> Option<String> {
    git_stdout(dir, &["symbolic-ref", "-q", "HEAD"])
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
    dir: &Path,
    head: &str,
    head_ref: Option<&str>,
    tree: &str,
) -> bool {
    let point_head = match head_ref {
        Some(branch) => git_stdout(dir, &["symbolic-ref", "HEAD", branch]).await,
        None => git_stdout(dir, &["update-ref", "--no-deref", "HEAD", head]).await,
    };
    point_head.is_some()
        && git_stdout(dir, &["reset", "--hard", "--quiet", head])
            .await
            .is_some()
        && git_stdout(dir, &["clean", "-ffdq"]).await.is_some()
        && git_stdout(dir, &["read-tree", "--reset", "-u", tree])
            .await
            .is_some()
        && git_stdout(dir, &["reset", "--quiet", head]).await.is_some()
}

/// What `git status` says, as it says it (`--porcelain=v1 -z`, every untracked file): a cheap
/// picture of the worktree that needs no commit and no index copy.
pub(crate) async fn status_text(dir: &Path) -> Option<String> {
    git_stdout(
        dir,
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
async fn exclude_path(dir: &Path) -> Option<std::path::PathBuf> {
    let common = git_stdout(dir, &["rev-parse", "--git-common-dir"]).await?;
    Some(dir.join(common).join("info/exclude"))
}

/// [`RepoState`] now, `None` when it cannot be read.
pub(crate) async fn repo_state(dir: &Path) -> Option<RepoState> {
    let refs = git_stdout(dir, &["for-each-ref", "--format=%(refname) %(objectname)"]).await?;
    let refs = refs
        .lines()
        .filter_map(|l| l.split_once(' '))
        .filter(|(name, _)| !moved_by_others(name))
        .map(|(n, id)| (n.to_owned(), id.to_owned()))
        .collect();
    let config = git_stdout(dir, &["config", "--local", "--list", "-z"]).await?;
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
    let exclude = match tokio::fs::read_to_string(exclude_path(dir).await?).await {
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
pub(crate) async fn restore_repo_state(dir: &Path, before: &RepoState) -> bool {
    let Some(now) = repo_state(dir).await else {
        return false;
    };
    let mut ok = true;
    // `--no-deref`: a ref that is a symbolic ref is itself what is deleted or set, never what it
    // points at.
    for (name, _) in &now.refs {
        if !before.refs.iter().any(|(n, _)| n == name) {
            ok &= git_stdout(dir, &["update-ref", "--no-deref", "-d", name])
                .await
                .is_some();
        }
    }
    for (name, id) in &before.refs {
        if !now.refs.iter().any(|r| r == &(name.clone(), id.clone())) {
            ok &= git_stdout(dir, &["update-ref", "--no-deref", name, id])
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
            let _ = git_stdout(dir, &["config", "--local", "--unset-all", key]).await;
            for value in was {
                ok &= git_stdout(dir, &["config", "--local", "--add", key, value])
                    .await
                    .is_some();
            }
        }
    }
    if now.exclude != before.exclude {
        let Some(path) = exclude_path(dir).await else {
            return false;
        };
        ok &= match &before.exclude {
            Some(text) => tokio::fs::write(&path, text).await.is_ok(),
            None => tokio::fs::remove_file(&path).await.is_ok(),
        };
    }
    ok && repo_state(dir).await.as_ref() == Some(before)
}

/// The tree id of `HEAD`: the code a pull request from this branch contains.
pub(crate) async fn head_tree(dir: &Path) -> Option<String> {
    git_stdout(dir, &["rev-parse", "HEAD^{tree}"])
        .await
        .filter(|s| !s.is_empty())
}

/// The tree id of the worktree's current content, exactly as `commit_all`
/// (`git add -A`, then commit) would record it: tracked and untracked files,
/// minus what `.gitignore` excludes. Computed in a temporary copy of the
/// index, so the real index is untouched. `None` when it cannot be computed.
pub(crate) async fn working_tree_id(dir: &Path) -> Option<String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let index = git_stdout(dir, &["rev-parse", "--git-path", "index"]).await?;
    let index = dir.join(index);
    let tmp = index.with_extension(format!(
        "adam-tree-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    tokio::fs::copy(&index, &tmp).await.ok()?;
    keep_mtime(&index, &tmp).await?;
    let tree = async {
        let run = |args: &'static [&'static str]| {
            let tmp = tmp.clone();
            async move {
                let mut cmd = Command::new("git");
                adam_workspace::confine_git_env(&mut cmd);
                let out = cmd
                    .args([
                        "-c",
                        "core.hooksPath=/dev/null",
                        "-c",
                        "core.fsmonitor=false",
                    ])
                    .args(args)
                    .current_dir(dir)
                    .env("GIT_INDEX_FILE", &tmp)
                    .env("GIT_TERMINAL_PROMPT", "0")
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .env("LC_ALL", "C")
                    .stdin(Stdio::null())
                    .stderr(Stdio::null())
                    .kill_on_drop(true)
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
    .await;
    let _ = tokio::fs::remove_file(&tmp).await;
    tree
}

/// Give `copy` the modification time of `original`, an index file.
///
/// Git takes an index entry whose file looks unchanged (same size, same mtime) as unchanged,
/// except when the entry is as new as the index file itself: then it reads the file again
/// ("racily clean"), because the file may have been rewritten within one timestamp tick of the
/// index being written. A copy that is stamped with the time it was made makes every entry look
/// older than the index, so a file rewritten with the same size right after the checkout would
/// keep its old content in the tree computed from the copy. Keeping the original's mtime keeps
/// the protection. `None` when it cannot be done (the caller then has no tree id, which is
/// "unknown" and never a wrong one).
pub(crate) async fn keep_mtime(original: &Path, copy: &Path) -> Option<()> {
    let modified = tokio::fs::metadata(original).await.ok()?.modified().ok()?;
    let copy = copy.to_owned();
    tokio::task::spawn_blocking(move || {
        std::fs::File::options()
            .write(true)
            .open(copy)?
            .set_modified(modified)
    })
    .await
    .ok()?
    .ok()
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
        let head = head_tree(dir).await.unwrap();
        assert_ne!(expected, head, "the rewrite is a change");

        // Git compares these timestamps in whole seconds, so the copy has to be made in a later
        // second than the entry's to look older than it.
        tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
        assert_eq!(working_tree_id(dir).await.unwrap(), expected);
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
        assert!(working_tree_id(&dir).await.is_some());
        let env = std::fs::read_to_string(&seen).expect("the filter ran");
        // `CARGO_PKG_NAME` is set by cargo in every test process: it stands for a secret.
        assert!(!env.contains("CARGO_PKG_NAME"), "{env}");
        assert!(env.contains("PATH="), "what git needs is kept: {env}");
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
