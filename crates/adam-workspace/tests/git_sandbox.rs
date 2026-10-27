//! The coder's git split in two (ADR 0041, open decision 6): what carries no credential runs in the git
//! sandbox, what talks to a remote runs here. Real `git`, real Unix sockets, a real server process
//! (`adam-git-sandbox`, built by these tests), local bare repositories and a mock HTTP server as
//! remotes; no network.
//!
//! The isolation itself (another container, no secret, another PID namespace) is not here: the server
//! of these tests is a process of the same user on the same machine, so a program it runs *can* read
//! the test's files. What is shown is that the commands go where they are meant to go, and that what a
//! repository can run runs **in the server's process tree, under the server's environment**.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use adam_error::{Classify, ErrorClass};
use adam_workspace::testing_sandbox::{GitSandboxServer as Server, SANDBOX_MARKER};
use adam_workspace::{
    GitIdentity, GitSandbox, LocalGit, RepoRef, Scratch, Slot, StaticToken, WorkspaceError,
    Workspaces, Worktree,
};

const TOKEN: &str = "ghp_FAKEtoken0123456789abcdefghijklmnop";

/// A name in this process's environment that stands for a secret of the coder: cargo sets it in every
/// test process.
const SECRET_NAME: &str = "CARGO_PKG_NAME";
const SECRET_VALUE: &str = "adam-workspace";

/// `git <args>` in `dir`, hermetic; panics with its stderr on failure.
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

fn me() -> GitIdentity {
    GitIdentity::new("Adam Agent", "adam@example.com")
}

/// A local remote with one commit, a workspace root, and (when asked for) a server on that root.
struct Fx {
    tmp: tempfile::TempDir,
    root: PathBuf,
    remote: PathBuf,
    repo: RepoRef,
    ws: Workspaces,
    server: Option<Server>,
}

impl Fx {
    fn with(sandboxed: bool) -> Self {
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
        std::fs::write(seed.join("README.md"), "hello\n").unwrap();
        git(&seed, &["add", "-A"]);
        git(&seed, &["commit", "--quiet", "-m", "seed"]);
        git(
            &seed,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&seed, &["push", "--quiet", "origin", "main"]);

        let root = tmp.path().join("workspaces");
        std::fs::create_dir_all(&root).unwrap();
        let mut ws = Workspaces::new(root.clone(), Arc::new(StaticToken::new(TOKEN)));
        let server = sandboxed.then(|| Server::start(tmp.path(), &root));
        if let Some(server) = &server {
            ws = ws.with_git_sandbox(server.socket());
        }
        let repo = RepoRef::new(remote.to_str().unwrap(), "main");
        Self {
            tmp,
            root,
            remote,
            repo,
            ws,
            server,
        }
    }

    fn plain() -> Self {
        Self::with(false)
    }

    fn sandboxed() -> Self {
        Self::with(true)
    }

    fn server(&self) -> &Server {
        self.server.as_ref().expect("a sandboxed fixture")
    }

    async fn worktree(&self, run: &str) -> Worktree {
        self.ws
            .run(run)
            .unwrap()
            .add_repository(&self.repo)
            .await
            .unwrap()
            .worktree()
            .unwrap()
            .clone()
    }

    async fn scratch(&self, run: &str) -> Scratch {
        self.ws
            .run(run)
            .unwrap()
            .add_scratch("proj", &me())
            .await
            .unwrap()
            .scratch()
            .unwrap()
            .clone()
    }

    fn mirror(&self) -> PathBuf {
        self.root
            .join(self.repo.locate().unwrap().mirror_relative())
    }
}

/// A program a repository's configuration can name (a `filter.*.clean`): it writes to `marker` the
/// processes it runs under, from itself up to the git sandbox's server (or six levels), and what it
/// can read of each one's environment; then it copies its input to its output, as a filter does.
fn spy(dir: &Path, marker: &Path) -> PathBuf {
    let path = dir.join("spy.sh");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\npid=$$; depth=0\nwhile [ \"$pid\" -gt 1 ] && [ $depth -lt 6 ]; do\n  comm=$(cat /proc/$pid/comm)\n  echo \"ancestor $pid $comm\" >> {m}\n  tr '\\0' '\\n' < /proc/$pid/environ | sed 's/^/environ /' >> {m} 2>/dev/null\n  case \"$comm\" in adam-git-sandb*) break;; esac\n  pid=$(sed 's/^[^)]*) . \\([0-9]*\\) .*/\\1/' /proc/$pid/stat)\n  depth=$((depth + 1))\ndone\ncat\n",
            m = marker.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn ancestors(marker: &str) -> impl Iterator<Item = &str> {
    marker.lines().filter(|l| l.starts_with("ancestor "))
}

/// What a marker shows when the filter ran under the server: the server is among its ancestors and
/// its environment is readable (it is the server's own), and the coder's secret is nowhere in it.
fn assert_ran_in_the_server(marker: &str, server_pid: u32, what: &str) {
    assert!(
        ancestors(marker).any(|a| a.starts_with(&format!("ancestor {server_pid} adam-git-sandb"))),
        "{what}: the server is not among the filter's ancestors:\n{marker}"
    );
    assert!(
        marker.contains(&format!(
            "environ {}={}",
            SANDBOX_MARKER.0, SANDBOX_MARKER.1
        )),
        "{what}: the filter does not read the server's environment:\n{marker}"
    );
    assert!(
        !marker.contains(&format!("environ {SECRET_NAME}={SECRET_VALUE}")),
        "{what}: the coder's secret is in what the filter read:\n{marker}"
    );
}

/// What a marker shows when the filter ran under the coder: the coder's environment is readable.
fn assert_ran_under_the_coder(marker: &str, what: &str) {
    assert!(
        marker.contains(&format!("environ {SECRET_NAME}={SECRET_VALUE}")),
        "{what}: the control does not show the vector (the filter did not run under the coder):\n{marker}"
    );
    assert!(
        !ancestors(marker).any(|a| a.contains("adam-git-sandb")),
        "{what}: {marker}"
    );
}

// ------------------------------------------------------------------------------------ the split

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn what_carries_no_credential_goes_through_the_server_and_what_talks_to_a_remote_does_not() {
    let fx = Fx::sandboxed();
    let wt = fx.worktree("run-aaaaaaaa-1").await;
    std::fs::write(wt.path().join("README.md"), "changed\n").unwrap();
    std::fs::write(wt.path().join("new.txt"), "new\n").unwrap();

    let status = wt.status().await.unwrap();
    assert_eq!(status.len(), 2, "{status:?}");
    // The copy of the index `diff_stat` works on is made where the server sees it.
    let stat = wt.diff_stat().await.unwrap();
    assert!(
        stat.contains("README.md") && stat.contains("new.txt"),
        "{stat}"
    );
    let sha = wt.commit_all("a change", &me()).await.unwrap().unwrap();
    assert_eq!(git(wt.mirror(), &["rev-parse", wt.local_branch()]), sha);
    wt.push().await.unwrap();
    assert_eq!(
        git(&fx.remote, &["rev-parse", wt.branch()]),
        sha,
        "the push reached the remote"
    );
    assert_eq!(fx.ws.default_branch(&fx.repo.url).await.unwrap(), "main");
    assert!(!fx.ws.remote_is_empty(&fx.repo.url).await.unwrap());

    let commands = fx.server().commands();
    let ran: Vec<&str> = commands.iter().map(|c| c.subcommand.as_str()).collect();
    for local in [
        "worktree",
        "status",
        "add",
        "diff",
        "write-tree",
        "commit-tree",
        "update-ref",
    ] {
        assert!(
            ran.contains(&local),
            "{local} did not go through the server: {ran:?}"
        );
    }
    for here in ["fetch", "push", "ls-remote", "init"] {
        assert!(
            !ran.contains(&here),
            "{here} went through the server: {ran:?}"
        );
    }
    for command in &commands {
        assert!(
            command.cwd.starts_with(&fx.root),
            "{} ran in {}",
            command.subcommand,
            command.cwd.display()
        );
        assert!(command.status.starts_with("exit"), "{command:?}");
    }
    // The mirror has been made, fetched and pushed from the coder's side: it is in the volume the
    // server sees, and git worked on it from both.
    assert!(fx.mirror().join("HEAD").is_file());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_scratch_project_is_made_and_committed_through_the_server() {
    let fx = Fx::sandboxed();
    let scratch = fx.scratch("run-scratch-0001").await;
    std::fs::write(scratch.path().join("a.txt"), "a\n").unwrap();
    assert_eq!(scratch.status().await.unwrap().len(), 1);
    let sha = scratch.commit_all("add a", &me()).await.unwrap().unwrap();
    assert_eq!(git(scratch.path(), &["rev-parse", "HEAD"]), sha);
    assert_eq!(scratch.files().await.unwrap(), [PathBuf::from("a.txt")]);

    let ran = fx.server().subcommands();
    for local in ["init", "commit", "status", "add", "ls-files"] {
        assert!(ran.iter().any(|s| s == local), "{local}: {ran:?}");
    }
    assert!(!ran.iter().any(|s| s == "fetch" || s == "push"), "{ran:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fetch_and_an_ls_remote_with_a_token_run_here_and_reach_the_remote_with_it() {
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let remote = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(404))
        .mount(&remote)
        .await;
    let fx = Fx::sandboxed();
    let repo = RepoRef::new(format!("{}/owner/repo.git", remote.uri()), "main");

    // `prepare`: the mirror is made (here), fetched (here, with the token) and fails with 404.
    let err = fx.ws.prepare(&repo, "run-http-0001").await.unwrap_err();
    assert!(matches!(err, WorkspaceError::NotFound(_)), "{err:?}");
    // The default branch and the emptiness: `ls-remote` (here, with the token).
    let err = fx.ws.default_branch(&repo.url).await.unwrap_err();
    assert!(matches!(err, WorkspaceError::NotFound(_)), "{err:?}");
    let err = fx.ws.remote_is_empty(&repo.url).await.unwrap_err();
    assert!(matches!(err, WorkspaceError::NotFound(_)), "{err:?}");

    let want = format!(
        "Basic {}",
        base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            format!("x-access-token:{TOKEN}")
        )
    );
    let requests = remote.received_requests().await.unwrap();
    assert!(
        requests.len() >= 3,
        "git never reached the server: {requests:?}"
    );
    for request in &requests {
        let got = request
            .headers
            .get("authorization")
            .expect("every request carries the header");
        assert_eq!(
            got.to_str().unwrap(),
            want,
            "the token is with the fetch, here"
        );
    }
    let ran = fx.server().subcommands();
    assert!(
        !ran.iter()
            .any(|s| matches!(s.as_str(), "fetch" | "ls-remote" | "push" | "init")),
        "a command that talks to a remote went through the server: {ran:?}"
    );
    // And what the sandbox logged holds no part of the token.
    let log = fx.server().log();
    assert!(
        !log.contains(TOKEN) && !log.contains("x-access-token"),
        "{log}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sandbox_that_cannot_be_reached_stops_the_commands_that_need_it_and_runs_nothing_here() {
    let fx = Fx::plain();
    let marker = fx.tmp.path().join("marker");
    let planted = spy(fx.tmp.path(), &marker);

    // The deployment says it has a sandbox, and nobody is there.
    let ws = fx
        .ws
        .clone()
        .with_git_sandbox(fx.tmp.path().join("nobody.sock"));
    // A scratch project with a filter, made by someone else, in a place the coder knows.
    let scratch = {
        let run = ws.run("run-down-00001").unwrap();
        let error = run.add_scratch("proj", &me()).await.unwrap_err();
        // Making it needs `git init`: a local command, so a sandbox command.
        assert_eq!(error.class(), ErrorClass::Transient, "{error:?}");
        assert!(error.is_retryable());
        assert!(error.to_string().contains("git sandbox"), "{error}");
        // The directory the failed attempt may have left is a project with the filter planted by hand.
        let dir = fx.root.join("workspaces/run-down-00001/proj");
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "--quiet", "--initial-branch=main"]);
        git(
            &dir,
            &["config", "filter.x.clean", planted.to_str().unwrap()],
        );
        std::fs::write(dir.join(".gitattributes"), "* filter=x\n").unwrap();
        std::fs::write(dir.join("a.txt"), "a\n").unwrap();
        dir
    };
    assert!(scratch.join("a.txt").exists());

    // A mirror and a worktree: the mirror is made here (a command on a mirror with no repository
    // code in it), the worktree needs the sandbox and is not made.
    let error = ws
        .run("run-down-00002")
        .unwrap()
        .add_repository(&fx.repo)
        .await
        .unwrap_err();
    assert_eq!(error.class(), ErrorClass::Transient, "{error:?}");
    assert!(
        !marker.exists(),
        "a filter ran in this container because the sandbox was down"
    );
}

// ------------------------------------------------------------------------------- the vector

/// A scratch project whose own `.git/config` runs `spy` as a clean filter (the run controls the
/// project entirely) and whose attributes apply it to every file.
fn plant_filter(scratch: &Scratch, spy: &Path) {
    git(
        scratch.path(),
        &["config", "filter.x.clean", spy.to_str().unwrap()],
    );
    std::fs::write(scratch.path().join(".gitattributes"), "* filter=x\n").unwrap();
    std::fs::write(scratch.path().join("a.txt"), "content\n").unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_filter_in_a_scratch_project_runs_in_the_servers_tree_and_without_the_coders_secrets() {
    // The control: no sandbox. The coder's `git add -A` runs the filter under the coder, and the
    // filter reads the coder's environment.
    let control = Fx::plain();
    let marker = control.tmp.path().join("marker");
    let scratch = control.scratch("run-control-001").await;
    plant_filter(&scratch, &spy(control.tmp.path(), &marker));
    scratch.commit_all("control", &me()).await.unwrap().unwrap();
    assert_ran_under_the_coder(
        &std::fs::read_to_string(&marker).expect("the filter ran"),
        "control",
    );

    // The same through the sandbox.
    let fx = Fx::sandboxed();
    let marker = fx.tmp.path().join("marker");
    let scratch = fx.scratch("run-sandboxed-01").await;
    plant_filter(&scratch, &spy(fx.tmp.path(), &marker));
    scratch
        .commit_all("sandboxed", &me())
        .await
        .unwrap()
        .unwrap();
    assert_ran_in_the_server(
        &std::fs::read_to_string(&marker).expect("the filter ran"),
        fx.server().pid(),
        "scratch project",
    );
    // The same filter, on the questions the coder's tools ask (`status` runs it too).
    std::fs::remove_file(&marker).unwrap();
    std::fs::write(scratch.path().join("a.txt"), "changed content\n").unwrap();
    scratch.status().await.unwrap();
    assert_ran_in_the_server(
        &std::fs::read_to_string(&marker).expect("status ran the filter"),
        fx.server().pid(),
        "scratch project, status",
    );
}

/// A worktree with an embedded repository the index lists as a gitlink, whose own configuration runs
/// `spy` as a clean filter on a file that has changed: the coder's `git status`, `git add -A` and
/// `git diff` of the outer repository enter it.
async fn worktree_with_an_embedded_repository(fx: &Fx, run: &str, spy: &Path) -> Worktree {
    let wt = fx.worktree(run).await;
    let nested = wt.path().join("vendor/dep");
    std::fs::create_dir_all(&nested).unwrap();
    git(&nested, &["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(nested.join("lib.txt"), "one\n").unwrap();
    std::fs::write(nested.join(".gitattributes"), "*.txt filter=x\n").unwrap();
    git(&nested, &["add", "-A"]);
    git(&nested, &["commit", "--quiet", "-m", "dep"]);
    // The coder commits the embedded repository as a gitlink: the outer repository now looks into it.
    wt.commit_all("vendor the dependency", &me())
        .await
        .unwrap()
        .unwrap();
    // The run then arms it and changes a file of it.
    git(
        &nested,
        &["config", "filter.x.clean", spy.to_str().unwrap()],
    );
    // The same size: git cannot tell from its stat data that the content is different, so it reads
    // the file through the filter.
    std::fs::write(nested.join("lib.txt"), "two\n").unwrap();
    wt
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_filter_in_an_embedded_repository_runs_in_the_servers_tree_and_without_the_coders_secrets()
 {
    // The control.
    let control = Fx::plain();
    let marker = control.tmp.path().join("marker");
    let wt = worktree_with_an_embedded_repository(
        &control,
        "run-control-001",
        &spy(control.tmp.path(), &marker),
    )
    .await;
    wt.status().await.unwrap();
    assert_ran_under_the_coder(
        &std::fs::read_to_string(&marker).expect("the embedded repository's filter ran (status)"),
        "control, status",
    );

    // Through the sandbox: `status`, `diff_stat` and `commit_all` (`add -A`) of the outer repository.
    let fx = Fx::sandboxed();
    let marker = fx.tmp.path().join("marker");
    let wt =
        worktree_with_an_embedded_repository(&fx, "run-sandboxed-01", &spy(fx.tmp.path(), &marker))
            .await;
    let pid = fx.server().pid();
    let mut ran_for = Vec::new();
    for (what, ask) in [("status", 0), ("diff_stat", 1), ("commit_all", 2)] {
        let _ = std::fs::remove_file(&marker);
        match ask {
            0 => drop(wt.status().await.unwrap()),
            1 => drop(wt.diff_stat().await.unwrap()),
            _ => drop(wt.commit_all("a change", &me()).await.unwrap()),
        }
        if let Ok(text) = std::fs::read_to_string(&marker) {
            assert_ran_in_the_server(&text, pid, what);
            ran_for.push(what);
        }
    }
    assert!(
        !ran_for.is_empty(),
        "no question entered the embedded repository (the setup does not show the vector)"
    );
    assert!(ran_for.contains(&"status"), "{ran_for:?}");
}

// ------------------------------------------------------- what the sandbox can write: the mirror

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_filter_that_rewrites_the_mirror_is_undone_before_the_next_fetch_and_push() {
    let fx = Fx::sandboxed();
    let wt = fx.worktree("run-first-00001").await;
    let mirror = fx.mirror();
    let evil = fx.tmp.path().join("evil.git");
    std::fs::create_dir_all(&evil).unwrap();
    git(
        &evil,
        &["init", "--bare", "--quiet", "--initial-branch=main"],
    );

    // A scratch project (the run controls it) whose filter writes the shared mirror's configuration:
    // the sandbox sees the whole volume and runs as the user that owns it.
    let tamper = fx.tmp.path().join("tamper.sh");
    std::fs::write(
        &tamper,
        format!(
            "#!/bin/sh\ngit config --file {mirror}/config 'url.{evil}.insteadOf' '{remote}'\ngit config --file {mirror}/config http.extraHeader 'Authorization: Basic VALUE-THAT-MUST-NOT-BE-NAMED'\ncat\n",
            mirror = mirror.display(),
            evil = evil.display(),
            remote = fx.remote.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&tamper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let scratch = fx.scratch("run-scratch-0001").await;
    plant_filter(&scratch, &tamper);
    scratch.commit_all("arm", &me()).await.unwrap().unwrap();
    assert!(
        std::fs::read_to_string(mirror.join("config"))
            .unwrap()
            .to_lowercase()
            .contains("extraheader"),
        "the filter did write the mirror's configuration"
    );

    // The coder's own network commands put the file back to what the workspace writes, then run: the
    // push reaches the real remote and nothing reaches the other URL.
    std::fs::write(wt.path().join("f.txt"), "f\n").unwrap();
    wt.commit_all("f", &me()).await.unwrap().unwrap();
    wt.push().await.unwrap();
    let config = std::fs::read_to_string(mirror.join("config"))
        .unwrap()
        .to_lowercase();
    assert!(
        !config.contains("insteadof") && !config.contains("extraheader"),
        "{config}"
    );
    assert!(
        std::fs::symlink_metadata(mirror.join("config"))
            .unwrap()
            .file_type()
            .is_file()
    );
    assert_eq!(
        git(&fx.remote, &["for-each-ref", "refs/heads/"])
            .lines()
            .count(),
        2,
        "the branch reached the real remote"
    );
    assert_eq!(
        git(&evil, &["for-each-ref"]),
        "",
        "nothing reached the other URL"
    );

    // And again for a fetch and for `ls-remote`: armed once more, undone once more.
    scratch_rearm(&fx, &scratch).await;
    let second = fx
        .ws
        .run("run-second-0002")
        .unwrap()
        .add_repository(&fx.repo)
        .await
        .unwrap();
    assert!(second.worktree().is_some());
    scratch_rearm(&fx, &scratch).await;
    assert_eq!(fx.ws.default_branch(&fx.repo.url).await.unwrap(), "main");
    assert_eq!(
        git(&evil, &["for-each-ref"]),
        "",
        "nothing reached the other URL"
    );
}

/// Arm the filter again: a commit in the scratch project whose content changes, so the filter runs.
async fn scratch_rearm(fx: &Fx, scratch: &Scratch) {
    let n = fx.tmp.path().join("rearm-count");
    let count: u32 = std::fs::read_to_string(&n)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
        + 1;
    std::fs::write(&n, count.to_string()).unwrap();
    std::fs::write(scratch.path().join("tracked.txt"), format!("arm {count}\n")).unwrap();
    scratch.commit_all("rearm", &me()).await.unwrap().unwrap();
    assert!(
        std::fs::read_to_string(fx.mirror().join("config"))
            .unwrap()
            .to_lowercase()
            .contains("insteadof"),
        "the filter wrote the mirror's configuration again"
    );
}

// ------------------------------------------------------------------------- LocalGit

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_questions_a_tool_asks_go_where_the_commands_of_the_workspace_go() {
    let fx = Fx::sandboxed();
    let wt = fx.worktree("run-aaaaaaaa-1").await;
    let slot: Slot = fx
        .ws
        .run("run-aaaaaaaa-1")
        .unwrap()
        .slots()
        .await
        .unwrap()
        .remove(0);
    let pin = slot.git_pin().unwrap();
    let sandbox: &GitSandbox = pin.sandbox().expect("the pin knows the sandbox");
    assert_eq!(sandbox.socket(), fx.server().socket());
    assert!(sandbox.private_dir().starts_with(&fx.root));

    let out = LocalGit::new(wt.path(), pin.sandbox())
        .pinned(&pin)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .await
        .unwrap();
    assert!(out.success);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        wt.local_branch()
    );
    assert!(fx.server().subcommands().iter().any(|s| s == "rev-parse"));

    // Input goes through: a patch applied by `git apply -` from the model's text.
    std::fs::write(wt.path().join("README.md"), "hello\nmore\n").unwrap();
    let patch = LocalGit::new(wt.path(), pin.sandbox())
        .pinned(&pin)
        .arg("diff")
        .output()
        .await
        .unwrap()
        .stdout;
    git(wt.path(), &["checkout", "--", "README.md"]);
    let applied = LocalGit::new(wt.path(), pin.sandbox())
        .pinned(&pin)
        .args(["apply", "-"])
        .stdin(patch)
        .output()
        .await
        .unwrap();
    assert!(
        applied.success,
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(wt.path().join("README.md")).unwrap(),
        "hello\nmore\n"
    );

    // A command that carries a credential is not a question: it is refused before anything is sent.
    let before = fx.server().subcommands().len();
    let refused = LocalGit::new(wt.path(), pin.sandbox())
        .pinned(&pin)
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "http.extraHeader")
        .env(
            "GIT_CONFIG_VALUE_0",
            "Authorization: Basic eC1hY2Nlc3M6dG9rZW4=",
        )
        .arg("status")
        .output()
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("credential"), "{refused}");
    assert_eq!(fx.server().subcommands().len(), before, "nothing was sent");

    // With no sandbox it is a child of this process, from the same confined environment.
    let here = LocalGit::new(wt.path(), None)
        .pinned(&pin)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&here.stdout).trim(),
        wt.local_branch()
    );
}
