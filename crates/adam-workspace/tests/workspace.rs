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
    assert!(matches!(err, WorkspaceError::NotFound(_)), "{err:?}");
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
