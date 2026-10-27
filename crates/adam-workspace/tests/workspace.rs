//! Worktree behaviour against local bare repositories (no network).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use adam_error::Classify;
use adam_workspace::{
    FileStatus, GitCredentials, GitIdentity, RepoRef, ScopedToken, SlotKind, StaticToken,
    WorkspaceError, Workspaces, copy_into,
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

/// The remote's branch names, sorted.
fn remote_branches(env: &Env) -> Vec<String> {
    let mut names: Vec<String> = git(
        &env.remote,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads/"],
    )
    .lines()
    .map(str::to_owned)
    .collect();
    names.sort();
    names
}

/// A second run continues the branch a first one pushed: it starts from it. Its own commits go to
/// its own branch (`push`), and only `publish` moves the continued branch, so a pull request from
/// it is updated when the caller says so and not before.
#[tokio::test]
async fn a_run_can_continue_a_pushed_branch_and_publishes_to_it_only_on_request() {
    let env = Env::new();
    let first = env.ws.prepare(&env.repo, "run-first-0001").await.unwrap();
    std::fs::write(first.path().join("first.txt"), "one\n").unwrap();
    first.commit_all("first", &me()).await.unwrap().unwrap();
    first.push().await.unwrap();
    let pushed = first.branch().to_owned();
    assert_eq!(pushed, "agent/run-firs");
    assert_eq!(
        first.continues(),
        None,
        "a run on its own branch continues nothing"
    );
    assert_eq!(first.local_branch(), pushed);
    first.publish().await.unwrap();

    let second = env
        .ws
        .prepare_continuing(&env.repo, "run-second-002", &pushed)
        .await
        .unwrap();
    // It is the branch the pull request is opened from, and the work so far is in the worktree.
    assert_eq!(second.branch(), pushed);
    assert_eq!(second.continues(), Some(pushed.as_str()));
    assert_eq!(second.local_branch(), "agent/run-seco");
    assert_ne!(second.path(), first.path());
    assert_eq!(
        std::fs::read_to_string(second.path().join("first.txt")).unwrap(),
        "one\n"
    );
    let before = git(&env.remote, &["rev-parse", &format!("refs/heads/{pushed}")]);
    assert_eq!(git(second.path(), &["rev-parse", "HEAD"]), before);
    // What is checked out is the run's own branch: it cannot collide with the first worktree.
    assert_eq!(
        git(second.path(), &["symbolic-ref", "--short", "HEAD"]),
        "agent/run-seco"
    );
    // The diff against the base spans both runs' work.
    assert!(second.diff_stat().await.unwrap().contains("first.txt"));

    std::fs::write(second.path().join("second.txt"), "two\n").unwrap();
    let sha = second.commit_all("second", &me()).await.unwrap().unwrap();
    second.push().await.unwrap();
    // The commit is on the remote, on the run's own branch; the continued branch has not moved.
    assert_eq!(
        git(&env.remote, &["rev-parse", "refs/heads/agent/run-seco"]),
        sha
    );
    assert_eq!(
        git(&env.remote, &["rev-parse", &format!("refs/heads/{pushed}")]),
        before,
        "push leaves the continued branch where it was"
    );
    assert_eq!(
        remote_branches(&env),
        ["agent/run-firs", "agent/run-seco", "main"]
    );
    // Publishing moves it to the run's commit, as a fast-forward; repeating it is a no-op.
    second.publish().await.unwrap();
    second.publish().await.unwrap();
    assert_eq!(
        git(&env.remote, &["rev-parse", &format!("refs/heads/{pushed}")]),
        sha,
        "the pushed branch moved"
    );
    // Pushing again is a no-op, and the upstream is the run's own branch.
    second.push().await.unwrap();
    assert_eq!(
        git(second.path(), &["rev-parse", "--abbrev-ref", "@{upstream}"]),
        "origin/agent/run-seco"
    );
    // The first worktree is untouched.
    assert!(!first.path().join("second.txt").exists());
}

/// What carries the credentials goes where the workspace was prepared for, whatever was written in
/// the shared mirror's configuration afterwards: a `remote.origin.url` or `pushurl`, and the URL
/// rewrites (`insteadOf` and `pushInsteadOf` rewrite the URLs given on the command line too) that
/// a model's command, OpenCode or a repository script could plant. The configuration is made safe
/// under the mirror lock right before each credentialed command.
#[tokio::test]
async fn fetch_and_push_do_not_follow_the_mirrors_configuration() {
    let env = Env::new();
    let first = env.ws.prepare(&env.repo, "run-first-0001").await.unwrap();
    let bogus = env._tmp.path().join("nowhere.git");
    let evil = env._tmp.path().join("evil.git");
    std::fs::create_dir_all(&evil).unwrap();
    git(
        &evil,
        &["init", "--bare", "--quiet", "--initial-branch=main"],
    );
    let mirror = env.mirror();
    let remote = env.remote.to_str().unwrap().to_owned();
    let plant = |mirror: &std::path::Path| {
        for (key, value) in [
            ("remote.origin.url", bogus.to_str().unwrap()),
            ("remote.origin.pushurl", bogus.to_str().unwrap()),
            ("remote.evil.url", evil.to_str().unwrap()),
        ] {
            git(mirror, &["config", key, value]);
        }
        git(
            mirror,
            &[
                "config",
                &format!("url.{}.insteadOf", evil.display()),
                &remote,
            ],
        );
        git(
            mirror,
            &[
                "config",
                &format!("url.{}.pushInsteadOf", evil.display()),
                &remote,
            ],
        );
        git(mirror, &["config", "http.proxy", "http://127.0.0.1:9/"]);
        git(
            mirror,
            &["config", "core.fsmonitor", "touch /nonexistent/ran"],
        );
    };
    plant(&mirror);
    std::fs::write(first.path().join("f.txt"), "f\n").unwrap();
    first.commit_all("f", &me()).await.unwrap().unwrap();
    first.push().await.unwrap();
    assert_eq!(
        remote_branches(&env),
        [first.branch().to_owned(), "main".to_owned()],
        "the push reached the real remote"
    );
    assert_eq!(
        git(&evil, &["for-each-ref"]),
        "",
        "nothing was sent to the rewritten URL"
    );
    // The keys are gone, what the workspace itself writes stays.
    let config = git(&mirror, &["config", "--local", "--list"]);
    for gone in [
        "insteadof",
        "pushinsteadof",
        "pushurl",
        "remote.evil",
        "http.proxy",
        "fsmonitor",
    ] {
        assert!(!config.to_lowercase().contains(gone), "{gone} in {config}");
    }
    assert!(
        config.contains(&format!("remote.origin.url={remote}")),
        "{config}"
    );
    assert!(config.contains("remote.origin.fetch="), "{config}");
    assert!(
        config.contains(&format!("branch.{}.remote=origin", first.branch())),
        "{config}"
    );

    // A second run's fetch comes from the real remote too, and so does the default branch.
    plant(&mirror);
    env.advance_remote("later.txt");
    let second = env.ws.prepare(&env.repo, "run-second-002").await.unwrap();
    assert!(second.path().join("later.txt").exists());
    plant(&mirror);
    assert_eq!(env.ws.default_branch(&env.repo.url).await.unwrap(), "main");

    // And a continuing run's publish.
    let third = env
        .ws
        .prepare_continuing(&env.repo, "run-third-0003", first.branch())
        .await
        .unwrap();
    std::fs::write(third.path().join("g.txt"), "g\n").unwrap();
    third.commit_all("g", &me()).await.unwrap().unwrap();
    third.push().await.unwrap();
    plant(&mirror);
    third.publish().await.unwrap();
    assert_eq!(
        git(&env.remote, &["show", &format!("{}:g.txt", first.branch())]),
        "g"
    );
    assert_eq!(git(&evil, &["for-each-ref"]), "");
}

#[tokio::test]
async fn only_an_agent_branch_that_exists_can_be_continued() {
    let env = Env::new();
    let first = env.ws.prepare(&env.repo, "run-first-0001").await.unwrap();
    std::fs::write(first.path().join("f.txt"), "f\n").unwrap();
    first.commit_all("f", &me()).await.unwrap().unwrap();
    first.push().await.unwrap();
    let main_before = git(&env.remote, &["rev-parse", "refs/heads/main"]);

    // Somebody's branch, the base and a malformed name are refused before anything is fetched.
    git(
        &env.remote,
        &["branch", "people/feature", "refs/heads/main"],
    );
    for bad in [
        "main",
        "people/feature",
        "agent/",
        "agent",
        "agent/../main",
        "agent/a b",
        "agent/x..y",
        "-agent/x",
        "",
    ] {
        let err = env
            .ws
            .prepare_continuing(&env.repo, "run-bad-00001", bad)
            .await
            .unwrap_err();
        assert!(
            matches!(err, WorkspaceError::Invalid(_)),
            "{bad:?}: {err:?}"
        );
        assert!(!err.is_retryable());
    }
    // A well-formed agent branch that was never pushed is not found.
    let err = env
        .ws
        .prepare_continuing(&env.repo, "run-bad-00001", "agent/never-pushed")
        .await
        .unwrap_err();
    assert!(matches!(err, WorkspaceError::NotFound(_)), "{err:?}");
    assert!(err.to_string().contains("agent/never-pushed"), "{err}");
    // Nothing was left behind by the refusals.
    assert!(
        env.ws
            .open_existing("run-bad-00001")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        git(&env.remote, &["rev-parse", "refs/heads/main"]),
        main_before
    );
}

#[tokio::test]
async fn continuing_is_idempotent_survives_a_restart_and_does_not_change_its_mind() {
    let env = Env::new();
    let first = env.ws.prepare(&env.repo, "run-first-0001").await.unwrap();
    std::fs::write(first.path().join("f.txt"), "f\n").unwrap();
    first.commit_all("f", &me()).await.unwrap().unwrap();
    first.push().await.unwrap();
    let pushed = first.branch().to_owned();
    // A second pushed branch, to ask for another one.
    let other = env.ws.prepare(&env.repo, "run-other-0003").await.unwrap();
    std::fs::write(other.path().join("o.txt"), "o\n").unwrap();
    other.commit_all("o", &me()).await.unwrap().unwrap();
    other.push().await.unwrap();

    let a = env
        .ws
        .prepare_continuing(&env.repo, "run-second-002", &pushed)
        .await
        .unwrap();
    std::fs::write(a.path().join("wip.txt"), "uncommitted\n").unwrap();
    // Again: the same worktree, uncommitted work kept. A plain `prepare` finds it too.
    let again = env
        .ws
        .prepare_continuing(&env.repo, "run-second-002", &pushed)
        .await
        .unwrap();
    assert_eq!(again.path(), a.path());
    assert_eq!(again.branch(), pushed);
    assert!(again.path().join("wip.txt").exists());
    let plain = env.ws.prepare(&env.repo, "run-second-002").await.unwrap();
    assert_eq!(plain.branch(), pushed);

    // A new process finds it again, still publishing where it should.
    let restarted = Workspaces::new(env.root.clone(), Arc::new(StaticToken::new(TOKEN)));
    let found = restarted
        .open_existing("run-second-002")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.branch(), pushed);
    found.commit_all("wip", &me()).await.unwrap().unwrap();
    found.push().await.unwrap();

    // Another branch for the same run is a conflict, and so is continuing on a run that has a
    // branch of its own.
    let err = env
        .ws
        .prepare_continuing(&env.repo, "run-second-002", other.branch())
        .await
        .unwrap_err();
    assert!(matches!(err, WorkspaceError::Conflict(_)), "{err:?}");
    env.ws.prepare(&env.repo, "run-own-000004").await.unwrap();
    let err = env
        .ws
        .prepare_continuing(&env.repo, "run-own-000004", &pushed)
        .await
        .unwrap_err();
    assert!(matches!(err, WorkspaceError::Conflict(_)), "{err:?}");
}

#[tokio::test]
async fn a_continued_branch_that_moved_on_the_remote_is_not_overwritten() {
    let env = Env::new();
    let first = env.ws.prepare(&env.repo, "run-first-0001").await.unwrap();
    std::fs::write(first.path().join("f.txt"), "f\n").unwrap();
    first.commit_all("f", &me()).await.unwrap().unwrap();
    first.push().await.unwrap();
    let pushed = first.branch().to_owned();

    let second = env
        .ws
        .prepare_continuing(&env.repo, "run-second-002", &pushed)
        .await
        .unwrap();
    // Someone rewrites the branch on the remote meanwhile.
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
            &format!("tmp:refs/heads/{pushed}"),
        ],
    );
    let theirs = git(&env.remote, &["rev-parse", &format!("refs/heads/{pushed}")]);

    std::fs::write(second.path().join("c.txt"), "c\n").unwrap();
    let mine = second.commit_all("c", &me()).await.unwrap().unwrap();
    // The run's own branch takes the commit; the continued one, which moved, refuses it.
    second.push().await.unwrap();
    let err = second.publish().await.unwrap_err();
    assert!(matches!(err, WorkspaceError::Conflict(_)), "{err:?}");
    assert!(!err.is_retryable());
    assert!(
        err.to_string().contains("moved on the remote") && err.to_string().contains(&pushed),
        "{err}"
    );
    assert_eq!(
        git(&env.remote, &["rev-parse", &format!("refs/heads/{pushed}")]),
        theirs,
        "never forced"
    );
    assert_eq!(
        git(&env.remote, &["rev-parse", "refs/heads/agent/run-seco"]),
        mine,
        "the work is safe on the run's own branch"
    );
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

/// Two `Workspaces` on one root stand for two worker processes on a shared volume: they share
/// no in-process lock, only the file lock next to the mirror.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_workspaces_on_one_root_do_not_trip_over_each_others_git_locks() {
    let env = Env::new();
    let other = Workspaces::new(env.root.clone(), Arc::new(StaticToken::new(TOKEN)));
    let mut tasks = Vec::new();
    for i in 0..16 {
        let ws = if i % 2 == 0 {
            env.ws.clone()
        } else {
            other.clone()
        };
        let repo = env.repo.clone();
        tasks.push(tokio::spawn(async move {
            let run = format!("shared-{i:04}");
            let wt = ws.prepare(&repo, &run).await?;
            std::fs::write(wt.path().join(format!("f{i}.txt")), format!("{i}\n")).unwrap();
            wt.commit_all("work", &me()).await?;
            wt.push().await?;
            if i % 3 == 0 {
                ws.remove(&run).await?;
            }
            Ok::<_, WorkspaceError>(wt.branch().to_owned())
        }));
    }
    let mut branches = Vec::new();
    for t in tasks {
        let branch = t.await.unwrap().expect("no \"could not lock\" error");
        git(&env.remote, &["rev-parse", &format!("refs/heads/{branch}")]);
        branches.push(branch);
    }
    branches.sort_unstable();
    branches.dedup();
    assert_eq!(branches.len(), 16, "every run has its own branch");
    git(&env.mirror(), &["fsck", "--strict", "--no-dangling"]);
    let list = git(&env.mirror(), &["worktree", "list", "--porcelain"]);
    // The bare mirror plus the runs that were not removed (i % 3 != 0 -> 10 of 16).
    assert_eq!(list.matches("worktree ").count(), 1 + 10, "{list}");
}

/// The lock is a file next to the mirror, so a lock held by another process (here: another
/// handle) stops `prepare` until it lets go.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mirror_locked_by_another_process_makes_prepare_wait() {
    let env = Env::new();
    env.ws.prepare(&env.repo, "first-run-0001").await.unwrap();
    let lock_path = {
        let mut name = env.mirror().into_os_string();
        name.push(".lock");
        PathBuf::from(name)
    };
    assert!(lock_path.is_file(), "{}", lock_path.display());
    assert_eq!(
        lock_path.parent(),
        env.mirror().parent(),
        "the lock file is a sibling of the mirror, not inside it"
    );

    let held = std::fs::OpenOptions::new()
        .write(true)
        .open(&lock_path)
        .unwrap();
    held.lock().unwrap();

    let ws = env.ws.clone();
    let repo = env.repo.clone();
    let mut waiting = tokio::spawn(async move {
        ws.prepare(&repo, "second-run-0002")
            .await
            .map(|w| w.branch().to_owned())
    });
    let early = tokio::time::timeout(std::time::Duration::from_millis(400), &mut waiting).await;
    assert!(early.is_err(), "prepare must wait for the mirror lock");
    assert!(
        !env.root.join("worktrees/second-run-0002").exists(),
        "nothing was done to the mirror meanwhile"
    );

    held.unlock().unwrap();
    let branch = tokio::time::timeout(std::time::Duration::from_secs(20), waiting)
        .await
        .expect("prepare goes on once the lock is free")
        .unwrap()
        .unwrap();
    assert!(branch.starts_with("agent/"));
    assert!(env.root.join("worktrees/second-run-0002").is_dir());
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
    // The error says which branches the repository has, so that a caller can pick one.
    assert!(
        err.to_string().contains("no-such-branch")
            && err.to_string().contains("Its branches: main."),
        "{err}"
    );
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
    let tmp = tempfile::tempdir().unwrap();
    let ws = Workspaces::new(tmp.path().join("root"), Arc::new(Broken));
    // Credentials are only requested for http(s) remotes (a local remote
    // needs none); nothing listens on port 1, but the broker fails first.
    let repo = RepoRef::new("http://127.0.0.1:1/o/r.git", "main");
    let err = ws.prepare(&repo, "run-cred-0001").await.unwrap_err();
    assert!(matches!(err, WorkspaceError::Auth(_)), "{err:?}");
}

// --------------------------------------------------- repository host allowlist

/// Counts how often the token is requested, and for whom.
#[derive(Default)]
struct Spy {
    asked: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl GitCredentials for Spy {
    async fn token_for(&self, repo: &RepoRef) -> Result<secrecy::SecretString, WorkspaceError> {
        self.asked.lock().unwrap().push(repo.url.clone());
        Ok(secrecy::SecretString::from(TOKEN.to_owned()))
    }
}

/// An "evil" git server: answers 404 to everything and remembers what it saw.
async fn evil_server() -> wiremock::MockServer {
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    server
}

fn saw_authorization(requests: &[wiremock::Request]) -> bool {
    requests
        .iter()
        .any(|r| r.headers.contains_key("authorization"))
}

/// The point of the allowlist: whoever picks the repository URL must not be
/// able to make the token travel to a host of their choosing.
#[tokio::test]
async fn token_is_never_sent_to_a_host_outside_the_allowlist() {
    let evil = evil_server().await;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    let spy = Arc::new(Spy::default());
    let ws = Workspaces::new(root.clone(), spy.clone())
        .allow_hosts(["github.com"])
        .allow_local(true);

    let evil_url = format!("{}/octo/widgets.git", evil.uri());
    let mut hostile = vec![
        evil_url.clone(),
        // Look-alikes and tricks around the allowed name.
        "https://github.com.evil.example/octo/widgets.git".to_owned(),
        "https://evilgithub.com/octo/widgets.git".to_owned(),
        "https://evil.example/github.com/widgets.git".to_owned(),
        "https://github.com.:443/octo/widgets.git".to_owned(),
        "https://GITHUB.COM.evil.example/octo/widgets".to_owned(),
        "https://github.com:8443.evil.example/octo/widgets".to_owned(),
        "https://evil.example\\@github.com/octo/widgets.git".to_owned(),
    ];
    // Plain http to the allowed name is refused too when local is off (below).
    hostile.push(format!("http://127.0.0.1:{}/octo/widgets.git", 1));
    for (i, url) in hostile.iter().enumerate() {
        let err = ws
            .prepare(
                &RepoRef::new(url.clone(), "main"),
                &format!("run-evil-{i:04}"),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, WorkspaceError::Invalid(_)), "{url}: {err:?}");
        assert!(!err.to_string().contains(TOKEN), "{err}");
    }

    assert!(
        evil.received_requests().await.unwrap().is_empty(),
        "the evil host must not be contacted at all"
    );
    assert!(
        spy.asked.lock().unwrap().is_empty(),
        "the token must not even be requested for a refused host: {:?}",
        spy.asked.lock().unwrap()
    );
    assert!(
        !root.join("git").exists(),
        "no mirror is created for a refused repository"
    );
}

#[tokio::test]
async fn an_allowed_host_still_gets_the_token_and_only_it() {
    use base64::Engine as _;
    let server = evil_server().await;
    let port = server.address().port();
    let tmp = tempfile::tempdir().unwrap();
    let repo = RepoRef::new(format!("{}/octo/widgets.git", server.uri()), "main");
    let want = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{TOKEN}"))
    );

    // `host` and `host:port` entries both allow it (http needs allow_local).
    for (i, entry) in ["127.0.0.1".to_owned(), format!("127.0.0.1:{port}")]
        .into_iter()
        .enumerate()
    {
        let ws = Workspaces::new(
            tmp.path().join(format!("root-{i}")),
            Arc::new(StaticToken::new(TOKEN)),
        )
        .allow_hosts([entry.clone()])
        .allow_local(true);
        let err = ws
            .prepare(&repo, &format!("run-ok-{i:04}"))
            .await
            .unwrap_err();
        assert!(
            matches!(err, WorkspaceError::NotFound(_)),
            "{entry}: {err:?}"
        );
    }
    let requests = server.received_requests().await.unwrap();
    assert!(!requests.is_empty(), "git never reached the allowed host");
    for r in &requests {
        assert_eq!(
            r.headers.get("authorization").and_then(|v| v.to_str().ok()),
            Some(want.as_str())
        );
    }

    // The same host on another port is a different server.
    let before = requests.len();
    let ws = Workspaces::new(
        tmp.path().join("root-port"),
        Arc::new(StaticToken::new(TOKEN)),
    )
    .allow_hosts([format!("127.0.0.1:{}", port.wrapping_add(1))])
    .allow_local(true);
    let err = ws.prepare(&repo, "run-port-0001").await.unwrap_err();
    assert!(matches!(err, WorkspaceError::Invalid(_)), "{err:?}");
    assert_eq!(server.received_requests().await.unwrap().len(), before);

    // An empty allowlist accepts nothing.
    let ws = Workspaces::new(
        tmp.path().join("root-none"),
        Arc::new(StaticToken::new(TOKEN)),
    )
    .allow_hosts(Vec::<String>::new());
    let err = ws.prepare(&repo, "run-none-0001").await.unwrap_err();
    assert!(matches!(err, WorkspaceError::Invalid(_)), "{err:?}");
    assert!(err.to_string().contains("(none)"), "{err}");
}

#[tokio::test]
async fn local_paths_are_refused_unless_allowed() {
    let env = Env::new();
    let file_url = format!("file://{}", env.remote.display());
    let root = env.root.join("strict");
    let strict = Workspaces::new(root.clone(), Arc::new(Spy::default()))
        .allow_hosts(["github.com"])
        .allow_local(false);
    for (i, url) in [env.repo.url.clone(), file_url.clone()].iter().enumerate() {
        let err = strict
            .prepare(
                &RepoRef::new(url.clone(), "main"),
                &format!("run-loc-{i:04}"),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, WorkspaceError::Invalid(_)), "{url}: {err:?}");
        assert!(err.to_string().contains("local"), "{err}");
    }
    // Even with no host allowlist at all.
    let no_hosts = Workspaces::new(root.clone(), Arc::new(Spy::default())).allow_local(false);
    let err = no_hosts
        .prepare(&env.repo, "run-loc-0009")
        .await
        .unwrap_err();
    assert!(matches!(err, WorkspaceError::Invalid(_)), "{err:?}");
    assert!(!root.join("git").exists(), "nothing was created");

    // Plain http is dev-only as well, whatever the host allowlist says.
    let server = evil_server().await;
    let http = Workspaces::new(root.clone(), Arc::new(Spy::default()))
        .allow_hosts(["127.0.0.1"])
        .allow_local(false);
    let repo = RepoRef::new(format!("{}/o/r.git", server.uri()), "main");
    let err = http.prepare(&repo, "run-loc-0010").await.unwrap_err();
    assert!(matches!(err, WorkspaceError::Invalid(_)), "{err:?}");
    assert!(err.to_string().contains("https"), "{err}");
    assert!(server.received_requests().await.unwrap().is_empty());

    // Allowed: both spellings of a local remote work, and no token is asked for.
    let spy = Arc::new(Spy::default());
    let lax = Workspaces::new(env.root.join("lax"), spy.clone())
        .allow_hosts(["github.com"])
        .allow_local(true);
    lax.prepare(&env.repo, "run-loc-0020").await.unwrap();
    lax.prepare(&RepoRef::new(file_url, "main"), "run-loc-0021")
        .await
        .unwrap();
    assert!(
        spy.asked.lock().unwrap().is_empty(),
        "local remotes get no credentials"
    );
}

/// `check_repository` is the first check of every operation, on its own: a caller that wants to
/// ask a person about a repository first learns whether it could ever be added, and nothing is
/// spawned, requested or written to find out.
#[test]
fn check_repository_is_the_policy_alone() {
    let spy = Arc::new(Spy::default());
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    let ws = Workspaces::new(root.clone(), spy.clone())
        .allow_hosts(["github.com"])
        .allow_local(false);
    let check = |url: &str| ws.check_repository(&RepoRef::new(url, "main"));
    assert!(check("https://github.com/octo/widgets").is_ok());
    assert!(check("https://GitHub.com/octo/widgets.git").is_ok());
    for refused in [
        "https://evil.example/octo/widgets",
        "https://github.com.evil.example/octo/widgets",
        "http://github.com/octo/widgets",
        "/srv/git/widgets.git",
        "not a url",
        "https://user:pw@github.com/octo/widgets",
    ] {
        let err = check(refused).unwrap_err();
        assert!(
            matches!(err, WorkspaceError::Invalid(_)),
            "{refused}: {err:?}"
        );
    }
    let err = check("https://evil.example/octo/widgets").unwrap_err();
    assert!(
        err.to_string().contains("evil.example is not allowed"),
        "{err}"
    );
    assert!(
        spy.asked.lock().unwrap().is_empty(),
        "no credential was asked for"
    );
    assert!(!root.exists(), "nothing was created");
}

/// A token bound to `github.com` protects even a `Workspaces` that was never
/// given an allowlist.
#[tokio::test]
async fn a_scoped_token_alone_keeps_the_token_from_a_foreign_host() {
    let evil = evil_server().await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = Workspaces::new(
        tmp.path().join("root"),
        Arc::new(ScopedToken::new("github.com", TOKEN)),
    );
    let repo = RepoRef::new(format!("{}/octo/widgets.git", evil.uri()), "main");
    let err = ws.prepare(&repo, "run-scope-001").await.unwrap_err();
    assert!(matches!(err, WorkspaceError::Invalid(_)), "{err:?}");
    assert!(!err.to_string().contains(TOKEN), "{err}");
    assert!(
        !saw_authorization(&evil.received_requests().await.unwrap()),
        "no Authorization header may reach the foreign host"
    );
}

/// The policy is checked again where credentials are used: a worktree that
/// was made under a laxer configuration cannot push under a stricter one.
#[tokio::test]
async fn push_applies_the_current_policy() {
    let env = Env::new();
    let wt = env.ws.prepare(&env.repo, "run-push-pol-1").await.unwrap();
    std::fs::write(wt.path().join("p.txt"), "p\n").unwrap();
    wt.commit_all("p", &me()).await.unwrap().unwrap();

    let strict = Workspaces::new(env.root.clone(), Arc::new(StaticToken::new(TOKEN)))
        .allow_hosts(["github.com"])
        .allow_local(false);
    let same = strict
        .open_existing("run-push-pol-1")
        .await
        .unwrap()
        .unwrap();
    let err = same.push().await.unwrap_err();
    assert!(matches!(err, WorkspaceError::Invalid(_)), "{err:?}");
    assert!(
        git(&env.remote, &["branch", "--list", "agent/*"]).is_empty(),
        "nothing was pushed"
    );
    // The original, permissive handle still pushes.
    wt.push().await.unwrap();
}

#[tokio::test]
async fn git_is_given_the_canonical_url_of_an_http_remote() {
    let server = evil_server().await;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    let ws = Workspaces::new(root.clone(), Arc::new(StaticToken::new(TOKEN)));
    let raw = format!(
        "{}/Octo/Widgets/",
        server.uri().replace("http://", "HTTP://")
    );
    let repo = RepoRef::new(raw, "main");
    let _ = ws.prepare(&repo, "run-canon-001").await.unwrap_err();
    let loc = repo.locate().unwrap();
    let config = std::fs::read_to_string(root.join(loc.mirror_relative()).join("config")).unwrap();
    let want = format!("url = {}/Octo/Widgets.git", server.uri());
    assert!(config.contains(&want), "{config}");
}

/// A repository whose default branch is not `main` (the owner's case: `master`): the caller that
/// was not told a base branch asks the remote, and an unknown branch lists the real ones.
#[tokio::test]
async fn the_default_branch_is_the_remotes_and_a_missing_one_lists_the_branches() {
    let env = Env::new();
    assert_eq!(env.ws.default_branch(&env.repo.url).await.unwrap(), "main");
    // The remote's HEAD moves to another branch.
    git(&env.remote, &["branch", "master", "refs/heads/main"]);
    git(&env.remote, &["symbolic-ref", "HEAD", "refs/heads/master"]);
    assert_eq!(
        env.ws.default_branch(&env.repo.url).await.unwrap(),
        "master"
    );
    // ...and the workspace can start from it.
    let repo = RepoRef::new(env.repo.url.clone(), "master");
    env.ws.prepare(&repo, "run-default-01").await.unwrap();

    // Many branches: the first thirty by name, and the rest counted.
    for n in 0..34 {
        git(
            &env.remote,
            &["branch", &format!("topic-{n:02}"), "refs/heads/main"],
        );
    }
    let gone = RepoRef::new(env.repo.url.clone(), "develop");
    let err = env.ws.prepare(&gone, "run-default-02").await.unwrap_err();
    assert!(matches!(err, WorkspaceError::NotFound(_)), "{err:?}");
    let said = err.to_string();
    assert!(said.contains("develop"), "{said}");
    assert!(
        said.contains("master") && said.contains("topic-00"),
        "{said}"
    );
    assert!(
        said.contains("topic-27") && !said.contains("topic-28"),
        "{said}"
    );
    assert!(said.contains("(and 6 more)"), "{said}");
    // Branches of the agent's own are there too (a prior run's), not special.
    assert!(!said.contains("HEAD"), "{said}");
}

/// An empty remote has no default branch, which is not an empty answer.
#[tokio::test]
async fn an_empty_remote_has_no_default_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let remote = tmp.path().join("empty.git");
    std::fs::create_dir_all(&remote).unwrap();
    git(
        &remote,
        &["init", "--bare", "--quiet", "--initial-branch=main"],
    );
    let ws = Workspaces::new(
        tmp.path().join("workspaces"),
        Arc::new(StaticToken::new(TOKEN)),
    );
    let err = ws
        .default_branch(remote.to_str().unwrap())
        .await
        .unwrap_err();
    assert!(matches!(err, WorkspaceError::NoDefaultBranch(_)), "{err:?}");
    assert_eq!(err.class(), adam_error::ErrorClass::NotFound);
    assert!(err.to_string().contains("no default branch"), "{err}");
    // A refused url is refused before anything runs.
    let closed = Workspaces::new(tmp.path().join("w2"), Arc::new(StaticToken::new(TOKEN)))
        .allow_local(false);
    let err = closed
        .default_branch(remote.to_str().unwrap())
        .await
        .unwrap_err();
    assert!(matches!(err, WorkspaceError::Invalid(_)), "{err:?}");
}

// ------------------------------------------------------------------ a workspace of several slots

/// A bare remote `<tmp>/<owner>/<name>.git` on `main` with `files`, and the repository of it.
fn remote_with(tmp: &Path, owner: &str, name: &str, files: &[(&str, &str)]) -> (PathBuf, RepoRef) {
    let remote = tmp.join(owner).join(format!("{name}.git"));
    let seed = tmp.join(format!("seed-{owner}-{name}"));
    std::fs::create_dir_all(&remote).unwrap();
    std::fs::create_dir_all(&seed).unwrap();
    git(
        &remote,
        &["init", "--bare", "--quiet", "--initial-branch=main"],
    );
    git(&seed, &["init", "--quiet", "--initial-branch=main"]);
    for (file, content) in files {
        let path = seed.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
    if files.is_empty() {
        git(&seed, &["commit", "--quiet", "--allow-empty", "-m", "seed"]);
    } else {
        git(&seed, &["add", "-A"]);
        git(&seed, &["commit", "--quiet", "-m", "seed"]);
    }
    git(
        &seed,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&seed, &["push", "--quiet", "origin", "main"]);
    let repo = RepoRef::new(remote.to_str().unwrap(), "main");
    (remote, repo)
}

/// A bare remote with no refs at all, and the repository of it.
fn empty_remote(tmp: &Path, owner: &str, name: &str) -> (PathBuf, RepoRef) {
    let remote = tmp.join(owner).join(format!("{name}.git"));
    std::fs::create_dir_all(&remote).unwrap();
    git(
        &remote,
        &["init", "--bare", "--quiet", "--initial-branch=main"],
    );
    let repo = RepoRef::new(remote.to_str().unwrap(), "main");
    (remote, repo)
}

fn workspaces(tmp: &Path) -> Workspaces {
    Workspaces::new(tmp.join("root"), Arc::new(StaticToken::new(TOKEN)))
}

const RUN: &str = "slots-run-0001";

#[tokio::test]
async fn a_run_holds_two_repositories_each_in_a_slot_of_its_own() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, api) = remote_with(tmp.path(), "acme", "API", &[("api.txt", "api\n")]);
    let (_, web) = remote_with(tmp.path(), "acme", "web", &[("web.txt", "web\n")]);
    let ws = workspaces(tmp.path());
    let run = ws.run(RUN).unwrap();
    assert_eq!(run.path(), tmp.path().join("root/workspaces").join(RUN));
    assert!(run.slots().await.unwrap().is_empty(), "nothing yet");

    // Added in this order: web first.
    let web_slot = run.add_repository(&web).await.unwrap();
    let api_slot = run.add_repository(&api).await.unwrap();
    assert_eq!((web_slot.dir(), web_slot.seq()), ("web", 1));
    assert_eq!(
        (api_slot.dir(), api_slot.seq()),
        ("api", 2),
        "the name, lowercased"
    );
    assert_eq!(web_slot.path(), run.path().join("web"));
    let (web_wt, api_wt) = (web_slot.worktree().unwrap(), api_slot.worktree().unwrap());
    assert_eq!(web_wt.dir(), "web");
    assert_eq!(
        std::fs::read_to_string(web_wt.path().join("web.txt")).unwrap(),
        "web\n"
    );
    assert_eq!(
        std::fs::read_to_string(api_wt.path().join("api.txt")).unwrap(),
        "api\n"
    );
    assert!(
        !web_wt.path().join("api.txt").exists(),
        "each slot has its repository's files"
    );
    assert_eq!(
        git(web_wt.path(), &["symbolic-ref", "--short", "HEAD"]),
        web_wt.branch()
    );
    assert_eq!(
        web_wt.branch(),
        "agent/slots-ru",
        "the run's own branch, in each repository"
    );
    assert_eq!(api_wt.branch(), web_wt.branch());

    // Listed by directory, and in the order they joined.
    let by_dir: Vec<_> = run
        .slots()
        .await
        .unwrap()
        .iter()
        .map(|s| s.dir().to_owned())
        .collect();
    assert_eq!(by_dir, ["api", "web"]);
    let by_join: Vec<_> = run
        .slots_in_join_order()
        .await
        .unwrap()
        .iter()
        .map(|s| s.dir().to_owned())
        .collect();
    assert_eq!(by_join, ["web", "api"]);
    assert_eq!(
        run.slot("web").await.unwrap().unwrap().path(),
        web_slot.path()
    );
    assert!(run.slot("nope").await.unwrap().is_none());
    assert_eq!(run.slot_for(&api).await.unwrap().unwrap().dir(), "api");

    // Each is a worktree like any other: commit and push in one leaves the other alone.
    std::fs::write(web_wt.path().join("new.txt"), "new\n").unwrap();
    web_wt.commit_all("work", &me()).await.unwrap().unwrap();
    web_wt.push().await.unwrap();
    assert!(api_wt.status().await.unwrap().is_empty());
    let meta =
        std::fs::read_to_string(tmp.path().join("root/meta").join(RUN).join("web.json")).unwrap();
    assert!(
        meta.contains("\"version\": 2") && meta.contains("\"seq\": 1"),
        "{meta}"
    );
    assert!(
        meta.contains("\"kind\": \"repo\"") && meta.contains("\"dir\": \"web\""),
        "{meta}"
    );
    assert!(!meta.contains(TOKEN), "no credentials in the metadata");
}

#[tokio::test]
async fn adding_a_repository_twice_returns_its_one_slot() {
    let tmp = tempfile::tempdir().unwrap();
    let (remote, repo) = remote_with(tmp.path(), "acme", "lib", &[("lib.txt", "lib\n")]);
    let ws = workspaces(tmp.path());
    let run = ws.run(RUN).unwrap();
    let first = run.add_repository(&repo).await.unwrap();
    std::fs::write(first.path().join("wip.txt"), "uncommitted\n").unwrap();

    // The same repository, written another way and with another base: the same slot.
    let as_url = RepoRef::new(format!("file://{}", remote.display()), "develop");
    let again = run.add_repository(&as_url).await.unwrap();
    assert_eq!(again.path(), first.path());
    assert_eq!(again.seq(), first.seq());
    assert_eq!(
        again.worktree().unwrap().repo().base_branch,
        "main",
        "the slot keeps the base it was made with"
    );
    assert_eq!(run.slots().await.unwrap().len(), 1);
    assert!(
        first.path().join("wip.txt").is_file(),
        "work in progress survives"
    );
    assert_eq!(run.slot_for(&as_url).await.unwrap().unwrap().dir(), "lib");
}

#[tokio::test]
async fn two_repositories_with_one_name_are_told_apart_by_their_owner() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, mine) = remote_with(tmp.path(), "a", "lib", &[("mine.txt", "a\n")]);
    let (_, theirs) = remote_with(tmp.path(), "b", "Lib", &[("theirs.txt", "b\n")]);
    let (_, third) = remote_with(tmp.path(), "c", "lib", &[("third.txt", "c\n")]);
    let ws = workspaces(tmp.path());
    let run = ws.run(RUN).unwrap();
    let one = run.add_repository(&mine).await.unwrap();
    let two = run.add_repository(&theirs).await.unwrap();
    let three = run.add_repository(&third).await.unwrap();
    assert_eq!(one.dir(), "lib");
    assert!(
        two.dir().starts_with("lib-") && two.dir() != "lib",
        "{}",
        two.dir()
    );
    assert!(
        three.dir().starts_with("lib-") && three.dir() != two.dir(),
        "{}",
        three.dir()
    );
    assert!(two.path().join("theirs.txt").is_file());
    assert!(three.path().join("third.txt").is_file());
    // Each is found again by its own repository.
    assert_eq!(
        run.slot_for(&theirs).await.unwrap().unwrap().dir(),
        two.dir()
    );
    assert_eq!(
        run.slot_for(&third).await.unwrap().unwrap().dir(),
        three.dir()
    );
}

#[tokio::test]
async fn a_slot_can_continue_a_pushed_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let (remote, repo) = remote_with(tmp.path(), "acme", "lib", &[("lib.txt", "lib\n")]);
    let ws = workspaces(tmp.path());
    let first = ws
        .run("run-first-0001")
        .unwrap()
        .add_repository(&repo)
        .await
        .unwrap();
    let first = first.worktree().unwrap();
    std::fs::write(first.path().join("first.txt"), "one\n").unwrap();
    first.commit_all("first", &me()).await.unwrap().unwrap();
    first.push().await.unwrap();

    let second = ws.run("run-second-002").unwrap();
    let slot = second
        .add_repository_continuing(&repo, first.branch())
        .await
        .unwrap();
    let wt = slot.worktree().unwrap();
    assert_eq!(wt.continues(), Some(first.branch()));
    assert_eq!(
        std::fs::read_to_string(wt.path().join("first.txt")).unwrap(),
        "one\n"
    );
    assert_eq!(
        git(wt.path(), &["rev-parse", "HEAD"]),
        git(
            &remote,
            &["rev-parse", &format!("refs/heads/{}", first.branch())]
        )
    );
    // Again: the same slot; another branch for the repository the run has: a conflict.
    let again = second
        .add_repository_continuing(&repo, first.branch())
        .await
        .unwrap();
    assert_eq!(again.path(), slot.path());
    let err = second.add_repository(&repo).await;
    assert!(
        err.is_ok(),
        "a plain request finds the slot of a continued branch: {err:?}"
    );
    let err = second
        .add_repository_continuing(&repo, "agent/other")
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            WorkspaceError::Conflict(_) | WorkspaceError::Invalid(_)
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_slot_whose_directory_was_lost_is_made_again_on_its_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, repo) = remote_with(tmp.path(), "acme", "lib", &[("lib.txt", "lib\n")]);
    let ws = workspaces(tmp.path());
    let run = ws.run(RUN).unwrap();
    let slot = run.add_repository(&repo).await.unwrap();
    let wt = slot.worktree().unwrap();
    std::fs::write(wt.path().join("kept.txt"), "k\n").unwrap();
    let sha = wt.commit_all("kept", &me()).await.unwrap().unwrap();
    std::fs::remove_dir_all(slot.path()).unwrap();
    assert!(
        run.slots().await.unwrap().is_empty(),
        "a slot without its directory is not listed"
    );

    let again = run.add_repository(&repo).await.unwrap();
    assert_eq!(again.dir(), slot.dir());
    assert_eq!(again.seq(), slot.seq());
    assert_eq!(
        git(again.path(), &["rev-parse", "HEAD"]),
        sha,
        "the branch kept its commit"
    );
    assert!(again.path().join("kept.txt").is_file());
}

#[tokio::test]
async fn a_refused_repository_creates_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, repo) = remote_with(tmp.path(), "acme", "lib", &[("lib.txt", "lib\n")]);
    let ws = workspaces(tmp.path()).allow_local(false);
    let run = ws.run(RUN).unwrap();
    let err = run.add_repository(&repo).await.unwrap_err();
    assert!(matches!(err, WorkspaceError::Invalid(_)), "{err:?}");
    assert!(
        !tmp.path().join("root").exists(),
        "no directory, lock or mirror for a refused repository"
    );
    assert!(matches!(
        ws.run("a/b").unwrap_err(),
        WorkspaceError::Invalid(_)
    ));
    assert!(matches!(
        ws.run(".hidden").unwrap_err(),
        WorkspaceError::Invalid(_)
    ));
}

#[tokio::test]
async fn a_scratch_project_is_a_local_repository_with_a_root_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = workspaces(tmp.path());
    let run = ws.run(RUN).unwrap();
    let slot = run.add_scratch("fib", &me()).await.unwrap();
    assert_eq!((slot.dir(), slot.seq()), ("fib", 1));
    assert!(matches!(slot.kind(), SlotKind::Scratch(_)));
    assert!(slot.worktree().is_none());
    let scratch = slot.scratch().unwrap();
    assert_eq!(scratch.path(), run.path().join("fib"));
    assert!(
        scratch.path().join(".git").is_dir(),
        "a repository of its own"
    );
    assert_eq!(
        git(scratch.path(), &["symbolic-ref", "--short", "HEAD"]),
        "main"
    );
    assert_eq!(
        git(scratch.path(), &["rev-list", "--count", "HEAD"]),
        "1",
        "the root commit"
    );
    assert_eq!(
        git(scratch.path(), &["log", "-1", "--format=%an|%ae"]),
        "Adam Agent|adam@example.com"
    );
    assert!(scratch.files().await.unwrap().is_empty());
    assert!(
        !scratch.path().join(".git/hooks/pre-commit.sample").exists(),
        "no sample hooks from a template"
    );

    // Files: tracked and untracked ones that are not ignored, relative, sorted.
    std::fs::create_dir_all(scratch.path().join("src")).unwrap();
    std::fs::write(scratch.path().join("src/fib.sh"), "echo 0 1 1\n").unwrap();
    std::fs::write(scratch.path().join(".gitignore"), "*.log\ntarget/\n").unwrap();
    std::fs::write(scratch.path().join("run.log"), "noise\n").unwrap();
    std::fs::create_dir_all(scratch.path().join("target")).unwrap();
    std::fs::write(scratch.path().join("target/out"), "x\n").unwrap();
    std::fs::write(scratch.path().join("README.md"), "# fib\n").unwrap();
    let names = |files: Vec<PathBuf>| -> Vec<String> {
        files
            .into_iter()
            .map(|f| f.to_string_lossy().into_owned())
            .collect()
    };
    assert_eq!(
        names(scratch.files().await.unwrap()),
        [".gitignore", "README.md", "src/fib.sh"]
    );
    let sha = scratch
        .commit_all("feat: fib", &me())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(git(scratch.path(), &["rev-parse", "HEAD"]), sha);
    assert_eq!(git(scratch.path(), &["rev-list", "--count", "HEAD"]), "2");
    assert!(
        scratch.commit_all("again", &me()).await.unwrap().is_none(),
        "nothing to commit"
    );
    // A tracked file that was deleted is gone from the list.
    std::fs::remove_file(scratch.path().join("README.md")).unwrap();
    assert_eq!(
        names(scratch.files().await.unwrap()),
        [".gitignore", "src/fib.sh"]
    );
    assert!(
        scratch.commit_all("", &me()).await.is_err(),
        "a message is required"
    );

    // Idempotent; another name is another slot; a repository's name is taken.
    let again = run.add_scratch("fib", &me()).await.unwrap();
    assert_eq!(again.path(), slot.path());
    assert_eq!(again.seq(), 1);
    assert_eq!(
        git(scratch.path(), &["rev-list", "--count", "HEAD"]),
        "2",
        "not started over"
    );
    assert_eq!(run.add_scratch("other", &me()).await.unwrap().seq(), 2);
    for bad in ["", "Fib", "a/b", ".x", "repo.git", "a b"] {
        let err = run.add_scratch(bad, &me()).await.unwrap_err();
        assert!(
            matches!(err, WorkspaceError::Invalid(_)),
            "{bad:?}: {err:?}"
        );
    }
    let (_, repo) = remote_with(tmp.path(), "acme", "lib", &[("lib.txt", "lib\n")]);
    run.add_repository(&repo).await.unwrap();
    let err = run.add_scratch("lib", &me()).await.unwrap_err();
    assert!(matches!(err, WorkspaceError::Conflict(_)), "{err:?}");
    // A repository whose name a scratch project has gets the owner in its directory.
    let (_, other) = remote_with(tmp.path(), "zed", "fib", &[("x.txt", "x\n")]);
    assert!(
        run.add_repository(&other)
            .await
            .unwrap()
            .dir()
            .starts_with("fib-")
    );
}

#[tokio::test]
async fn a_scratch_project_that_lost_its_root_commit_gets_one_again() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = workspaces(tmp.path());
    let run = ws.run(RUN).unwrap();
    let slot = run.add_scratch("fib", &me()).await.unwrap();
    // A crash between `git init` and the first commit: an unborn branch.
    std::fs::remove_dir_all(slot.path().join(".git")).unwrap();
    git(slot.path(), &["init", "--quiet", "--initial-branch=main"]);
    let again = run.add_scratch("fib", &me()).await.unwrap();
    assert_eq!(git(again.path(), &["rev-list", "--count", "HEAD"]), "1");
}

/// A scratch project lives for its conversation: the run that made it records the conversation, a
/// later run of the same conversation takes the project over (its files, its history and what it
/// says of where it was published come with it), and another conversation's projects stay put.
#[tokio::test]
async fn a_later_run_of_the_conversation_takes_the_scratch_projects_over() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = workspaces(tmp.path());
    let first = ws.run("run-1").unwrap();
    let site = first
        .add_scratch_for("site", &me(), Some("c1"))
        .await
        .unwrap();
    std::fs::write(site.path().join("index.html"), "<p>hi</p>\n").unwrap();
    site.scratch()
        .unwrap()
        .commit_all("page", &me())
        .await
        .unwrap();
    site.scratch()
        .unwrap()
        .set_published_to("https://github.com/acme/site")
        .await
        .unwrap();
    first
        .add_scratch_for("other", &me(), Some("c2"))
        .await
        .unwrap();
    first.add_scratch("plain", &me()).await.unwrap();
    assert_eq!(
        first
            .slot("site")
            .await
            .unwrap()
            .unwrap()
            .scratch()
            .unwrap()
            .conversation(),
        Some("c1")
    );

    let second = ws.run("run-2").unwrap();
    assert_eq!(second.adopt_scratch("c1").await.unwrap(), ["site"]);
    let taken = second
        .slot("site")
        .await
        .unwrap()
        .expect("the project moved");
    let project = taken.scratch().unwrap();
    assert_eq!(project.path(), second.path().join("site"));
    assert_eq!(project.conversation(), Some("c1"));
    assert_eq!(project.published_to(), Some("https://github.com/acme/site"));
    assert_eq!(
        std::fs::read_to_string(project.path().join("index.html")).unwrap(),
        "<p>hi</p>\n"
    );
    assert_eq!(git(project.path(), &["rev-list", "--count", "HEAD"]), "2");
    // It is the second run's now, and not the first's; what is of another conversation, or of none, stays.
    assert!(first.slot("site").await.unwrap().is_none());
    assert!(!first.path().join("site").exists());
    assert!(first.slot("other").await.unwrap().is_some());
    assert!(first.slot("plain").await.unwrap().is_some());
    // A repeat finds nothing, and the project can be written to as any slot of the run.
    assert!(second.adopt_scratch("c1").await.unwrap().is_empty());
    assert_eq!(second.adopt_scratch("c2").await.unwrap(), ["other"]);
    std::fs::write(project.path().join("more.txt"), "more\n").unwrap();
    assert_eq!(project.status().await.unwrap().len(), 1);
    // The thread goes on: the next run takes the project from the run that holds it, work in
    // progress included.
    let third = ws.run("run-3").unwrap();
    assert_eq!(third.adopt_scratch("c1").await.unwrap(), ["site"]);
    assert!(second.slot("site").await.unwrap().is_none());
    assert!(third.path().join("site/more.txt").is_file());
}

/// A project whose name the run already has is not replaced: the run's own wins, and the other stays
/// with its run.
#[tokio::test]
async fn taking_over_never_replaces_a_project_the_run_already_has() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = workspaces(tmp.path());
    let first = ws.run("run-1").unwrap();
    let old = first
        .add_scratch_for("site", &me(), Some("c1"))
        .await
        .unwrap();
    std::fs::write(old.path().join("old.txt"), "old\n").unwrap();
    let second = ws.run("run-2").unwrap();
    let own = second
        .add_scratch_for("site", &me(), Some("c1"))
        .await
        .unwrap();
    std::fs::write(own.path().join("own.txt"), "own\n").unwrap();

    assert!(second.adopt_scratch("c1").await.unwrap().is_empty());
    assert!(second.path().join("site/own.txt").is_file());
    assert!(!second.path().join("site/old.txt").exists());
    assert!(
        first.path().join("site/old.txt").is_file(),
        "left where it was"
    );
    assert!(first.slot("site").await.unwrap().is_some());
}

/// The janitor's removal of a finished run keeps the projects a later task may take over, and nothing
/// else; once nothing is kept it is `remove`.
#[tokio::test]
async fn remove_keeping_scratch_keeps_only_the_projects_of_a_conversation() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, repo) = remote_with(tmp.path(), "acme", "web", &[("web.txt", "base\n")]);
    let ws = workspaces(tmp.path());
    let run = ws.run(RUN).unwrap();
    let worktree = run.add_repository(&repo).await.unwrap();
    run.add_scratch_for("site", &me(), Some("c1"))
        .await
        .unwrap();
    run.add_scratch("plain", &me()).await.unwrap();

    assert_eq!(run.remove_keeping_scratch().await.unwrap(), 1);
    assert!(!worktree.path().exists(), "the worktree is gone");
    let slots = run.slots().await.unwrap();
    assert_eq!(
        slots.iter().map(|s| s.dir().to_owned()).collect::<Vec<_>>(),
        ["site"]
    );
    assert!(run.path().join("site/.git").is_dir());
    assert!(!run.path().join("plain").exists());
    // Idempotent, and a later full removal takes the rest.
    assert_eq!(run.remove_keeping_scratch().await.unwrap(), 1);
    run.remove().await.unwrap();
    assert!(!run.path().exists());
    assert!(!tmp.path().join("root/meta").join(RUN).exists());
    // A workspace with nothing to keep is removed whole.
    let other = ws.run("run-9").unwrap();
    other.add_scratch("plain", &me()).await.unwrap();
    assert_eq!(other.remove_keeping_scratch().await.unwrap(), 0);
    assert!(!other.path().exists());
}

/// A check can be run on the code the run started from: a detached checkout of `origin/<base>`
/// next to the slot, with the installed (ignored) files of the slot linked in, and nothing of the
/// run's changes. It is a worktree of the same repository, and removing it leaves the run alone.
#[tokio::test]
async fn the_base_can_be_checked_out_beside_the_slot_and_removed() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, repo) = remote_with(
        tmp.path(),
        "acme",
        "web",
        &[("web.txt", "base\n"), (".gitignore", "node_modules/\n")],
    );
    let ws = workspaces(tmp.path());
    let run = ws.run(RUN).unwrap();
    let slot = run.add_repository(&repo).await.unwrap();
    let wt = slot.worktree().unwrap();
    let base = wt.base_commit().await.unwrap().expect("the base is known");
    assert_eq!(base, git(wt.path(), &["rev-parse", "origin/main"]));

    // The run changes a file, adds one, and has installed dependencies git ignores.
    std::fs::write(wt.path().join("web.txt"), "changed\n").unwrap();
    std::fs::write(wt.path().join("new.txt"), "new\n").unwrap();
    std::fs::create_dir_all(wt.path().join("node_modules/dep")).unwrap();
    std::fs::write(wt.path().join("node_modules/dep/index.js"), "dep\n").unwrap();
    wt.commit_all("change", &me()).await.unwrap();

    let checkout = wt.add_base_checkout().await.unwrap().expect("a checkout");
    assert_eq!(checkout.commit(), base);
    assert_eq!(checkout.path(), run.path().join(".adam-base/web"));
    assert_eq!(
        std::fs::read_to_string(checkout.path().join("web.txt")).unwrap(),
        "base\n",
        "the base's files, not the run's"
    );
    assert!(!checkout.path().join("new.txt").exists());
    assert_eq!(
        std::fs::read_to_string(checkout.path().join("node_modules/dep/index.js")).unwrap(),
        "dep\n",
        "the installed dependencies are the slot's"
    );
    assert_eq!(git(checkout.path(), &["rev-parse", "HEAD"]), base);
    assert_eq!(
        git(checkout.path(), &["rev-parse", "--abbrev-ref", "HEAD"]),
        "HEAD",
        "detached: no branch of the run moved"
    );
    assert!(
        run.slots().await.unwrap().iter().all(|s| s.dir() == "web"),
        "the checkout is not a slot"
    );

    // Made again over a leftover, and removed: the slot keeps its files and its dependencies.
    let again = wt.add_base_checkout().await.unwrap().expect("a checkout");
    wt.remove_base_checkout(&again).await.unwrap();
    assert!(!again.path().exists());
    assert_eq!(
        std::fs::read_to_string(wt.path().join("node_modules/dep/index.js")).unwrap(),
        "dep\n",
        "removing the links does not touch what they pointed at"
    );
    assert_eq!(
        std::fs::read_to_string(wt.path().join("web.txt")).unwrap(),
        "changed\n"
    );
    assert!(
        !git(wt.path(), &["worktree", "list"]).contains(".adam-base"),
        "the mirror forgot it"
    );
    wt.remove_base_checkout(&again).await.unwrap();
}

#[tokio::test]
async fn a_scratch_project_remembers_where_it_was_published_and_what_changed_since() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = workspaces(tmp.path());
    let run = ws.run(RUN).unwrap();
    let slot = run.add_scratch("fib", &me()).await.unwrap();
    let scratch = slot.scratch().unwrap();
    assert_eq!(scratch.published_to(), None);
    assert!(scratch.status().await.unwrap().is_empty());

    std::fs::write(scratch.path().join("fib.sh"), "echo 0\n").unwrap();
    let status = scratch.status().await.unwrap();
    assert_eq!(
        status
            .iter()
            .map(|f| (f.path.as_str(), f.status))
            .collect::<Vec<_>>(),
        [("fib.sh", adam_workspace::FileStatus::Untracked)]
    );
    scratch.commit_all("fib", &me()).await.unwrap();
    assert!(scratch.status().await.unwrap().is_empty());

    scratch
        .set_published_to("http://git-server:8080/scratch/fib.git")
        .await
        .unwrap();
    // The value a listing gives is the one in the metadata, which survives a new handle on the
    // root (a restart), and is replaced by a later publication.
    let ws = workspaces(tmp.path());
    let run = ws.run(RUN).unwrap();
    let listed = run.slot("fib").await.unwrap().unwrap();
    assert_eq!(
        listed.scratch().unwrap().published_to(),
        Some("http://git-server:8080/scratch/fib.git")
    );
    listed
        .scratch()
        .unwrap()
        .set_published_to("http://git-server:8080/scratch/other.git")
        .await
        .unwrap();
    let again = run.add_scratch("fib", &me()).await.unwrap();
    assert_eq!(
        again.scratch().unwrap().published_to(),
        Some("http://git-server:8080/scratch/other.git"),
        "asking for the project again does not forget where it went"
    );
    assert_eq!(again.seq(), 1, "and does not move it in the order");

    // A workspace that was removed has no project to say it of.
    let scratch = again.scratch().unwrap().clone();
    run.remove().await.unwrap();
    let err = scratch.set_published_to("http://x/y/z.git").await;
    assert!(matches!(err, Err(WorkspaceError::NotFound(_))), "{err:?}");
}

#[tokio::test]
async fn an_empty_remote_is_given_its_first_commit_and_only_then() {
    let tmp = tempfile::tempdir().unwrap();
    let (remote, repo) = empty_remote(tmp.path(), "scratch", "fib");
    let ws = workspaces(tmp.path());
    assert!(ws.remote_is_empty(&repo.url).await.unwrap());

    let sha = ws.initialize_empty(&repo, &me()).await.unwrap();
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/main"]), sha);
    assert_eq!(
        git(&remote, &["rev-parse", "main^{tree}"]),
        "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
        "the empty tree"
    );
    assert_eq!(
        git(&remote, &["log", "-1", "--format=%s|%an|%ae", "main"]),
        "Initial commit|Adam Agent|adam@example.com"
    );
    assert_eq!(git(&remote, &["rev-list", "--count", "main"]), "1");
    assert!(!ws.remote_is_empty(&repo.url).await.unwrap());

    // Not empty any more: a conflict, and nothing moved.
    let err = ws.initialize_empty(&repo, &me()).await.unwrap_err();
    assert!(matches!(err, WorkspaceError::Conflict(_)), "{err:?}");
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/main"]), sha);

    // A worktree starts from the new base, and is empty.
    let run = ws.run(RUN).unwrap();
    let slot = run.add_repository(&repo).await.unwrap();
    assert_eq!(git(slot.path(), &["rev-parse", "HEAD"]), sha);
    assert!(slot.worktree().unwrap().status().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_remote_with_refs_is_never_given_a_first_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let (remote, repo) = remote_with(tmp.path(), "acme", "lib", &[("lib.txt", "lib\n")]);
    let ws = workspaces(tmp.path());
    assert!(!ws.remote_is_empty(&repo.url).await.unwrap());
    let before = git(&remote, &["rev-parse", "refs/heads/main"]);
    let err = ws.initialize_empty(&repo, &me()).await.unwrap_err();
    assert!(matches!(err, WorkspaceError::Conflict(_)), "{err:?}");
    // Not even under another branch name.
    let err = ws
        .initialize_empty(&RepoRef::new(&repo.url, "develop"), &me())
        .await
        .unwrap_err();
    assert!(matches!(err, WorkspaceError::Conflict(_)), "{err:?}");
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/main"]),
        before,
        "nothing was forced"
    );
    assert_eq!(remote_refs_of(&remote), ["refs/heads/main"]);

    // A bad branch name or identity is refused before anything runs.
    let (_, empty) = empty_remote(tmp.path(), "scratch", "e");
    assert!(matches!(
        ws.initialize_empty(&RepoRef::new(&empty.url, "bad..branch"), &me())
            .await
            .unwrap_err(),
        WorkspaceError::Invalid(_)
    ));
    assert!(matches!(
        ws.initialize_empty(&empty, &GitIdentity::new("", "x@y"))
            .await
            .unwrap_err(),
        WorkspaceError::Invalid(_)
    ));
    assert!(ws.remote_is_empty(&empty.url).await.unwrap());
}

fn remote_refs_of(remote: &Path) -> Vec<String> {
    git(remote, &["for-each-ref", "--format=%(refname)"])
        .lines()
        .map(str::to_owned)
        .collect()
}

#[tokio::test]
async fn wait_reachable_waits_for_a_repository_that_appears() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = workspaces(tmp.path());
    let (_, there) = remote_with(tmp.path(), "acme", "lib", &[("lib.txt", "lib\n")]);
    ws.wait_reachable(&there.url, std::time::Duration::from_secs(5))
        .await
        .unwrap();

    // One that does not exist is NotFound once the time is up.
    let missing = tmp.path().join("later").join("late.git");
    let started = std::time::Instant::now();
    let err = ws
        .wait_reachable(
            missing.to_str().unwrap(),
            std::time::Duration::from_millis(600),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, WorkspaceError::NotFound(_)), "{err:?}");
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(500),
        "it waited"
    );

    // One that is created meanwhile is found.
    let url = missing.to_str().unwrap().to_owned();
    let make = tokio::spawn({
        let tmp = tmp.path().to_path_buf();
        async move {
            tokio::time::sleep(std::time::Duration::from_millis(700)).await;
            empty_remote(&tmp, "later", "late");
        }
    });
    ws.wait_reachable(&url, std::time::Duration::from_secs(15))
        .await
        .unwrap();
    make.await.unwrap();
    // A url the policy refuses fails at once, with no waiting.
    let strict = workspaces(tmp.path()).allow_local(false);
    let started = std::time::Instant::now();
    let err = strict
        .wait_reachable(&url, std::time::Duration::from_secs(10))
        .await
        .unwrap_err();
    assert!(matches!(err, WorkspaceError::Invalid(_)), "{err:?}");
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
}

/// The scratch project that is published: its files into a worktree of an (empty) repository.
async fn scratch_and_worktree(tmp: &Path) -> (adam_workspace::Scratch, adam_workspace::Worktree) {
    let (_, repo) = empty_remote(tmp, "scratch", "fib");
    let ws = workspaces(tmp);
    ws.initialize_empty(&repo, &me()).await.unwrap();
    let run = ws.run(RUN).unwrap();
    let scratch = run
        .add_scratch("fib", &me())
        .await
        .unwrap()
        .scratch()
        .unwrap()
        .clone();
    let wt = run
        .add_repository(&repo)
        .await
        .unwrap()
        .worktree()
        .unwrap()
        .clone();
    (scratch, wt)
}

fn put(root: &Path, file: &str, content: &str) {
    let path = root.join(file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn paths(files: &[PathBuf]) -> Vec<String> {
    files
        .iter()
        .map(|f| f.to_string_lossy().into_owned())
        .collect()
}

#[tokio::test]
async fn copy_into_puts_the_projects_files_in_the_repository() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let (scratch, wt) = scratch_and_worktree(tmp.path()).await;
    put(scratch.path(), "fib.sh", "echo 0 1 1 2 3 5 8\n");
    put(scratch.path(), "docs/notes.md", "notes\n");
    put(scratch.path(), ".gitignore", "*.log\n");
    put(scratch.path(), "run.log", "ignored\n");
    std::fs::set_permissions(
        scratch.path().join("fib.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    std::os::unix::fs::symlink("notes.md", scratch.path().join("docs/latest")).unwrap();

    let report = copy_into(&scratch, &wt, ".", false).await.unwrap();
    assert!(report.collisions.is_empty(), "{report:?}");
    assert_eq!(
        paths(&report.copied),
        [".gitignore", "docs/latest", "docs/notes.md", "fib.sh"]
    );
    assert!(report.unchanged.is_empty());
    assert_eq!(
        std::fs::read_to_string(wt.path().join("docs/notes.md")).unwrap(),
        "notes\n"
    );
    assert_eq!(
        std::fs::metadata(wt.path().join("fib.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755,
        "the executable bit stays"
    );
    assert_eq!(
        std::fs::read_link(wt.path().join("docs/latest")).unwrap(),
        PathBuf::from("notes.md"),
        "a symbolic link that stays inside is kept as a link"
    );
    assert!(
        !wt.path().join("run.log").exists(),
        "ignored files stay behind"
    );
    assert!(
        wt.path().join(".git").is_file(),
        "the repository's own .git is untouched"
    );
    assert!(
        !std::fs::read_dir(wt.path()).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains("adam-copy")),
        "no temporary file is left"
    );
    // The worktree sees them as changes, and they commit.
    assert!(wt.commit_all("feat: fib", &me()).await.unwrap().is_some());

    // The same again: all unchanged, nothing copied.
    let again = copy_into(&scratch, &wt, "", false).await.unwrap();
    assert!(
        again.copied.is_empty() && again.collisions.is_empty(),
        "{again:?}"
    );
    assert_eq!(again.unchanged.len(), 4);
}

#[tokio::test]
async fn copy_into_is_all_or_nothing_and_overwrite_replaces_files_only() {
    let tmp = tempfile::tempdir().unwrap();
    let (scratch, wt) = scratch_and_worktree(tmp.path()).await;
    put(scratch.path(), "a.txt", "from scratch\n");
    put(scratch.path(), "b.txt", "same\n");
    put(scratch.path(), "new.txt", "new\n");
    put(wt.path(), "a.txt", "in the repository\n");
    put(wt.path(), "b.txt", "same\n");

    let report = copy_into(&scratch, &wt, ".", false).await.unwrap();
    assert!(
        report.copied.is_empty(),
        "a collision stops the whole copy: {report:?}"
    );
    assert_eq!(paths(&report.unchanged), ["b.txt"]);
    assert_eq!(report.collisions.len(), 1);
    assert_eq!(report.collisions[0].path, PathBuf::from("a.txt"));
    assert!(
        report.collisions[0].reason.contains("other content"),
        "{:?}",
        report.collisions
    );
    assert!(!wt.path().join("new.txt").exists(), "nothing was copied");
    assert_eq!(
        std::fs::read_to_string(wt.path().join("a.txt")).unwrap(),
        "in the repository\n"
    );

    let report = copy_into(&scratch, &wt, ".", true).await.unwrap();
    assert!(report.collisions.is_empty(), "{report:?}");
    assert_eq!(paths(&report.copied), ["a.txt", "new.txt"]);
    assert_eq!(paths(&report.unchanged), ["b.txt"]);
    assert_eq!(
        std::fs::read_to_string(wt.path().join("a.txt")).unwrap(),
        "from scratch\n"
    );

    // What overwrite never does: put a file where the repository has a directory or a link, or
    // write through a link.
    put(scratch.path(), "docs", "a file\n");
    std::fs::create_dir_all(wt.path().join("docs")).unwrap();
    put(wt.path(), "docs/x", "x\n");
    let report = copy_into(&scratch, &wt, ".", true).await.unwrap();
    assert_eq!(report.collisions.len(), 1, "{report:?}");
    assert!(
        report.collisions[0].reason.contains("directory"),
        "{:?}",
        report.collisions
    );
    assert_eq!(
        std::fs::read_to_string(wt.path().join("docs/x")).unwrap(),
        "x\n"
    );
    std::fs::remove_file(scratch.path().join("docs")).unwrap();

    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), wt.path().join("linked")).unwrap();
    put(scratch.path(), "linked/inside.txt", "x\n");
    let report = copy_into(&scratch, &wt, ".", true).await.unwrap();
    assert_eq!(report.collisions.len(), 1, "{report:?}");
    assert!(
        report.collisions[0].reason.contains("symbolic link"),
        "{:?}",
        report.collisions
    );
    assert!(!outside.path().join("inside.txt").exists());
}

#[tokio::test]
async fn copy_into_refuses_a_link_that_leaves_the_project_and_a_destination_that_leaves_the_repository()
 {
    let tmp = tempfile::tempdir().unwrap();
    let (scratch, wt) = scratch_and_worktree(tmp.path()).await;
    put(scratch.path(), "ok.txt", "ok\n");
    std::os::unix::fs::symlink("../../etc/passwd", scratch.path().join("up")).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", scratch.path().join("abs")).unwrap();
    let report = copy_into(&scratch, &wt, ".", false).await.unwrap();
    assert!(report.copied.is_empty(), "all or nothing: {report:?}");
    let refused: Vec<_> = report
        .collisions
        .iter()
        .map(|c| c.path.to_string_lossy().into_owned())
        .collect();
    assert_eq!(refused, ["abs", "up"]);
    assert!(
        report
            .collisions
            .iter()
            .all(|c| c.reason.contains("outside the project")),
        "{:?}",
        report.collisions
    );
    assert!(!wt.path().join("ok.txt").exists());

    std::fs::remove_file(scratch.path().join("up")).unwrap();
    std::fs::remove_file(scratch.path().join("abs")).unwrap();
    for bad in ["..", "../x", "/tmp", ".git", "a/.GIT/b"] {
        let err = copy_into(&scratch, &wt, bad, false).await.unwrap_err();
        assert!(matches!(err, WorkspaceError::Invalid(_)), "{bad}: {err:?}");
    }
    assert!(!wt.path().join("ok.txt").exists());
    // Into a directory of the repository.
    let report = copy_into(&scratch, &wt, "apps/fib", false).await.unwrap();
    assert!(report.collisions.is_empty(), "{report:?}");
    assert_eq!(
        std::fs::read_to_string(wt.path().join("apps/fib/ok.txt")).unwrap(),
        "ok\n"
    );
}

#[tokio::test]
async fn a_scratch_project_never_copies_a_dot_git_it_does_not_have() {
    // A nested repository is a directory in the listing, and a directory is not copied.
    let tmp = tempfile::tempdir().unwrap();
    let (scratch, wt) = scratch_and_worktree(tmp.path()).await;
    put(scratch.path(), "ok.txt", "ok\n");
    let nested = scratch.path().join("vendor/dep");
    std::fs::create_dir_all(&nested).unwrap();
    git(&nested, &["init", "--quiet", "--initial-branch=main"]);
    put(&nested, "x.txt", "x\n");
    let report = copy_into(&scratch, &wt, ".", false).await.unwrap();
    assert!(report.collisions.is_empty(), "{report:?}");
    assert!(!wt.path().join("vendor/dep/.git").exists());
}

#[tokio::test]
async fn a_legacy_worktree_is_a_slot_that_a_new_repository_joins() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, old) = remote_with(tmp.path(), "acme", "old", &[("old.txt", "old\n")]);
    let (_, new) = remote_with(tmp.path(), "acme", "new", &[("new.txt", "new\n")]);
    let ws = workspaces(tmp.path());
    let legacy = ws.prepare(&old, RUN).await.unwrap();
    assert_eq!(legacy.dir(), "old");
    assert_eq!(legacy.path(), tmp.path().join("root/worktrees").join(RUN));
    std::fs::write(legacy.path().join("wip.txt"), "wip\n").unwrap();

    let run = ws.run(RUN).unwrap();
    let slots = run.slots().await.unwrap();
    assert_eq!(slots.len(), 1);
    assert_eq!((slots[0].dir(), slots[0].seq()), ("old", 0));
    assert_eq!(slots[0].path(), legacy.path());

    // The same repository again: the legacy slot, not a second one.
    let again = run.add_repository(&old).await.unwrap();
    assert_eq!(again.path(), legacy.path());
    assert_eq!(run.slots().await.unwrap().len(), 1);
    // Another: a slot of the new layout, after it.
    let joined = run.add_repository(&new).await.unwrap();
    assert_eq!((joined.dir(), joined.seq()), ("new", 1));
    assert_eq!(joined.path(), run.path().join("new"));
    let order: Vec<_> = run
        .slots_in_join_order()
        .await
        .unwrap()
        .iter()
        .map(|s| s.dir().to_owned())
        .collect();
    assert_eq!(order, ["old", "new"]);
    assert_eq!(run.slot_for(&old).await.unwrap().unwrap().seq(), 0);
    // The legacy helper still finds the legacy worktree.
    assert_eq!(
        ws.open_existing(RUN).await.unwrap().unwrap().path(),
        legacy.path()
    );
    // A repository named like the legacy one gets the owner in its directory.
    let (_, same_name) = remote_with(tmp.path(), "zed", "old", &[("z.txt", "z\n")]);
    assert!(
        run.add_repository(&same_name)
            .await
            .unwrap()
            .dir()
            .starts_with("old-")
    );

    // Everything goes together, the legacy worktree and its metadata included.
    run.remove().await.unwrap();
    assert!(run.slots().await.unwrap().is_empty());
    assert!(!legacy.path().exists());
    assert!(
        !tmp.path()
            .join("root/meta")
            .join(format!("{RUN}.json"))
            .exists()
    );
    assert!(ws.open_existing(RUN).await.unwrap().is_none());
    assert!(ws.runs().await.unwrap().is_empty());
}

#[tokio::test]
async fn removing_a_workspace_deletes_its_files_keeps_its_branches_and_can_be_repeated() {
    let tmp = tempfile::tempdir().unwrap();
    let (remote, repo) = remote_with(tmp.path(), "acme", "lib", &[("lib.txt", "lib\n")]);
    let ws = workspaces(tmp.path());
    let run = ws.run(RUN).unwrap();
    let slot = run.add_repository(&repo).await.unwrap();
    let wt = slot.worktree().unwrap().clone();
    std::fs::write(wt.path().join("unpushed.txt"), "u\n").unwrap();
    let sha = wt.commit_all("unpushed", &me()).await.unwrap().unwrap();
    let scratch = run.add_scratch("fib", &me()).await.unwrap();
    std::fs::write(scratch.path().join("f.txt"), "f\n").unwrap();

    run.remove().await.unwrap();
    assert!(!run.path().exists(), "the directory of the workspace");
    assert!(
        !tmp.path().join("root/meta").join(RUN).exists(),
        "its metadata"
    );
    assert!(
        !tmp.path()
            .join("root/workspaces")
            .join(format!("{RUN}.lock"))
            .exists(),
        "its lock"
    );
    assert!(run.slots().await.unwrap().is_empty());
    // The commit that was never pushed is still in the mirror, on the run's branch.
    let mirror = tmp
        .path()
        .join("root")
        .join(repo.locate().unwrap().mirror_relative());
    assert_eq!(
        git(&mirror, &["rev-parse", "refs/heads/agent/slots-ru"]),
        sha
    );
    assert!(!git(&mirror, &["worktree", "list", "--porcelain"]).contains("workspaces"));
    assert_eq!(remote_refs_of(&remote), ["refs/heads/main"]);

    run.remove().await.unwrap();
    ws.remove(RUN).await.unwrap();
    // Added again, the branch is attached to a new worktree with its commit.
    let back = run.add_repository(&repo).await.unwrap();
    assert_eq!(git(back.path(), &["rev-parse", "HEAD"]), sha);
}

#[tokio::test]
async fn a_workspace_with_unreadable_metadata_is_still_removed() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, repo) = remote_with(tmp.path(), "acme", "lib", &[("lib.txt", "lib\n")]);
    let ws = workspaces(tmp.path());
    let run = ws.run(RUN).unwrap();
    let slot = run.add_repository(&repo).await.unwrap();
    std::fs::write(
        tmp.path().join("root/meta").join(RUN).join("lib.json"),
        "not json",
    )
    .unwrap();
    assert!(matches!(
        run.slots().await.unwrap_err(),
        WorkspaceError::Corrupt(_)
    ));
    run.remove().await.unwrap();
    assert!(!slot.path().exists());
    assert!(!tmp.path().join("root/meta").join(RUN).exists());
}

#[tokio::test]
async fn runs_lists_every_run_that_has_a_workspace() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, repo) = remote_with(tmp.path(), "acme", "lib", &[("lib.txt", "lib\n")]);
    let ws = workspaces(tmp.path());
    assert!(ws.runs().await.unwrap().is_empty(), "nothing on disk yet");
    ws.prepare(&repo, "run-legacy-001").await.unwrap();
    ws.run("run-slots-0002")
        .unwrap()
        .add_repository(&repo)
        .await
        .unwrap();
    ws.run("run-scratch-03")
        .unwrap()
        .add_scratch("fib", &me())
        .await
        .unwrap();
    // A run whose workspace is partly gone: only its metadata is left.
    let half = tmp.path().join("root/meta/run-half-00004");
    std::fs::create_dir_all(&half).unwrap();
    // Files that are not runs: a lock, a temporary file, something with a bad name.
    std::fs::write(tmp.path().join("root/meta/stray.txt"), "x").unwrap();
    std::fs::create_dir_all(tmp.path().join("root/workspaces/.hidden")).unwrap();
    assert_eq!(
        ws.runs().await.unwrap(),
        [
            "run-half-00004",
            "run-legacy-001",
            "run-scratch-03",
            "run-slots-0002"
        ]
    );
    ws.remove("run-half-00004").await.unwrap();
    ws.remove("run-slots-0002").await.unwrap();
    assert_eq!(
        ws.runs().await.unwrap(),
        ["run-legacy-001", "run-scratch-03"]
    );
}

/// Two `Workspaces` on one root stand for two worker processes on a shared volume. Slots join one
/// run from both while other runs take the same repositories: every slot gets its own directory and
/// its own place in the order, and no git lock trips.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_workspaces_on_one_root_add_slots_without_clashing() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = workspaces(tmp.path());
    let other = Workspaces::new(tmp.path().join("root"), Arc::new(StaticToken::new(TOKEN)));
    let repos: Vec<RepoRef> = (0..6)
        .map(|i| remote_with(tmp.path(), "acme", &format!("repo{i}"), &[("f.txt", "f\n")]).1)
        .collect();
    let mut tasks = Vec::new();
    for (i, repo) in repos.iter().enumerate() {
        for (n, handle) in [&ws, &other].into_iter().enumerate() {
            let (handle, repo) = (handle.clone(), repo.clone());
            tasks.push(tokio::spawn(async move {
                // The same run from both handles, and another run of its own beside it.
                let shared = handle.run("shared-run-0001")?.add_repository(&repo).await?;
                let own = handle
                    .run(&format!("own-run-{i}-{n}"))?
                    .add_repository(&repo)
                    .await?;
                Ok::<_, WorkspaceError>((shared.dir().to_owned(), shared.seq(), own.seq()))
            }));
        }
    }
    let mut dirs = Vec::new();
    let mut seqs = Vec::new();
    for task in tasks {
        let (dir, seq, own) = task
            .await
            .unwrap()
            .expect("no clash, no \"could not lock\"");
        assert_eq!(own, 1);
        dirs.push(dir);
        seqs.push(seq);
    }
    dirs.sort();
    dirs.dedup();
    assert_eq!(
        dirs.len(),
        6,
        "one slot per repository, however often it was asked for: {dirs:?}"
    );
    seqs.sort_unstable();
    seqs.dedup();
    assert_eq!(
        seqs,
        [1, 2, 3, 4, 5, 6],
        "every slot has its own place in the order"
    );
    let slots = ws.run("shared-run-0001").unwrap().slots().await.unwrap();
    assert_eq!(slots.len(), 6);
    for repo in &repos {
        let mirror = tmp
            .path()
            .join("root")
            .join(repo.locate().unwrap().mirror_relative());
        git(&mirror, &["fsck", "--strict", "--no-dangling"]);
    }
}

// ---------------------------------------------------------------------------------------------
// The coder's git works where its own metadata says, not where files a run writes say (ADR 0041)
// ---------------------------------------------------------------------------------------------

/// A program that records that it ran (`<dir>/markers/<its first argument>`) and then behaves as a
/// `filter.*.clean` (copies stdin to stdout) or a `core.fsmonitor` (fails, which git tolerates)
/// would. Configured in a repository the run controls, it stands for any command a run can make the
/// coder's git run.
struct Planted {
    dir: PathBuf,
}

impl Planted {
    fn new(under: &Path) -> Self {
        let dir = under.join("spy");
        std::fs::create_dir_all(dir.join("markers")).unwrap();
        let script = dir.join("spy.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$1\" > {}/markers/\"$1\"\ncat\n",
                dir.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir }
    }

    /// `<spy> <name>`: the command a configuration key names.
    fn command(&self, name: &str) -> String {
        format!("{} {name}", self.dir.join("spy.sh").display())
    }

    /// The names of the markers left since the last call, sorted; they are cleared.
    fn ran(&self) -> Vec<String> {
        let dir = self.dir.join("markers");
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        for name in &names {
            std::fs::remove_file(dir.join(name)).unwrap();
        }
        names.sort();
        names
    }

    /// A repository, bare or not, whose configuration runs the spy as a clean filter and as the
    /// file system monitor, and whose attributes apply the filter to markdown files.
    fn plant(&self, repo: &Path, bare: bool) {
        std::fs::create_dir_all(repo).unwrap();
        let mut init = vec!["init", "--quiet", "--initial-branch=main"];
        if bare {
            init.push("--bare");
        }
        git(repo, &init);
        let dir = if bare {
            repo.to_owned()
        } else {
            repo.join(".git")
        };
        git(
            repo,
            &["config", "filter.spy.clean", &self.command("filter")],
        );
        git(
            repo,
            &["config", "core.fsmonitor", &self.command("fsmonitor")],
        );
        std::fs::create_dir_all(dir.join("info")).unwrap();
        std::fs::write(dir.join("info/attributes"), "*.md filter=spy\n").unwrap();
    }
}

/// A run's worktree with `README.md` changed (so that `add -A` reads it) and the attributes that
/// would apply a filter named `spy` to it.
async fn worktree_with_a_change(env: &Env, run: &str) -> adam_workspace::Slot {
    let slot = env
        .ws
        .run(run)
        .unwrap()
        .add_repository(&env.repo)
        .await
        .unwrap();
    std::fs::write(slot.path().join(".gitattributes"), "*.md filter=spy\n").unwrap();
    std::fs::write(slot.path().join("README.md"), format!("changed by {run}\n")).unwrap();
    slot
}

/// What the coder does to a worktree that reads the repository's configuration and would run what
/// it names: `status`, `diff_stat` (`add -A --intent-to-add` in a copy of the index) and
/// `commit_all` (`add -A`, `diff --cached`, `commit`). All must work.
async fn the_coders_git_works_on(slot: &adam_workspace::Slot) {
    let wt = slot.worktree().unwrap();
    let status = wt.status().await.unwrap();
    assert!(status.iter().any(|f| f.path == "README.md"), "{status:?}");
    wt.diff_stat().await.unwrap();
    let sha = wt.commit_all("a change", &me()).await.unwrap();
    assert!(sha.is_some(), "the change was committed");
}

/// `git add -A` as a person would run it in the worktree: it finds the repository through the files
/// in the slot, and runs what that repository's configuration names.
fn git_as_discovered(slot: &adam_workspace::Slot) {
    Command::new("git")
        .current_dir(slot.path())
        .args(["-c", "core.hooksPath=/dev/null", "add", "-A"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
}

fn meta_json(env: &Env, run: &str, slot: &adam_workspace::Slot) -> serde_json::Value {
    let path = env
        .root
        .join("meta")
        .join(run)
        .join(format!("{}.json", slot.dir()));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[tokio::test]
async fn the_worktrees_git_directory_is_recorded_by_the_coder_and_each_run_has_its_own() {
    let env = Env::new();
    let run_a = env.ws.run("run-aaaaaaaa-1").unwrap();
    let a = run_a.add_repository(&env.repo).await.unwrap();
    let b = env
        .ws
        .run("run-bbbbbbbb-2")
        .unwrap()
        .add_repository(&env.repo)
        .await
        .unwrap();
    let (da, db) = (
        a.worktree().unwrap().git_dir().unwrap().to_owned(),
        b.worktree().unwrap().git_dir().unwrap().to_owned(),
    );
    assert_ne!(
        da, db,
        "git names the second worktree of a mirror differently"
    );
    for (slot, dir) in [(&a, &da), (&b, &db)] {
        assert_eq!(dir.parent().unwrap(), env.mirror().join("worktrees"));
        // What git wrote in the `.git` file is the same directory.
        let text = std::fs::read_to_string(slot.path().join(".git")).unwrap();
        assert_eq!(text.trim(), format!("gitdir: {}", dir.display()));
        // The pin is that directory, the mirror and the slot.
        let pin = slot.git_pin().unwrap();
        assert_eq!(pin.git_dir(), dir);
        assert_eq!(pin.common_dir(), env.mirror());
        assert_eq!(pin.work_tree(), slot.path());
    }
    // It is in the coder's metadata, which a pod never mounts.
    let meta = meta_json(&env, "run-aaaaaaaa-1", &a);
    assert_eq!(
        meta["admin"].as_str().unwrap(),
        da.file_name().unwrap().to_str().unwrap()
    );
    assert_eq!(
        meta["version"], 2,
        "no new metadata version: the field is optional"
    );
    // And it is found again after a restart, from the metadata.
    let again = Workspaces::new(env.root.clone(), Arc::new(StaticToken::new(TOKEN)));
    let slots = again.run("run-aaaaaaaa-1").unwrap().slots().await.unwrap();
    assert_eq!(slots[0].worktree().unwrap().git_dir().unwrap(), da);
}

#[tokio::test]
async fn a_run_that_points_its_git_file_at_another_runs_directory_does_not_get_it() {
    let env = Env::new();
    let a = worktree_with_a_change(&env, "run-aaaaaaaa-1").await;
    let b = env
        .ws
        .run("run-bbbbbbbb-2")
        .unwrap()
        .add_repository(&env.repo)
        .await
        .unwrap();
    let (da, db) = (
        a.worktree().unwrap().git_dir().unwrap().to_owned(),
        b.worktree().unwrap().git_dir().unwrap().to_owned(),
    );
    // A's commands write A's `.git` file, naming B's directory in the mirror.
    std::fs::write(a.path().join(".git"), format!("gitdir: {}\n", db.display())).unwrap();

    // The coder takes A's directory from its own record, as it did and as a restart will.
    let again = Workspaces::new(env.root.clone(), Arc::new(StaticToken::new(TOKEN)));
    let slots = again.run("run-aaaaaaaa-1").unwrap().slots().await.unwrap();
    assert_eq!(slots[0].worktree().unwrap().git_dir().unwrap(), da);
    assert_eq!(slots[0].git_pin().unwrap().git_dir(), da);

    // And git acts on A's branch, not on B's.
    let branch = |wt: &adam_workspace::Worktree| format!("refs/heads/{}", wt.branch());
    let b_wt = b.worktree().unwrap();
    let b_before = git(&env.mirror(), &["rev-parse", &branch(b_wt)]);
    let b_index = std::fs::read(db.join("index")).unwrap();
    let a_wt = slots[0].worktree().unwrap();
    let sha = a_wt.commit_all("A's change", &me()).await.unwrap().unwrap();
    assert_eq!(git(&env.mirror(), &["rev-parse", &branch(a_wt)]), sha);
    assert_eq!(
        git(&env.mirror(), &["show", &format!("{sha}:README.md")]),
        "changed by run-aaaaaaaa-1",
        "the commit holds A's files, from A's index and not B's"
    );
    assert_eq!(
        git(&env.mirror(), &["rev-parse", &branch(b_wt)]),
        b_before,
        "B's branch did not move"
    );
    assert_eq!(
        std::fs::read(db.join("index")).unwrap(),
        b_index,
        "B's index was not written"
    );
}

#[tokio::test]
async fn a_slot_made_before_the_directory_was_recorded_is_resolved_from_the_mirror_only_when_that_is_unambiguous()
 {
    let env = Env::new();
    let a = worktree_with_a_change(&env, "run-aaaaaaaa-1").await;
    let da = a.worktree().unwrap().git_dir().unwrap().to_owned();
    let meta_path = env
        .root
        .join("meta/run-aaaaaaaa-1")
        .join(format!("{}.json", a.dir()));
    let forget = || {
        let mut meta = meta_json(&env, "run-aaaaaaaa-1", &a);
        meta.as_object_mut().unwrap().remove("admin");
        std::fs::write(&meta_path, serde_json::to_vec_pretty(&meta).unwrap()).unwrap();
    };
    let reopen = || async {
        let ws = Workspaces::new(env.root.clone(), Arc::new(StaticToken::new(TOKEN)));
        ws.run("run-aaaaaaaa-1").unwrap().slots().await.unwrap()
    };

    // The one directory whose `gitdir` names this slot's `.git`: found, and recorded.
    forget();
    let slots = reopen().await;
    assert_eq!(slots[0].worktree().unwrap().git_dir().unwrap(), da);
    assert_eq!(
        meta_json(&env, "run-aaaaaaaa-1", &a)["admin"],
        da.file_name().unwrap().to_str().unwrap()
    );
    the_coders_git_works_on(&slots[0]).await;

    // Two directories name it (a run's commands can add a candidate by writing their own `gitdir`):
    // none is taken, git is not run on the slot, and nothing is recorded.
    forget();
    let forged = env.mirror().join("worktrees/forged");
    std::fs::create_dir_all(&forged).unwrap();
    std::fs::write(
        forged.join("gitdir"),
        format!("{}\n", a.path().join(".git").display()),
    )
    .unwrap();
    let slots = reopen().await;
    let wt = slots[0].worktree().unwrap();
    assert!(wt.git_dir().is_none() && wt.git_pin().is_none() && slots[0].git_pin().is_none());
    let refused = wt.status().await.unwrap_err();
    assert!(matches!(refused, WorkspaceError::Corrupt(_)), "{refused}");
    assert!(
        refused.to_string().contains("cannot tell which directory"),
        "{refused}"
    );
    assert!(meta_json(&env, "run-aaaaaaaa-1", &a).get("admin").is_none());

    // None names it (its `gitdir` file is gone): the same.
    std::fs::remove_dir_all(&forged).unwrap();
    std::fs::remove_file(da.join("gitdir")).unwrap();
    let slots = reopen().await;
    let wt = slots[0].worktree().unwrap();
    assert!(wt.git_dir().is_none());
    assert!(matches!(
        wt.commit_all("x", &me()).await.unwrap_err(),
        WorkspaceError::Corrupt(_)
    ));
}

#[tokio::test]
async fn a_commondir_or_a_git_file_a_run_rewrites_steers_nothing_of_the_coders_git() {
    let env = Env::new();
    let spy = Planted::new(env._tmp.path());
    let slot = worktree_with_a_change(&env, "run-aaaaaaaa-1").await;
    let admin = slot.worktree().unwrap().git_dir().unwrap().to_owned();

    // The control: git as a person would run it finds the configuration through the files, and
    // the planted command runs. Without this the rest would prove nothing.
    let fake = env._tmp.path().join("fake.git");
    spy.plant(&fake, true);
    std::fs::write(admin.join("commondir"), format!("{}\n", fake.display())).unwrap();
    git_as_discovered(&slot);
    assert!(
        spy.ran().contains(&"filter".to_owned()),
        "the control ran the filter"
    );

    // The coder's git, with the `commondir` of the worktree's directory pointing at that repository.
    // (The control put the files in the other repository's objects and their entries in this index:
    // the index is made `HEAD`'s again, and the file is written again, so that the coder's `add` has
    // to read them.)
    std::fs::write(admin.join("commondir"), "../..\n").unwrap();
    git(slot.path(), &["read-tree", "HEAD"]);
    std::fs::write(admin.join("commondir"), format!("{}\n", fake.display())).unwrap();
    std::fs::write(
        slot.path().join("README.md"),
        "changed after the control, and longer\n",
    )
    .unwrap();
    // Git's reference store follows this file even when the common directory is named, so the
    // coder does not run git at all, and does not put the file right either (it would have to open
    // a file a run can replace by a link): a run pod cannot write it.
    let wt = slot.worktree().unwrap();
    for refused in [
        wt.status().await.map(drop),
        wt.diff_stat().await.map(drop),
        wt.commit_all("x", &me()).await.map(drop),
    ] {
        let refused = refused.unwrap_err();
        assert!(matches!(refused, WorkspaceError::Corrupt(_)), "{refused}");
    }
    assert_eq!(spy.ran(), Vec::<String>::new(), "commondir rewritten");
    assert_eq!(
        std::fs::read_to_string(admin.join("commondir")).unwrap(),
        format!("{}\n", fake.display()),
        "refused, not rewritten"
    );
    std::fs::write(admin.join("commondir"), "../..\n").unwrap();
    the_coders_git_works_on(&slot).await;
    assert_eq!(spy.ran(), Vec::<String>::new(), "commondir as git wrote it");
    std::fs::write(
        slot.path().join("README.md"),
        "changed again after, and longer still\n",
    )
    .unwrap();

    // The same with the slot's `.git` file naming a repository with a working tree of its own.
    let fake_project = env._tmp.path().join("fake-project");
    spy.plant(&fake_project, false);
    git(
        &fake_project,
        &[
            "-c",
            "core.fsmonitor=false",
            "commit",
            "--allow-empty",
            "--quiet",
            "-m",
            "f",
        ],
    );
    std::fs::write(
        slot.path().join(".git"),
        format!("gitdir: {}\n", fake_project.join(".git").display()),
    )
    .unwrap();
    std::fs::write(slot.path().join("README.md"), "changed again\n").unwrap();
    git_as_discovered(&slot);
    assert!(
        spy.ran().contains(&"filter".to_owned()),
        "the control ran the filter"
    );
    std::fs::write(slot.path().join("README.md"), "changed once more\n").unwrap();
    the_coders_git_works_on(&slot).await;
    assert_eq!(spy.ran(), Vec::<String>::new(), ".git file rewritten");

    // Asking for the worktree again reads the files git reads (`rev-parse` checks that the slot is
    // still a worktree of its mirror): it may say the slot is not, and it runs nothing.
    let _ = env
        .ws
        .run("run-aaaaaaaa-1")
        .unwrap()
        .add_repository(&env.repo)
        .await;
    assert_eq!(spy.ran(), Vec::<String>::new(), "add_repository again");
}

#[tokio::test]
async fn a_worktree_configuration_file_is_never_read_because_the_coder_never_turns_it_on() {
    let env = Env::new();
    let spy = Planted::new(env._tmp.path());
    let slot = worktree_with_a_change(&env, "run-aaaaaaaa-1").await;
    let admin = slot.worktree().unwrap().git_dir().unwrap().to_owned();
    let mirror = env.mirror();

    // What the workspace makes: a repository of format 0 that does not turn the extension on, so
    // the `config.worktree` of a worktree's directory, which a run can write, is not read.
    let config = |key: &str| {
        Command::new("git")
            .args(["config", "--file"])
            .arg(mirror.join("config"))
            .args(["--get", key])
            .output()
            .unwrap()
    };
    assert_eq!(
        String::from_utf8_lossy(&config("core.repositoryformatversion").stdout).trim(),
        "0"
    );
    assert!(config("extensions.worktreeConfig").stdout.is_empty());
    git(
        &admin,
        &[
            "config",
            "--file",
            "config.worktree",
            "filter.spy.clean",
            &spy.command("worktree-config"),
        ],
    );
    the_coders_git_works_on(&slot).await;
    assert_eq!(
        spy.ran(),
        Vec::<String>::new(),
        "config.worktree is not read"
    );

    // Whoever turns it on (a command of a run that shares the coder's container, on a mirror it can
    // write) is undone the next time the workspace works on the mirror: it asks for a worktree.
    git(&mirror, &["config", "core.repositoryformatversion", "1"]);
    git(&mirror, &["config", "extensions.worktreeConfig", "true"]);
    let other = env
        .ws
        .run("run-bbbbbbbb-2")
        .unwrap()
        .add_repository(&env.repo)
        .await
        .unwrap();
    assert!(other.worktree().is_some());
    assert!(
        config("extensions.worktreeConfig").stdout.is_empty(),
        "the extension is turned off again"
    );
}

#[tokio::test]
async fn a_head_a_run_points_at_another_runs_branch_is_not_where_the_coder_commits() {
    let env = Env::new();
    let a = worktree_with_a_change(&env, "run-aaaaaaaa-1").await;
    let b = env
        .ws
        .run("run-bbbbbbbb-2")
        .unwrap()
        .add_repository(&env.repo)
        .await
        .unwrap();
    let (a_wt, b_wt) = (a.worktree().unwrap(), b.worktree().unwrap());
    let branch = |wt: &adam_workspace::Worktree| format!("refs/heads/{}", wt.branch());
    let (a_before, b_before) = (
        git(&env.mirror(), &["rev-parse", &branch(a_wt)]),
        git(&env.mirror(), &["rev-parse", &branch(b_wt)]),
    );
    // A's commands write the HEAD of A's directory in the mirror: it names B's branch.
    std::fs::write(
        a_wt.git_dir().unwrap().join("HEAD"),
        format!("ref: {}\n", branch(b_wt)),
    )
    .unwrap();

    let sha = a_wt.commit_all("A's change", &me()).await.unwrap().unwrap();
    assert_eq!(
        git(&env.mirror(), &["rev-parse", &branch(a_wt)]),
        sha,
        "the commit is on A's own branch, which it extends"
    );
    assert_eq!(
        git(&env.mirror(), &["rev-parse", &format!("{sha}^")]),
        a_before
    );
    assert_eq!(
        git(&env.mirror(), &["rev-parse", &branch(b_wt)]),
        b_before,
        "B's branch did not move"
    );
    // Nothing more to commit is nothing, as for `git commit`.
    assert_eq!(a_wt.commit_all("again", &me()).await.unwrap(), None);
}

#[tokio::test]
async fn a_scratch_projects_git_is_its_own_directory_and_nothing_a_command_put_there_redirects_it()
{
    let env = Env::new();
    let run = env.ws.run("run-aaaaaaaa-1").unwrap();
    let scratch = run.add_scratch("notes", &me()).await.unwrap();
    let project = scratch.scratch().unwrap();
    std::fs::write(project.path().join("a.txt"), "a\n").unwrap();
    assert!(project.commit_all("first", &me()).await.unwrap().is_some());
    let pin = scratch.git_pin().unwrap();
    assert_eq!(pin.git_dir(), project.path().join(".git"));
    assert_eq!(pin.common_dir(), project.path().join(".git"));

    // A `commondir` file in the project's repository would move its refs elsewhere: git is not run.
    let elsewhere = env._tmp.path().join("elsewhere.git");
    git(
        env._tmp.path(),
        &["init", "--bare", "--quiet", elsewhere.to_str().unwrap()],
    );
    std::fs::write(
        project.path().join(".git/commondir"),
        format!("{}\n", elsewhere.display()),
    )
    .unwrap();
    std::fs::write(project.path().join("a.txt"), "b\n").unwrap();
    let refused = project.commit_all("second", &me()).await.unwrap_err();
    assert!(matches!(refused, WorkspaceError::Corrupt(_)), "{refused}");
    assert!(
        project.path().join(".git/commondir").exists(),
        "refused, not removed"
    );
    assert!(
        git(&elsewhere, &["for-each-ref"]).is_empty(),
        "nothing was written there"
    );
    std::fs::remove_file(project.path().join(".git/commondir")).unwrap();
    assert!(project.commit_all("second", &me()).await.unwrap().is_some());

    // A `.git` that is a file naming another run's worktree directory: git is not run.
    let b = env
        .ws
        .run("run-bbbbbbbb-2")
        .unwrap()
        .add_repository(&env.repo)
        .await
        .unwrap();
    let b_wt = b.worktree().unwrap();
    let b_tip = git(
        &env.mirror(),
        &["rev-parse", &format!("refs/heads/{}", b_wt.branch())],
    );
    std::fs::remove_dir_all(project.path().join(".git")).unwrap();
    std::fs::write(
        project.path().join(".git"),
        format!("gitdir: {}\n", b_wt.git_dir().unwrap().display()),
    )
    .unwrap();
    std::fs::write(project.path().join("a.txt"), "c\n").unwrap();
    let refused = project.commit_all("third", &me()).await.unwrap_err();
    assert!(matches!(refused, WorkspaceError::Corrupt(_)), "{refused}");
    assert_eq!(
        git(
            &env.mirror(),
            &["rev-parse", &format!("refs/heads/{}", b_wt.branch())]
        ),
        b_tip,
        "B's branch did not move"
    );
}

// ---------------------------------------------------------------------------------------------
// What the coder opens in a directory a run can write is never followed anywhere (ADR 0041)
// ---------------------------------------------------------------------------------------------

const VICTIM: &str = "not the coder's to write\n";

/// A file the coder must never write to or copy from on a run's behalf.
fn victim(env: &Env, name: &str) -> PathBuf {
    let path = env._tmp.path().join(name);
    std::fs::write(&path, VICTIM).unwrap();
    path
}

fn symlink_over(target: &Path, link: &Path) {
    let _ = std::fs::remove_file(link);
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[tokio::test]
async fn a_commondir_that_is_a_link_is_refused_and_nothing_is_written_through_it() {
    let env = Env::new();
    let slot = worktree_with_a_change(&env, "run-aaaaaaaa-1").await;
    let wt = slot.worktree().unwrap();
    let admin = wt.git_dir().unwrap().to_owned();
    let secret = victim(&env, "victim.txt");

    symlink_over(&secret, &admin.join("commondir"));
    for refused in [
        wt.status().await.map(drop),
        wt.diff_stat().await.map(drop),
        wt.commit_all("x", &me()).await.map(drop),
    ] {
        let refused = refused.unwrap_err();
        assert!(matches!(refused, WorkspaceError::Corrupt(_)), "{refused}");
        assert!(refused.to_string().contains("commondir"), "{refused}");
    }
    assert_eq!(
        std::fs::read_to_string(&secret).unwrap(),
        VICTIM,
        "the coder wrote through the link"
    );

    // A file with other content is refused too, and not put right: the pod cannot write it, so a
    // run that did is not on the pod's mounts, and the coder does not rewrite what it did not make.
    std::fs::remove_file(admin.join("commondir")).unwrap();
    std::fs::write(admin.join("commondir"), "/somewhere/else\n").unwrap();
    assert!(wt.status().await.is_err());
    assert_eq!(
        std::fs::read_to_string(admin.join("commondir")).unwrap(),
        "/somewhere/else\n",
        "refused, not rewritten"
    );
    // What git wrote is accepted.
    std::fs::write(admin.join("commondir"), "../..\n").unwrap();
    the_coders_git_works_on(&slot).await;
}

#[tokio::test]
async fn a_commondir_that_is_a_pipe_is_refused_and_does_not_hang_the_coder() {
    let env = Env::new();
    let slot = worktree_with_a_change(&env, "run-aaaaaaaa-1").await;
    let wt = slot.worktree().unwrap();
    let fifo = wt.git_dir().unwrap().join("commondir");
    std::fs::remove_file(&fifo).unwrap();
    assert!(
        Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), wt.status()).await;
    // Let a reader that waits for a writer (a coder that opens the pipe) end, so that the test
    // reports the failure and does not hang with it.
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(0o4000)
            .open(&fifo);
    }
    let refused = result.expect("the coder waited on a pipe").unwrap_err();
    assert!(matches!(refused, WorkspaceError::Corrupt(_)), "{refused}");
}

#[tokio::test]
async fn a_temporary_index_is_not_a_name_a_run_can_plant_a_link_at() {
    let env = Env::new();
    let slot = worktree_with_a_change(&env, "run-aaaaaaaa-1").await;
    let wt = slot.worktree().unwrap();
    let admin = wt.git_dir().unwrap().to_owned();
    let secret = victim(&env, "victim.txt");
    // The name the copy of the index used to have: `index.adam-diff-<pid>-<n>`.
    for n in 0..256 {
        symlink_over(
            &secret,
            &admin.join(format!("index.adam-diff-{}-{n}", std::process::id())),
        );
    }
    wt.diff_stat().await.unwrap();
    assert_eq!(
        std::fs::read_to_string(&secret).unwrap(),
        VICTIM,
        "the copy of the index was written through a link a run planted"
    );
    let names: Vec<String> = std::fs::read_dir(&admin)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with("index.adam-diff-"))
        .collect();
    assert!(
        names.iter().all(|n| !n.contains("adam-")),
        "no copy of the index is left in the directory a run writes: {names:?}"
    );
}

#[tokio::test]
async fn an_index_that_is_a_link_is_refused_and_nothing_is_copied_from_it() {
    let env = Env::new();
    let slot = worktree_with_a_change(&env, "run-aaaaaaaa-1").await;
    let wt = slot.worktree().unwrap();
    let admin = wt.git_dir().unwrap().to_owned();
    // The coder's own secrets stand for any file it can read: the App key, the token.
    let secret = victim(&env, "app-key.pem");
    symlink_over(&secret, &admin.join("index"));
    let refused = wt.diff_stat().await.unwrap_err();
    assert!(
        refused.to_string().contains("not a plain file"),
        "the index is read through no link: {refused}"
    );
    assert_eq!(std::fs::read_to_string(&secret).unwrap(), VICTIM);

    // Another repository's index, which the link would make the coder's: refused, not used.
    let other = env._tmp.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    git(&other, &["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(other.join("elsewhere.txt"), "e\n").unwrap();
    git(&other, &["add", "-A"]);
    symlink_over(&other.join(".git/index"), &admin.join("index"));
    let refused = wt.diff_stat().await.unwrap_err();
    assert!(
        refused.to_string().contains("not a plain file"),
        "{refused}"
    );
}

#[tokio::test]
async fn a_slot_that_is_a_link_to_another_runs_slot_is_not_a_slot() {
    let env = Env::new();
    let a = env
        .ws
        .run("run-aaaaaaaa-1")
        .unwrap()
        .add_repository(&env.repo)
        .await
        .unwrap();
    let b = env
        .ws
        .run("run-bbbbbbbb-2")
        .unwrap()
        .add_repository(&env.repo)
        .await
        .unwrap();
    std::fs::write(b.path().join(".env"), "TOKEN=b's\n").unwrap();
    std::fs::write(a.path().join("README.md"), "a's change\n").unwrap();
    let a_wt = a.worktree().unwrap().clone();
    let a_tip = git(
        &env.mirror(),
        &["rev-parse", &format!("refs/heads/{}", a_wt.branch())],
    );

    // A's commands move A's slot away and put a link to B's in its place. (B's directory is not
    // in A's pod, but the coder sees the whole volume.)
    std::fs::rename(a.path(), a.path().with_extension("away")).unwrap();
    std::os::unix::fs::symlink(b.path(), a.path()).unwrap();

    let again = Workspaces::new(env.root.clone(), Arc::new(StaticToken::new(TOKEN)));
    let run = again.run("run-aaaaaaaa-1").unwrap();
    assert!(
        run.slots().await.unwrap().is_empty(),
        "a link is not a slot"
    );
    assert!(run.slot(a.dir()).await.unwrap().is_none());
    // The worktree the coder holds does not act on what the link leads to either.
    let refused = a_wt.commit_all("A's", &me()).await.unwrap_err();
    assert!(matches!(refused, WorkspaceError::Corrupt(_)), "{refused}");
    assert!(a_wt.status().await.is_err());
    assert_eq!(
        git(
            &env.mirror(),
            &["rev-parse", &format!("refs/heads/{}", a_wt.branch())]
        ),
        a_tip,
        "A's branch did not move"
    );
    assert!(
        git(&env.mirror(), &["ls-tree", "-r", "--name-only", &a_tip])
            .lines()
            .all(|f| f != ".env"),
        "B's .env is not on A's branch"
    );
    // And it is not adopted when the repository is added again.
    assert!(run.add_repository(&env.repo).await.is_err());
}

// ---- A slot swapped between a check and the use that follows it (vymalo/another-adam-rs#125)
//
// A command in the run's pod can rename a slot's directory and put a link at its name at any
// moment. Each of these tests makes the swap happen at a `race::checkpoint`: where the coder has
// checked the slot and has not yet used it.

use adam_workspace::safe_fs::race;

/// Where a swapped slot's directory is moved to.
fn away_of(slot: &Path) -> PathBuf {
    slot.with_file_name(format!(
        "{}.away",
        slot.file_name().unwrap().to_string_lossy()
    ))
}

/// What a command of a run can do between a check and a use: move the slot's directory away and
/// put a link to another directory at its name.
fn swap_for_a_link(slot: &Path, other: &Path) {
    std::fs::rename(slot, away_of(slot)).unwrap();
    std::os::unix::fs::symlink(other, slot).unwrap();
}

/// … or a directory of the same name, which is not a link.
fn swap_for_a_directory(slot: &Path) {
    std::fs::rename(slot, away_of(slot)).unwrap();
    std::fs::create_dir(slot).unwrap();
}

/// The files below `dir`, relative and sorted.
fn files_under(dir: &Path) -> Vec<String> {
    fn walk(dir: &Path, at: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                walk(&path, at, out);
            } else {
                out.push(path.strip_prefix(at).unwrap().display().to_string());
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

#[tokio::test]
async fn copy_into_does_not_write_through_a_destination_slot_swapped_after_the_plan() {
    let tmp = tempfile::tempdir().unwrap();
    let (scratch, wt) = scratch_and_worktree(tmp.path()).await;
    put(scratch.path(), "fib.sh", "echo 1\n");
    put(scratch.path(), "docs/notes.md", "notes\n");
    // Another run's slot: the coder holds the whole volume, so it can write there.
    let other = tmp.path().join("another-runs-slot");
    put(&other, "SENTINEL", "theirs\n");

    let dest = wt.path().to_path_buf();
    let hook = race::on_checkpoint(&dest, {
        let (dest, other) = (dest.clone(), other.clone());
        move |at, _| {
            if at == "copy-dest" {
                swap_for_a_link(&dest, &other);
            }
        }
    });
    let report = copy_into(&scratch, &wt, ".", false).await;

    assert_eq!(hook.reached("copy-dest"), 1, "the swap was made");
    assert_eq!(
        files_under(&other),
        ["SENTINEL"],
        "nothing was written in the other run's slot"
    );
    // The copy went to the directory the coder had opened, which is now at the new name.
    let report = report.unwrap();
    assert_eq!(paths(&report.copied), ["docs/notes.md", "fib.sh"]);
    let away = away_of(&dest);
    assert_eq!(
        std::fs::read_to_string(away.join("fib.sh")).unwrap(),
        "echo 1\n"
    );
    assert_eq!(
        std::fs::read_to_string(away.join("docs/notes.md")).unwrap(),
        "notes\n"
    );
}

#[tokio::test]
async fn copy_into_does_not_copy_a_source_file_swapped_for_a_link_after_the_plan() {
    let tmp = tempfile::tempdir().unwrap();
    let (scratch, wt) = scratch_and_worktree(tmp.path()).await;
    put(scratch.path(), "fib.sh", "echo 1\n");
    put(scratch.path(), "docs/notes.md", "notes\n");
    let other = tmp.path().join("another-runs-slot");
    put(&other, "secret.txt", "TOP SECRET\n");

    let source = scratch.path().to_path_buf();
    let hook = race::on_checkpoint(&source, {
        let (source, secret) = (source.clone(), other.join("secret.txt"));
        move |at, _| {
            if at == "copy-source" {
                std::fs::remove_file(source.join("fib.sh")).unwrap();
                std::os::unix::fs::symlink(&secret, source.join("fib.sh")).unwrap();
            }
        }
    });
    let result = copy_into(&scratch, &wt, ".", false).await;

    assert_eq!(hook.reached("copy-source"), 1, "the swap was made");
    // Whatever the copy did with it, the repository got neither the secret nor a link to it.
    let landed = wt.path().join("fib.sh");
    if let Ok(meta) = std::fs::symlink_metadata(&landed) {
        assert!(
            !meta.file_type().is_symlink(),
            "a link to {}",
            std::fs::read_link(&landed).unwrap().display()
        );
        assert!(
            !std::fs::read_to_string(&landed)
                .unwrap()
                .contains("TOP SECRET")
        );
    }
    assert!(
        result.is_err(),
        "the file changed under the copy: {result:?}"
    );
}

#[tokio::test]
async fn copy_into_does_not_read_a_source_slot_swapped_after_the_plan() {
    let tmp = tempfile::tempdir().unwrap();
    let (scratch, wt) = scratch_and_worktree(tmp.path()).await;
    put(scratch.path(), "fib.sh", "echo 1\n");
    let other = tmp.path().join("another-runs-project");
    put(&other, "fib.sh", "THEIRS\n");

    let source = scratch.path().to_path_buf();
    let hook = race::on_checkpoint(&source, {
        let (source, other) = (source.clone(), other.clone());
        move |at, _| {
            if at == "copy-source" {
                swap_for_a_link(&source, &other);
            }
        }
    });
    let result = copy_into(&scratch, &wt, ".", false).await;

    assert_eq!(hook.reached("copy-source"), 1, "the swap was made");
    // The project's own file is copied, or nothing is: never the other run's.
    if let Ok(copied) = std::fs::read_to_string(wt.path().join("fib.sh")) {
        assert_eq!(copied, "echo 1\n");
    }
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        std::fs::read_to_string(wt.path().join("fib.sh")).unwrap(),
        "echo 1\n"
    );
}

#[tokio::test]
async fn git_is_not_run_on_a_slot_replaced_by_a_directory_after_the_coder_opened_it() {
    let env = Env::new();
    let slot = env
        .ws
        .run("run-aaaaaaaa-1")
        .unwrap()
        .add_repository(&env.repo)
        .await
        .unwrap();
    let wt = slot.worktree().unwrap().clone();
    let pin = slot.git_pin().unwrap();
    let branch = format!("refs/heads/{}", wt.branch());
    let tip = git(&env.mirror(), &["rev-parse", &branch]);

    // The slot's directory is moved away and a directory (not a link: that one is refused by
    // looking at the name) is made at its name, empty: to `git add -A` it is a worktree in which
    // every file was deleted.
    swap_for_a_directory(slot.path());

    let refused = pin.check().await.expect_err("the slot was replaced");
    assert!(refused.contains("replaced"), "{refused}");
    let refused = wt.commit_all("delete everything", &me()).await.unwrap_err();
    assert!(matches!(refused, WorkspaceError::Corrupt(_)), "{refused}");
    assert_eq!(git(&env.mirror(), &["rev-parse", &branch]), tip);
}

#[tokio::test]
async fn git_is_not_started_on_a_slot_replaced_between_the_check_and_the_start() {
    let env = Env::new();
    let slot = env
        .ws
        .run("run-aaaaaaaa-1")
        .unwrap()
        .add_repository(&env.repo)
        .await
        .unwrap();
    let wt = slot.worktree().unwrap().clone();
    let branch = format!("refs/heads/{}", wt.branch());
    let tip = git(&env.mirror(), &["rev-parse", &branch]);

    let path = slot.path().to_path_buf();
    let hook = race::on_checkpoint(&path, {
        let path = path.clone();
        move |at, n| {
            if at == "git" && n == 0 {
                swap_for_a_directory(&path);
            }
        }
    });
    let refused = wt.commit_all("delete everything", &me()).await.unwrap_err();

    assert!(hook.reached("git") >= 1, "the swap was made");
    assert!(matches!(refused, WorkspaceError::Corrupt(_)), "{refused}");
    assert_eq!(
        git(&env.mirror(), &["rev-parse", &branch]),
        tip,
        "the run's branch did not move"
    );
}

#[tokio::test]
async fn a_worktree_directory_of_the_mirror_is_never_taken_over_by_another_run() {
    let env = Env::new();
    let a = env
        .ws
        .run("run-aaaaaaaa-1")
        .unwrap()
        .add_repository(&env.repo)
        .await
        .unwrap();
    let a_admin = a.worktree().unwrap().git_dir().unwrap().to_owned();
    // A's commands move A's slot away: its directory in the mirror now names a slot that is not there.
    let away = a.path().with_extension("away");
    std::fs::rename(a.path(), &away).unwrap();

    // Other runs add worktrees to the mirror. Nothing of A's is pruned, so no name is free to reuse.
    let c = env
        .ws
        .run("run-cccccccc-3")
        .unwrap()
        .add_repository(&env.repo)
        .await
        .unwrap();
    let d = env
        .ws
        .run("run-dddddddd-4")
        .unwrap()
        .add_repository(&env.repo)
        .await
        .unwrap();
    let (c_admin, d_admin) = (
        c.worktree().unwrap().git_dir().unwrap().to_owned(),
        d.worktree().unwrap().git_dir().unwrap().to_owned(),
    );
    assert!(a_admin.is_dir(), "A's directory in the mirror was pruned");
    assert_ne!(c_admin, a_admin);
    assert_ne!(d_admin, a_admin);
    for (admin, run) in [
        (&a_admin, "run-aaaaaaaa-1"),
        (&c_admin, "run-cccccccc-3"),
        (&d_admin, "run-dddddddd-4"),
    ] {
        let name = admin.file_name().unwrap().to_str().unwrap();
        assert!(
            name.starts_with(&format!("{run}+")),
            "{name} carries its run: no other run's directory is called that"
        );
    }

    // A puts its slot back: it is still on its own directory, and its own branch gets its work.
    std::fs::rename(&away, a.path()).unwrap();
    let again = Workspaces::new(env.root.clone(), Arc::new(StaticToken::new(TOKEN)));
    let slots = again.run("run-aaaaaaaa-1").unwrap().slots().await.unwrap();
    let a_wt = slots[0].worktree().unwrap();
    assert_eq!(a_wt.git_dir().unwrap(), a_admin);
    std::fs::write(a.path().join("README.md"), "A's work\n").unwrap();
    let sha = a_wt.commit_all("A", &me()).await.unwrap().unwrap();
    assert_eq!(
        git(&env.mirror(), &["show", &format!("{sha}:README.md")]),
        "A's work"
    );

    // A directory that is not A's any more (another run's, whose `gitdir` names its own slot) is
    // not A's, whatever the record says.
    std::fs::write(
        a_admin.join("gitdir"),
        format!("{}\n", d.path().join(".git").display()),
    )
    .unwrap();
    let slots = again.run("run-aaaaaaaa-1").unwrap().slots().await.unwrap();
    let a_wt = slots[0].worktree().unwrap();
    assert!(a_wt.git_dir().is_none(), "{:?}", a_wt.git_dir());
    assert!(a_wt.status().await.is_err());
    // A directory that is called after another run is not taken either.
    let mut meta = meta_json(&env, "run-aaaaaaaa-1", &a);
    meta["admin"] = d_admin.file_name().unwrap().to_str().unwrap().into();
    let meta_path = env
        .root
        .join("meta/run-aaaaaaaa-1")
        .join(format!("{}.json", a.dir()));
    std::fs::write(&meta_path, serde_json::to_vec_pretty(&meta).unwrap()).unwrap();
    std::fs::write(
        d_admin.join("gitdir"),
        format!("{}\n", a.path().join(".git").display()),
    )
    .unwrap();
    let slots = again.run("run-aaaaaaaa-1").unwrap().slots().await.unwrap();
    assert!(
        slots[0].worktree().unwrap().git_dir().is_none(),
        "a record that names another run's directory is not believed even when its gitdir was made to agree"
    );
}

#[tokio::test]
async fn removing_a_run_whose_slot_became_a_link_does_not_remove_what_the_link_leads_to() {
    let env = Env::new();
    let a = env
        .ws
        .run("run-aaaaaaaa-1")
        .unwrap()
        .add_repository(&env.repo)
        .await
        .unwrap();
    let b = worktree_with_a_change(&env, "run-bbbbbbbb-2").await;
    let b_admin = b.worktree().unwrap().git_dir().unwrap().to_owned();

    // `git worktree remove` resolves a link and deletes the worktree it leads to (*verified* with
    // git 2.43.0) unless the path is the very one a worktree is registered at (then it checks that
    // the `.git` file points back, and refuses). A's slot is replaced by a link to B's, and A's
    // `gitdir` (which a pod without the read-only mount, local mode and Podman can write) names
    // another path.
    let a_admin = a.worktree().unwrap().git_dir().unwrap().to_owned();
    std::fs::write(a_admin.join("gitdir"), "/nowhere/.git\n").unwrap();
    std::fs::rename(a.path(), a.path().with_extension("away")).unwrap();
    std::os::unix::fs::symlink(b.path(), a.path()).unwrap();

    env.ws.remove("run-aaaaaaaa-1").await.unwrap();
    assert!(
        b.path().join("README.md").is_file() && b_admin.is_dir(),
        "B's worktree and its directory in the mirror are still there"
    );
    assert!(!a.path().exists() && std::fs::symlink_metadata(a.path()).is_err());
    the_coders_git_works_on(&b).await;
}

#[tokio::test]
async fn a_base_checkout_directory_that_is_a_link_is_replaced_and_never_followed() {
    let env = Env::new();
    let slot = env
        .ws
        .run("run-aaaaaaaa-1")
        .unwrap()
        .add_repository(&env.repo)
        .await
        .unwrap();
    let wt = slot.worktree().unwrap();
    let other = env
        .ws
        .run("run-bbbbbbbb-2")
        .unwrap()
        .add_repository(&env.repo)
        .await
        .unwrap();
    // A's `.adam-base` leads into B's workspace, where a directory is called like A's slot.
    let run_dir = slot.path().parent().unwrap().to_owned();
    let lure = other.path().parent().unwrap().to_owned();
    std::os::unix::fs::symlink(&lure, run_dir.join(".adam-base")).unwrap();
    std::fs::create_dir_all(lure.join(slot.dir())).unwrap();
    std::fs::write(lure.join(slot.dir()).join("precious"), "p\n").unwrap();

    let base = wt.add_base_checkout().await.unwrap().unwrap();
    assert!(
        base.path().starts_with(&run_dir)
            && !std::fs::symlink_metadata(run_dir.join(".adam-base"))
                .unwrap()
                .file_type()
                .is_symlink(),
        "the link was replaced by a directory of the run"
    );
    assert!(lure.join(slot.dir()).join("precious").is_file());
    // The checkout's directory in the mirror carries the run.
    let admins: Vec<String> = std::fs::read_dir(env.mirror().join("worktrees"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        admins.contains(&format!("run-aaaaaaaa-1+{}+base", slot.dir())),
        "{admins:?}"
    );

    // Now the checkout itself is replaced by a link to B's slot: removing it removes the link.
    std::fs::remove_dir_all(base.path()).unwrap();
    std::os::unix::fs::symlink(other.path(), base.path()).unwrap();
    wt.remove_base_checkout(&base).await.unwrap();
    assert!(
        other.path().join("README.md").is_file(),
        "B's worktree is intact"
    );
    assert!(std::fs::symlink_metadata(base.path()).is_err());
    assert!(
        std::fs::read_dir(env.mirror().join("worktrees"))
            .unwrap()
            .all(|e| !e.unwrap().file_name().to_string_lossy().ends_with("+base")),
        "its directory in the mirror is gone"
    );
}

#[tokio::test]
async fn a_planted_reflog_link_is_not_appended_to() {
    let env = Env::new();
    let slot = worktree_with_a_change(&env, "run-aaaaaaaa-1").await;
    let wt = slot.worktree().unwrap();
    let logs = wt.git_dir().unwrap().join("logs");
    // git appends a line to the file a link at `logs/HEAD` names (*verified* with git 2.43.0).
    let secret = victim(&env, "mirror-config");
    symlink_over(&secret, &logs.join("HEAD"));
    let refused = wt.commit_all("x", &me()).await.unwrap_err();
    assert!(matches!(refused, WorkspaceError::Corrupt(_)), "{refused}");
    assert_eq!(std::fs::read_to_string(&secret).unwrap(), VICTIM);
    // A link in place of the directory is as bad.
    std::fs::remove_file(logs.join("HEAD")).unwrap();
    std::fs::rename(&logs, logs.with_extension("moved")).unwrap();
    std::os::unix::fs::symlink(env._tmp.path(), &logs).unwrap();
    assert!(wt.status().await.is_err());
}

#[tokio::test]
async fn a_gitdir_file_that_is_a_pipe_does_not_hang_the_coder() {
    let env = Env::new();
    let slot = worktree_with_a_change(&env, "run-aaaaaaaa-1").await;
    let gitdir = slot.worktree().unwrap().git_dir().unwrap().join("gitdir");
    std::fs::remove_file(&gitdir).unwrap();
    assert!(
        Command::new("mkfifo")
            .arg(&gitdir)
            .status()
            .unwrap()
            .success()
    );
    let again = Workspaces::new(env.root.clone(), Arc::new(StaticToken::new(TOKEN)));
    let listed = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        again.run("run-aaaaaaaa-1").unwrap().slots(),
    )
    .await;
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(0o4000)
            .open(&gitdir);
    }
    let slots = listed.expect("the coder waited on a pipe").unwrap();
    // The directory is the run's by its record, and its back-pointer cannot be read: not its own.
    assert!(slots[0].worktree().unwrap().git_dir().is_none());
}

#[tokio::test]
async fn a_workspace_with_unreadable_metadata_leaves_none_of_its_directories_in_the_mirror() {
    let env = Env::new();
    let other = env
        .ws
        .run("run-bbbbbbbb-2")
        .unwrap()
        .add_repository(&env.repo)
        .await
        .unwrap();
    let a = env
        .ws
        .run("run-aaaaaaaa-1")
        .unwrap()
        .add_repository(&env.repo)
        .await
        .unwrap();
    let a_admin = a.worktree().unwrap().git_dir().unwrap().to_owned();
    let b_admin = other.worktree().unwrap().git_dir().unwrap().to_owned();
    // What a try that died between making a worktree and moving it left in staging.
    let staged = |run: &str| env.root.join("staging").join(format!("{run}+remote"));
    for run in ["run-aaaaaaaa-1", "run-bbbbbbbb-2"] {
        std::fs::create_dir_all(staged(run)).unwrap();
        std::fs::write(staged(run).join("README.md"), "x").unwrap();
    }
    std::fs::write(
        env.root
            .join("meta/run-aaaaaaaa-1")
            .join(format!("{}.json", a.dir())),
        "not json",
    )
    .unwrap();
    env.ws.remove("run-aaaaaaaa-1").await.unwrap();
    assert!(!a.path().exists());
    assert!(
        !a_admin.exists(),
        "A's directory in the mirror went with it"
    );
    assert!(b_admin.is_dir(), "B's did not");
    assert!(!staged("run-aaaaaaaa-1").exists(), "nor does its staging");
    assert!(staged("run-bbbbbbbb-2").is_dir());
    // With readable metadata the same is found by name.
    env.ws.remove("run-bbbbbbbb-2").await.unwrap();
    assert!(!staged("run-bbbbbbbb-2").exists());
    assert!(!b_admin.exists());
}

// ------------------------------------------------------------ a scratch project a run replaced by a link

#[tokio::test]
async fn a_scratch_directory_that_is_a_link_is_not_made_into_a_repository_where_it_leads() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = workspaces(tmp.path());
    let run = ws.run(RUN).unwrap();
    let first = run.add_scratch("fib", &me()).await.unwrap();

    // The directory of another run's project (or any directory the coder can write), which has no
    // repository yet: `git init` would make one in it.
    let elsewhere = tmp.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::fs::rename(first.path(), first.path().with_extension("away")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, first.path()).unwrap();

    let refused = run.add_scratch("fib", &me()).await.unwrap_err();
    assert!(matches!(refused, WorkspaceError::Corrupt(_)), "{refused}");
    assert!(
        refused.to_string().contains("not a directory of its own"),
        "{refused}"
    );
    assert!(
        std::fs::read_dir(&elsewhere).unwrap().next().is_none(),
        "nothing was made where the link leads"
    );

    // The same for a project that is not there yet: its name is a link planted before it was added.
    let planted = run.path().join("notes");
    std::fs::create_dir_all(run.path()).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &planted).unwrap();
    let refused = run.add_scratch("notes", &me()).await.unwrap_err();
    assert!(matches!(refused, WorkspaceError::Conflict(_)), "{refused}");
    assert!(std::fs::read_dir(&elsewhere).unwrap().next().is_none());
}

#[tokio::test]
async fn a_scratch_projects_dot_git_that_is_a_link_or_a_file_is_refused_and_nothing_is_made() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = workspaces(tmp.path());
    let run = ws.run(RUN).unwrap();
    let slot = run.add_scratch("fib", &me()).await.unwrap();
    let elsewhere = tmp.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();

    let dot_git = slot.path().join(".git");
    std::fs::rename(&dot_git, slot.path().join("git.away")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &dot_git).unwrap();
    let refused = run.add_scratch("fib", &me()).await.unwrap_err();
    assert!(
        refused.to_string().contains("not a directory of its own"),
        "{refused}"
    );
    assert!(std::fs::read_dir(&elsewhere).unwrap().next().is_none());

    std::fs::remove_file(&dot_git).unwrap();
    std::fs::write(&dot_git, "gitdir: /somewhere\n").unwrap();
    let refused = run.add_scratch("fib", &me()).await.unwrap_err();
    assert!(
        refused.to_string().contains("not a directory of its own"),
        "{refused}"
    );
    assert_eq!(
        std::fs::read_to_string(&dot_git).unwrap(),
        "gitdir: /somewhere\n"
    );
}

// ------------------------------------------------------------ the generation of a git directory

#[tokio::test]
async fn a_slot_made_again_under_the_same_name_has_another_generation_of_its_git_directory() {
    let env = Env::new();
    let run = env.ws.run("run-aaaaaaaa-1").unwrap();
    let slot = run.add_repository(&env.repo).await.unwrap();
    let wt = slot.worktree().unwrap();
    let (admin, generation) = (
        wt.git_dir().unwrap().to_owned(),
        wt.git_dir_generation().expect("recorded").to_owned(),
    );
    // Found again, it is the same directory and the same generation (also by another handle).
    let again = Workspaces::new(env.root.clone(), Arc::new(StaticToken::new(TOKEN)));
    let same = again
        .run("run-aaaaaaaa-1")
        .unwrap()
        .add_repository(&env.repo)
        .await
        .unwrap();
    assert_eq!(
        same.worktree().unwrap().git_dir_generation(),
        Some(generation.as_str())
    );

    // The slot's directory is lost; adding the repository makes the worktree again, its directory
    // in the mirror has the same name and is another directory.
    std::fs::remove_dir_all(slot.path()).unwrap();
    let remade = run.add_repository(&env.repo).await.unwrap();
    let remade = remade.worktree().unwrap();
    assert_eq!(remade.git_dir().unwrap(), admin, "the name is the same");
    let new = remade.git_dir_generation().expect("recorded").to_owned();
    assert_ne!(new, generation);
    // A line of an annotation: lowercase hex and a dash.
    assert!(
        new.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'),
        "{new}"
    );
    assert!(!new.contains(char::is_uppercase), "{new}");
    assert_eq!(
        run.slot(slot.dir())
            .await
            .unwrap()
            .unwrap()
            .worktree()
            .unwrap()
            .git_dir_generation(),
        Some(new.as_str()),
        "and it is what the metadata says now"
    );
}
