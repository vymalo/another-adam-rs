//! Worktree behaviour against local bare repositories (no network).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use adam_workspace::{
    FileStatus, GitCredentials, GitIdentity, RepoRef, StaticToken, WorkspaceError, Workspaces,
};
use base64::Engine as _;
use tempfile::TempDir;

/// Run git hermetically; panic with its stderr on failure.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "Seed")
        .env("GIT_AUTHOR_EMAIL", "seed@example.com")
        .env("GIT_COMMITTER_NAME", "Seed")
        .env("GIT_COMMITTER_EMAIL", "seed@example.com")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

const TOKEN: &str = "ghp_FAKEtoken0123456789abcdefghijklmnop";

struct Env {
    _tmp: TempDir,
    remote: PathBuf,
    root: PathBuf,
    repo: RepoRef,
    ws: Workspaces,
}

impl Env {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let remote = tmp.path().join("remote.git");
        let seed = tmp.path().join("seed");
        std::fs::create_dir_all(&remote).unwrap();
        std::fs::create_dir_all(&seed).unwrap();
        git(
            &remote,
            &["init", "--bare", "--quiet", "--initial-branch=main"],
        );
        git(&seed, &["init", "--quiet", "--initial-branch=main"]);
        std::fs::write(seed.join("README.md"), "hello\n").unwrap();
        std::fs::create_dir_all(seed.join("src")).unwrap();
        std::fs::write(seed.join("src/lib.rs"), "fn main() {}\n").unwrap();
        git(&seed, &["add", "-A"]);
        git(&seed, &["commit", "--quiet", "-m", "seed"]);
        git(
            &seed,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&seed, &["push", "--quiet", "origin", "main"]);

        let root = tmp.path().join("workspaces");
        let repo = RepoRef::new(remote.to_str().unwrap(), "main");
        let ws = Workspaces::new(root.clone(), Arc::new(StaticToken::new(TOKEN)));
        Self {
            _tmp: tmp,
            remote,
            root,
            repo,
            ws,
        }
    }

    fn mirror(&self) -> PathBuf {
        let loc = self.repo.locate().unwrap();
        self.root.join(loc.mirror_relative())
    }

    /// A push from a different clone, to make the remote move.
    fn advance_remote(&self, file: &str) {
        let clone = self._tmp.path().join(format!("other-{file}"));
        git(
            self._tmp.path(),
            &[
                "clone",
                "--quiet",
                self.remote.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        std::fs::write(clone.join(file), "x\n").unwrap();
        git(&clone, &["add", "-A"]);
        git(&clone, &["commit", "--quiet", "-m", "advance"]);
        git(&clone, &["push", "--quiet", "origin", "main"]);
    }
}

fn me() -> GitIdentity {
    GitIdentity::new("Adam Agent", "adam@example.com")
}

#[tokio::test]
async fn two_runs_get_two_isolated_worktrees_on_two_branches() {
    let env = Env::new();
    let a = env.ws.prepare(&env.repo, "run-aaaaaaaa-1").await.unwrap();
    let b = env.ws.prepare(&env.repo, "run-bbbbbbbb-2").await.unwrap();

    assert_ne!(a.path(), b.path());
    assert_eq!(a.path(), env.root.join("worktrees/run-aaaaaaaa-1"));
    assert_eq!(a.branch(), "agent/run-aaaa");
    assert_eq!(b.branch(), "agent/run-bbbb");
    assert_eq!(
        git(a.path(), &["symbolic-ref", "--short", "HEAD"]),
        a.branch()
    );
    assert_eq!(
        git(b.path(), &["symbolic-ref", "--short", "HEAD"]),
        b.branch()
    );
    // Both start from origin/main.
    assert_eq!(
        git(a.path(), &["rev-parse", "HEAD"]),
        git(&env.remote, &["rev-parse", "refs/heads/main"])
    );
    assert!(a.path().join("src/lib.rs").exists());

    // Edits in one are invisible in the other.
    std::fs::write(a.path().join("only-in-a.txt"), "a\n").unwrap();
    std::fs::write(a.path().join("README.md"), "changed in a\n").unwrap();
    assert!(!b.path().join("only-in-a.txt").exists());
    assert_eq!(
        std::fs::read_to_string(b.path().join("README.md")).unwrap(),
        "hello\n"
    );
    assert!(b.status().await.unwrap().is_empty());
    assert_eq!(a.status().await.unwrap().len(), 2);
}

#[tokio::test]
async fn commit_all_and_push_put_the_branch_on_the_remote() {
    let env = Env::new();
    let wt = env.ws.prepare(&env.repo, "run-commit-1").await.unwrap();

    assert_eq!(wt.commit_all("nothing", &me()).await.unwrap(), None);

    std::fs::write(wt.path().join("new.txt"), "new\n").unwrap();
    std::fs::write(wt.path().join("src/lib.rs"), "fn main() { println!(); }\n").unwrap();
    let sha = wt
        .commit_all("add things", &me())
        .await
        .unwrap()
        .expect("changes were committed");
    assert_eq!(sha.len(), 40);
    assert_eq!(git(wt.path(), &["rev-parse", "HEAD"]), sha);
    assert_eq!(
        git(wt.path(), &["log", "-1", "--format=%an <%ae>|%cn|%s"]),
        "Adam Agent <adam@example.com>|Adam Agent|add things"
    );
    assert_eq!(wt.commit_all("again", &me()).await.unwrap(), None);
    assert!(wt.status().await.unwrap().is_empty());

    wt.push().await.unwrap();
    assert_eq!(
        git(
            &env.remote,
            &["rev-parse", &format!("refs/heads/{}", wt.branch())]
        ),
        sha
    );
    // main is untouched.
    assert_ne!(git(&env.remote, &["rev-parse", "refs/heads/main"]), sha);
    // Upstream is set, so plain `git status`/`git pull` know where to look.
    assert_eq!(
        git(wt.path(), &["rev-parse", "--abbrev-ref", "@{upstream}"]),
        format!("origin/{}", wt.branch())
    );

    // Re-pushing the same sha is a no-op, not an error.
    wt.push().await.unwrap();

    // A second commit fast-forwards.
    std::fs::write(wt.path().join("more.txt"), "more\n").unwrap();
    let sha2 = wt.commit_all("more", &me()).await.unwrap().unwrap();
    wt.push().await.unwrap();
    assert_eq!(
        git(
            &env.remote,
            &["rev-parse", &format!("refs/heads/{}", wt.branch())]
        ),
        sha2
    );
}

#[tokio::test]
async fn diverged_remote_branch_is_rejected_without_force() {
    let env = Env::new();
    let wt = env.ws.prepare(&env.repo, "run-diverge-1").await.unwrap();
    std::fs::write(wt.path().join("a.txt"), "a\n").unwrap();
    wt.commit_all("a", &me()).await.unwrap().unwrap();
    wt.push().await.unwrap();

    // Someone else rewrites the branch on the remote.
    let other = env._tmp.path().join("other");
    git(
        env._tmp.path(),
        &[
            "clone",
            "--quiet",
            env.remote.to_str().unwrap(),
            other.to_str().unwrap(),
        ],
    );
    git(&other, &["checkout", "--quiet", "-b", "tmp", "origin/main"]);
    std::fs::write(other.join("b.txt"), "b\n").unwrap();
    git(&other, &["add", "-A"]);
    git(&other, &["commit", "--quiet", "-m", "b"]);
    git(
        &other,
        &[
            "push",
            "--quiet",
            "--force",
            "origin",
            &format!("tmp:refs/heads/{}", wt.branch()),
        ],
    );

    std::fs::write(wt.path().join("c.txt"), "c\n").unwrap();
    wt.commit_all("c", &me()).await.unwrap().unwrap();
    let err = wt.push().await.unwrap_err();
    assert!(matches!(err, WorkspaceError::Invalid(_)), "{err:?}");
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn prepare_is_idempotent_and_keeps_uncommitted_work() {
    let env = Env::new();
    let first = env.ws.prepare(&env.repo, "run-idem-1").await.unwrap();
    std::fs::write(first.path().join("wip.txt"), "work in progress\n").unwrap();

    // The remote moves on; a reused worktree must not be touched or re-based.
    env.advance_remote("later.txt");
    let head_before = git(first.path(), &["rev-parse", "HEAD"]);

    let second = env.ws.prepare(&env.repo, "run-idem-1").await.unwrap();
    assert_eq!(first.path(), second.path());
    assert_eq!(first.branch(), second.branch());
    assert_eq!(git(second.path(), &["rev-parse", "HEAD"]), head_before);
    assert_eq!(
        std::fs::read_to_string(second.path().join("wip.txt")).unwrap(),
        "work in progress\n"
    );
    let worktrees = git(&env.mirror(), &["worktree", "list", "--porcelain"]);
    assert_eq!(
        worktrees.matches("\nworktree ").count() + 1,
        2,
        "{worktrees}"
    );

    // A new run starts from the moved remote.
    let third = env.ws.prepare(&env.repo, "run-idem-2").await.unwrap();
    assert!(third.path().join("later.txt").exists());
}

#[tokio::test]
async fn worktrees_survive_a_process_restart() {
    let env = Env::new();
    let wt = env.ws.prepare(&env.repo, "run-restart-1").await.unwrap();
    std::fs::write(wt.path().join("wip.txt"), "wip\n").unwrap();
    let sha = {
        std::fs::write(wt.path().join("committed.txt"), "c\n").unwrap();
        wt.commit_all("c", &me()).await.unwrap().unwrap()
    };
    std::fs::write(wt.path().join("wip.txt"), "uncommitted\n").unwrap();
    drop(wt);

    // A brand-new Workspaces over the same root, as after a restart.
    let ws2 = Workspaces::new(env.root.clone(), Arc::new(StaticToken::new(TOKEN)));
    assert!(ws2.open_existing("never-prepared").await.unwrap().is_none());
    let wt = ws2
        .open_existing("run-restart-1")
        .await
        .unwrap()
        .expect("found again");
    assert_eq!(wt.repo(), &env.repo);
    assert_eq!(wt.run(), "run-restart-1");
    assert_eq!(git(wt.path(), &["rev-parse", "HEAD"]), sha);
    assert_eq!(
        std::fs::read_to_string(wt.path().join("wip.txt")).unwrap(),
        "uncommitted\n"
    );
    // ... and it is fully usable: push works from the recovered handle.
    wt.push().await.unwrap();
    assert_eq!(
        git(
            &env.remote,
            &["rev-parse", &format!("refs/heads/{}", wt.branch())]
        ),
        sha
    );
    // prepare for the same run agrees.
    let again = ws2.prepare(&env.repo, "run-restart-1").await.unwrap();
    assert_eq!(again.path(), wt.path());
}

#[tokio::test]
async fn a_lost_worktree_directory_is_recreated_from_its_branch() {
    let env = Env::new();
    let wt = env.ws.prepare(&env.repo, "run-lost-1").await.unwrap();
    std::fs::write(wt.path().join("kept.txt"), "kept\n").unwrap();
    let sha = wt.commit_all("keep", &me()).await.unwrap().unwrap();
    std::fs::remove_dir_all(wt.path()).unwrap();
    assert!(env.ws.open_existing("run-lost-1").await.unwrap().is_none());

    let back = env.ws.prepare(&env.repo, "run-lost-1").await.unwrap();
    assert_eq!(back.branch(), wt.branch());
    assert_eq!(git(back.path(), &["rev-parse", "HEAD"]), sha);
    assert!(back.path().join("kept.txt").exists());
}

#[tokio::test]
async fn remove_deletes_the_worktree_keeps_the_branch_and_is_idempotent() {
    let env = Env::new();
    let wt = env.ws.prepare(&env.repo, "run-rm-0001").await.unwrap();
    std::fs::write(wt.path().join("x.txt"), "x\n").unwrap();
    let sha = wt.commit_all("x", &me()).await.unwrap().unwrap();
    let branch = wt.branch().to_owned();

    env.ws.remove("run-rm-0001").await.unwrap();
    env.ws.remove("run-rm-0001").await.unwrap();
    env.ws.remove("never-existed").await.unwrap();

    assert!(!wt.path().exists());
    assert!(env.ws.open_existing("run-rm-0001").await.unwrap().is_none());
    let list = git(&env.mirror(), &["worktree", "list", "--porcelain"]);
    assert!(!list.contains("run-rm-0001"), "{list}");
    // The unpushed commit is still reachable from the run's branch.
    assert_eq!(
        git(
            &env.mirror(),
            &["rev-parse", &format!("refs/heads/{branch}")]
        ),
        sha
    );

    // Preparing the run again re-attaches that branch.
    let again = env.ws.prepare(&env.repo, "run-rm-0001").await.unwrap();
    assert_eq!(again.branch(), branch);
    assert_eq!(git(again.path(), &["rev-parse", "HEAD"]), sha);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn eight_concurrent_prepares_succeed_and_the_mirror_is_clean() {
    let env = Env::new();
    // UUIDv7-style ids created close together share their first 8 characters,
    // so the branch names must be disambiguated.
    let runs: Vec<String> = (0..8)
        .map(|i| format!("018f3a2b-7c1d-7000-8000-00000000000{i}"))
        .collect();

    let mut tasks = Vec::new();
    for run in &runs {
        let ws = env.ws.clone();
        let repo = env.repo.clone();
        let run = run.clone();
        tasks.push(tokio::spawn(async move { ws.prepare(&repo, &run).await }));
    }
    let mut worktrees = Vec::new();
    for t in tasks {
        worktrees.push(t.await.unwrap().expect("prepare succeeds"));
    }

    let mut branches: Vec<&str> = worktrees.iter().map(|w| w.branch()).collect();
    branches.sort_unstable();
    branches.dedup();
    assert_eq!(branches.len(), 8, "distinct branches: {branches:?}");
    assert!(branches.iter().all(|b| b.starts_with("agent/018f3a2b")));
    for w in &worktrees {
        assert!(w.path().join("README.md").exists());
        assert_eq!(
            git(w.path(), &["symbolic-ref", "--short", "HEAD"]),
            w.branch()
        );
    }
    // Idempotent afterwards, with the same names.
    for (w, run) in worktrees.iter().zip(&runs) {
        assert_eq!(
            env.ws.prepare(&env.repo, run).await.unwrap().branch(),
            w.branch()
        );
    }

    let mirror = env.mirror();
    git(&mirror, &["fsck", "--strict", "--no-dangling"]);
    let list = git(&mirror, &["worktree", "list", "--porcelain"]);
    assert_eq!(list.matches("worktree ").count(), 9, "bare + 8: {list}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_prepare_push_and_remove_do_not_interfere() {
    let env = Env::new();
    let mut tasks = Vec::new();
    for i in 0..6 {
        let ws = env.ws.clone();
        let repo = env.repo.clone();
        tasks.push(tokio::spawn(async move {
            let run = format!("mixed-{i:04}");
            let wt = ws.prepare(&repo, &run).await?;
            std::fs::write(wt.path().join(format!("f{i}.txt")), format!("{i}\n")).unwrap();
            wt.commit_all("work", &me()).await?;
            wt.push().await?;
            if i % 2 == 0 {
                ws.remove(&run).await?;
            }
            Ok::<_, WorkspaceError>(wt.branch().to_owned())
        }));
    }
    for t in tasks {
        let branch = t.await.unwrap().expect("task succeeds");
        git(&env.remote, &["rev-parse", &format!("refs/heads/{branch}")]);
    }
    git(&env.mirror(), &["fsck", "--strict", "--no-dangling"]);
}

#[tokio::test]
async fn status_reports_every_kind_of_change() {
    let env = Env::new();
    let wt = env.ws.prepare(&env.repo, "run-status-1").await.unwrap();
    std::fs::write(wt.path().join("README.md"), "modified\n").unwrap();
    std::fs::remove_file(wt.path().join("src/lib.rs")).unwrap();
    std::fs::create_dir_all(wt.path().join("dir with space")).unwrap();
    std::fs::write(wt.path().join("dir with space/new file.txt"), "n\n").unwrap();

    let mut status: Vec<(String, FileStatus)> = wt
        .status()
        .await
        .unwrap()
        .into_iter()
        .map(|c| (c.path, c.status))
        .collect();
    status.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        status,
        vec![
            ("README.md".to_owned(), FileStatus::Modified),
            (
                "dir with space/new file.txt".to_owned(),
                FileStatus::Untracked
            ),
            ("src/lib.rs".to_owned(), FileStatus::Deleted),
        ]
    );
}

#[tokio::test]
async fn diff_stat_spans_commits_and_uncommitted_changes_without_touching_the_index() {
    let env = Env::new();
    let wt = env.ws.prepare(&env.repo, "run-diff-001").await.unwrap();
    assert_eq!(wt.diff_stat().await.unwrap(), "");

    std::fs::write(wt.path().join("committed.txt"), "c\n").unwrap();
    wt.commit_all("c", &me()).await.unwrap().unwrap();
    std::fs::write(wt.path().join("README.md"), "changed\n").unwrap();
    std::fs::write(wt.path().join("untracked.txt"), "u\n").unwrap();

    // The remote moves on: the diff is against the merge-base, so it must not
    // show the other side's change (the mirror is only updated by prepare, but
    // this guards the merge-base logic all the same).
    env.advance_remote("theirs.txt");

    let stat = wt.diff_stat().await.unwrap();
    for file in ["committed.txt", "README.md", "untracked.txt"] {
        assert!(stat.contains(file), "{file} missing from:\n{stat}");
    }
    assert!(stat.contains("3 files changed"), "{stat}");
    assert!(!stat.contains("theirs.txt"));

    // Nothing was staged by computing it, and no temp index is left behind.
    git(wt.path(), &["diff", "--cached", "--quiet"]);
    let gitdir = PathBuf::from(git(wt.path(), &["rev-parse", "--absolute-git-dir"]));
    let leftovers: Vec<_> = std::fs::read_dir(&gitdir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().contains("adam-diff"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    let untracked = wt.status().await.unwrap();
    assert!(
        untracked
            .iter()
            .any(|c| c.path == "untracked.txt" && c.status == FileStatus::Untracked)
    );
}

#[tokio::test]
async fn bad_inputs_are_reported_precisely() {
    let env = Env::new();

    // Missing base branch.
    let missing = RepoRef::new(env.repo.url.clone(), "no-such-branch");
    let err = env.ws.prepare(&missing, "run-err-0001").await.unwrap_err();
    assert!(matches!(err, WorkspaceError::NotFound(_)), "{err:?}");
    // Nothing half-made is left for the run.
    assert!(
        env.ws
            .open_existing("run-err-0001")
            .await
            .unwrap()
            .is_none()
    );
    assert!(!env.root.join("worktrees/run-err-0001").exists());

    // Missing remote.
    let nowhere = RepoRef::new(env.root.join("nowhere.git").to_str().unwrap(), "main");
    let err = env.ws.prepare(&nowhere, "run-err-0002").await.unwrap_err();
    assert!(matches!(err, WorkspaceError::NotFound(_)), "{err:?}");

    // Bad ids and branch names.
    for run in ["", "../escape", "a/b", ".hidden"] {
        let err = env.ws.prepare(&env.repo, run).await.unwrap_err();
        assert!(
            matches!(err, WorkspaceError::Invalid(_)),
            "{run:?}: {err:?}"
        );
    }
    for base in ["", "-x", "a..b", "with space"] {
        let repo = RepoRef::new(env.repo.url.clone(), base);
        let err = env.ws.prepare(&repo, "run-err-0003").await.unwrap_err();
        assert!(
            matches!(err, WorkspaceError::Invalid(_)),
            "{base:?}: {err:?}"
        );
    }
    let err = env
        .ws
        .prepare(
            &RepoRef::new("https://user:pw@github.com/o/r.git", "main"),
            "run-err-0004",
        )
        .await
        .unwrap_err();
    assert!(matches!(err, WorkspaceError::Invalid(_)), "{err:?}");
    assert!(!err.to_string().contains("pw@"), "{err}");

    // A run is bound to one repository.
    env.ws.prepare(&env.repo, "run-bound-01").await.unwrap();
    let other = RepoRef::new("https://github.com/o/r.git", "main");
    let err = env.ws.prepare(&other, "run-bound-01").await.unwrap_err();
    assert!(matches!(err, WorkspaceError::Conflict(_)), "{err:?}");

    // A squatter directory is not silently overwritten.
    std::fs::create_dir_all(env.root.join("worktrees/run-squat-01")).unwrap();
    std::fs::write(env.root.join("worktrees/run-squat-01/precious"), "p").unwrap();
    let err = env.ws.prepare(&env.repo, "run-squat-01").await.unwrap_err();
    assert!(matches!(err, WorkspaceError::Corrupt(_)), "{err:?}");
    assert!(env.root.join("worktrees/run-squat-01/precious").exists());
    env.ws.remove("run-squat-01").await.unwrap();
    env.ws.prepare(&env.repo, "run-squat-01").await.unwrap();

    let err = git_commit_with_bad_identity(&env).await;
    assert!(matches!(err, WorkspaceError::Invalid(_)), "{err:?}");
}

async fn git_commit_with_bad_identity(env: &Env) -> WorkspaceError {
    let wt = env.ws.prepare(&env.repo, "run-ident-01").await.unwrap();
    wt.commit_all("m", &GitIdentity::new("", "a@b"))
        .await
        .unwrap_err()
}

#[tokio::test]
async fn the_token_appears_in_no_file_and_no_error_message() {
    let env = Env::new();
    let mut messages: Vec<String> = Vec::new();
    let mut note = |e: &WorkspaceError| {
        messages.push(e.to_string());
        messages.push(format!("{e:?}"));
    };

    // Happy path: the token is used for prepare and push against a local remote.
    let wt = env.ws.prepare(&env.repo, "run-token-01").await.unwrap();
    std::fs::write(wt.path().join("t.txt"), "t\n").unwrap();
    wt.commit_all("t", &me()).await.unwrap().unwrap();
    wt.push().await.unwrap();

    // Error path 1: an http remote that refuses connections. Git gets the
    // header, fails, and its stderr must come back scrubbed.
    let dead = RepoRef::new("http://127.0.0.1:1/owner/repo.git", "main");
    let err = env.ws.prepare(&dead, "run-token-02").await.unwrap_err();
    assert!(
        err.is_retryable(),
        "connection refused is transient: {err:?}"
    );
    note(&err);

    // Error path 2: the remote disappears under a prepared worktree.
    let gone = env.remote.with_extension("moved");
    std::fs::rename(&env.remote, &gone).unwrap();
    std::fs::write(wt.path().join("t2.txt"), "t2\n").unwrap();
    wt.commit_all("t2", &me()).await.unwrap().unwrap();
    let err = wt.push().await.unwrap_err();
    note(&err);
    let err = env.ws.prepare(&env.repo, "run-token-03").await.unwrap_err();
    note(&err);
    std::fs::rename(&gone, &env.remote).unwrap();

    let b64 = base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{TOKEN}"));
    for m in &messages {
        assert!(!m.contains(TOKEN), "token leaked in error: {m}");
        assert!(
            !m.contains(b64.trim_end_matches('=')),
            "encoded token leaked in error: {m}"
        );
    }

    // Nothing under the root (mirror config, worktree .git files, metadata,
    // refs, objects) mentions the token in either form.
    let mut scanned = 0;
    let mut stack = vec![env.root.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                stack.push(path);
                continue;
            }
            let bytes = std::fs::read(&path).unwrap();
            scanned += 1;
            for needle in [TOKEN, b64.trim_end_matches('=')] {
                assert!(
                    !bytes.windows(needle.len()).any(|w| w == needle.as_bytes()),
                    "{} contains the token",
                    path.display()
                );
            }
        }
    }
    assert!(
        scanned > 20,
        "the scan should have covered a real repository, saw {scanned} files"
    );
    let config = std::fs::read_to_string(env.mirror().join("config")).unwrap();
    assert!(
        !config.contains("extraheader") && !config.contains("extraHeader"),
        "{config}"
    );
}

#[tokio::test]
async fn the_auth_header_is_sent_to_the_remote_and_scoped_to_its_origin() {
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let ws = Workspaces::new(tmp.path().join("root"), Arc::new(StaticToken::new(TOKEN)));
    let repo = RepoRef::new(format!("{}/owner/repo.git", server.uri()), "main");
    let err = ws.prepare(&repo, "run-hdr-0001").await.unwrap_err();
    assert!(matches!(err, WorkspaceError::NotFound(_)), "{err:?}");
    assert!(!err.to_string().contains(TOKEN));

    let requests = server.received_requests().await.unwrap();
    assert!(!requests.is_empty(), "git never reached the server");
    let want = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{TOKEN}"))
    );
    for r in &requests {
        let got = r
            .headers
            .get("authorization")
            .expect("every request carries the header")
            .to_str()
            .unwrap();
        assert_eq!(got, want);
    }
    // The mirror was created under host `127.0.0.1_<port>`, never the token.
    let loc = repo.locate().unwrap();
    assert!(loc.host.starts_with("127.0.0.1_"), "{}", loc.host);
    assert!(!loc.is_local());
}

#[tokio::test]
async fn credentials_errors_propagate() {
    struct Broken;
    #[async_trait::async_trait]
    impl GitCredentials for Broken {
        async fn token_for(&self, _: &RepoRef) -> Result<secrecy::SecretString, WorkspaceError> {
            Err(WorkspaceError::Auth("broker unavailable".to_owned()))
        }
    }
    let env = Env::new();
    let ws = Workspaces::new(env.root.join("other-root"), Arc::new(Broken));
    let err = ws.prepare(&env.repo, "run-cred-0001").await.unwrap_err();
    assert!(matches!(err, WorkspaceError::Auth(_)), "{err:?}");
}
