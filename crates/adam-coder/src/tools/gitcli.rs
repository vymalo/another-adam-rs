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
