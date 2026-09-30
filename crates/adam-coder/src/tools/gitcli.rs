//! The few read-only `git` questions the tools ask that `adam-workspace` does
//! not answer. The worktree's git directory is discovered from its path; no
//! credentials are involved.

use std::path::Path;
use std::process::Stdio;

use tokio::process::Command;

/// Run `git <args>` in `dir`; `Some(stdout)` (trimmed) on exit 0.
pub(crate) async fn git_stdout(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(["-c", "core.hooksPath=/dev/null"])
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
/// Used after a read-only command changed something it should not have. `reset --hard` undoes
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
                let out = Command::new("git")
                    .args(["-c", "core.hooksPath=/dev/null"])
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
}
