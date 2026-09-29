//! Shared fixture: a local bare git repository as the remote, a mock GitHub for
//! pull requests, the adam-acp fake agent standing in for OpenCode, and helpers
//! to script the model. Everything is offline.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use adam_coder::opencode::OpenCodeLaunch;
use adam_coder::{CoderSettings, ToolEnv};
use adam_model::ToolCall;
use adam_workspace::{
    CodeHost, DynCodeHost, GitHub, NewPullRequest, PullRequest, RepoRef, StaticToken,
    WorkspaceError, Workspaces,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

pub const PR_URL: &str = "https://github.com/octo/widgets/pull/7";

/// Hermetic git for setting up and inspecting the remote; panics on failure.
pub fn git(dir: &Path, args: &[&str]) -> String {
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

/// Path of the scripted ACP agent, built on first use.
///
/// `CARGO_BIN_EXE_*` exists only inside `adam-acp`, so this crate's tests build
/// the binary themselves: `cargo build -p adam-acp --bin adam-acp-fake-agent`,
/// with the same profile as the running test binary. The output directory is
/// derived from the test executable's own location
/// (`<target>/<profile>/deps/<test>`), which is right for any `CARGO_TARGET_DIR`
/// or `--target-dir`. Cargo is found through `$CARGO`, then `PATH`.
pub fn fake_agent() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let exe = std::env::current_exe().expect("test executable path");
        let profile_dir = exe
            .parent()
            .and_then(Path::parent)
            .expect("<target>/<profile>/deps/<test>")
            .to_path_buf();
        let profile = profile_dir
            .file_name()
            .and_then(|n| n.to_str())
            .expect("profile directory name")
            .to_owned();
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
        let mut cmd = Command::new(cargo);
        cmd.args([
            "build",
            "--quiet",
            "-p",
            "adam-acp",
            "--bin",
            "adam-acp-fake-agent",
        ]);
        match profile.as_str() {
            "debug" => {}
            "release" => {
                cmd.arg("--release");
            }
            other => {
                cmd.args(["--profile", other]);
            }
        }
        // The same target directory as this test binary, wherever it is.
        cmd.env(
            "CARGO_TARGET_DIR",
            profile_dir.parent().expect("target dir"),
        );
        let status = cmd.status().expect("cargo runs");
        assert!(status.success(), "building adam-acp-fake-agent failed");
        let bin = profile_dir.join("adam-acp-fake-agent");
        assert!(bin.is_file(), "{} was not built", bin.display());
        bin
    })
}

/// GitHub's REST API, faked: lists open pull requests for a head (none until
/// one was created) and creates them.
struct ListPulls {
    created: Arc<AtomicBool>,
}

fn branch_of(head: &str) -> &str {
    head.split_once(':').map_or(head, |(_, b)| b)
}

fn api_pull(branch: &str) -> Value {
    json!({
        "number": 7,
        "html_url": PR_URL,
        "head": {"ref": branch},
        "state": "open",
    })
}

impl Respond for ListPulls {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        if !self.created.load(Ordering::SeqCst) {
            return ResponseTemplate::new(200).set_body_json(json!([]));
        }
        let head = request
            .url
            .query_pairs()
            .find(|(k, _)| k == "head")
            .map(|(_, v)| v.into_owned())
            .unwrap_or_default();
        ResponseTemplate::new(200).set_body_json(json!([api_pull(branch_of(&head))]))
    }
}

struct CreatePull {
    created: Arc<AtomicBool>,
}

impl Respond for CreatePull {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        self.created.store(true, Ordering::SeqCst);
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let head = body["head"].as_str().unwrap_or_default();
        ResponseTemplate::new(201).set_body_json(api_pull(head))
    }
}

/// The real [`GitHub`] client pointed at the mock, but answering for a
/// `github.com` repository slug: `GitHub` refuses local paths, and the test
/// remote is one.
struct GithubBehindMock {
    inner: GitHub,
    slug: RepoRef,
}

#[async_trait]
impl CodeHost for GithubBehindMock {
    async fn open_pull_request(
        &self,
        mut pr: NewPullRequest,
    ) -> Result<PullRequest, WorkspaceError> {
        pr.repo = self.slug.clone();
        self.inner.open_pull_request(pr).await
    }

    async fn find_pull_request(
        &self,
        _repo: &RepoRef,
        head: &str,
    ) -> Result<Option<PullRequest>, WorkspaceError> {
        self.inner.find_pull_request(&self.slug, head).await
    }
}

pub struct Fixture {
    pub tmp: TempDir,
    /// The bare remote.
    pub remote: PathBuf,
    /// Workspace root (mirrors, worktrees, notes).
    pub root: PathBuf,
    pub github: MockServer,
    pub env: Arc<ToolEnv>,
}

impl Fixture {
    /// A fixture whose fake OpenCode writes `hello.txt` with `content`.
    pub async fn new(content: &str) -> Self {
        Self::with(content, |_| {}).await
    }

    /// Like [`new`](Self::new), adjusting the settings first.
    pub async fn with(content: &str, tweak: impl FnOnce(&mut CoderSettings)) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let remote = tmp.path().join("remote.git");
        let seed = tmp.path().join("seed");
        std::fs::create_dir_all(&remote).unwrap();
        std::fs::create_dir_all(&seed).unwrap();
        git(
            &remote,
            &["init", "--bare", "--quiet", "--initial-branch=main"],
        );
        // Record every ref update, so duplicate pushes/commits are visible.
        git(&remote, &["config", "core.logAllRefUpdates", "always"]);
        git(&seed, &["init", "--quiet", "--initial-branch=main"]);
        std::fs::write(seed.join("README.md"), "widgets\n").unwrap();
        git(&seed, &["add", "-A"]);
        git(&seed, &["commit", "--quiet", "-m", "seed"]);
        git(
            &seed,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&seed, &["push", "--quiet", "origin", "main"]);

        let github = MockServer::start().await;
        let created = Arc::new(AtomicBool::new(false));
        Mock::given(method("GET"))
            .and(path("/repos/octo/widgets/pulls"))
            .respond_with(ListPulls {
                created: created.clone(),
            })
            .mount(&github)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/octo/widgets/pulls"))
            .respond_with(CreatePull { created })
            .mount(&github)
            .await;

        let root = tmp.path().join("work");
        let creds = Arc::new(StaticToken::new("ghp_FAKEtoken0123456789abcdefghijklmnop"));
        let workspaces = Workspaces::new(root.clone(), creds.clone());
        let code_host: DynCodeHost = Arc::new(GithubBehindMock {
            inner: GitHub::new(creds)
                .expect("client")
                .with_api_base(github.uri()),
            slug: RepoRef::new("https://github.com/octo/widgets.git", "main"),
        });

        let launch = OpenCodeLaunch::program(fake_agent())
            .env("FAKE_ACP_SCENARIO", "write-file")
            .env("FAKE_ACP_WRITE_PATH", "hello.txt")
            .env("FAKE_ACP_WRITE_CONTENT", content);
        let mut settings = CoderSettings::new(launch);
        tweak(&mut settings);
        let env = Arc::new(ToolEnv::new(workspaces, code_host, settings));
        Self {
            tmp,
            remote,
            root,
            github,
            env,
        }
    }

    pub fn remote_url(&self) -> String {
        self.remote.to_string_lossy().into_owned()
    }

    /// Branches on the remote other than `main`.
    pub fn agent_branches(&self) -> Vec<String> {
        git(
            &self.remote,
            &[
                "for-each-ref",
                "--format=%(refname:short)",
                "refs/heads/agent",
            ],
        )
        .lines()
        .map(str::to_owned)
        .collect()
    }

    /// `hello.txt` as committed on `branch` of the remote.
    pub fn file_on(&self, branch: &str, file: &str) -> String {
        git(&self.remote, &["show", &format!("{branch}:{file}")])
    }

    /// Commits `branch` has beyond `main`.
    pub fn commits_ahead(&self, branch: &str) -> usize {
        git(
            &self.remote,
            &["rev-list", "--count", &format!("main..{branch}")],
        )
        .parse()
        .unwrap()
    }

    /// Ref updates the remote saw for `branch` (a duplicate commit or push
    /// that changed something would add one).
    pub fn ref_updates(&self, branch: &str) -> usize {
        let log = self.remote.join("logs/refs/heads").join(branch);
        std::fs::read_to_string(log).map_or(0, |t| t.lines().count())
    }

    /// The JSON bodies of every `POST /pulls` the mock GitHub received.
    pub async fn created_pulls(&self) -> Vec<Value> {
        self.github
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.method.as_str() == "POST")
            .map(|r| serde_json::from_slice(&r.body).unwrap_or(Value::Null))
            .collect()
    }
}

pub fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.to_owned(),
        name: name.to_owned(),
        arguments: args,
    }
}

/// The model script of the happy path: prepare, delegate, check, commit and
/// push, open the PR, then answer.
pub fn happy_script(mock: &adam_model::MockModel, remote_url: &str) {
    mock.push_tool_calls(vec![call(
        "c1",
        "prepare_workspace",
        json!({"repo_url": remote_url, "base_branch": "main"}),
    )])
    .push_tool_calls(vec![call(
        "c2",
        "delegate_to_opencode",
        json!({"instructions": "add hello.txt containing hello"}),
    )])
    .push_tool_calls(vec![call(
        "c3",
        "run_checks",
        json!({"command": "test -f hello.txt && cat hello.txt"}),
    )])
    .push_tool_calls(vec![call(
        "c4",
        "commit_and_push",
        json!({"message": "feat: add hello.txt"}),
    )])
    .push_tool_calls(vec![call(
        "c5",
        "open_pull_request",
        json!({"title": "feat: add hello.txt", "body": "Adds hello.txt.\n\n## Verification\n- `test -f hello.txt`: passed"}),
    )])
    .push_text("Opened the pull request.");
}
