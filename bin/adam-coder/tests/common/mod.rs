//! Shared fixture: a local bare git repository as the remote, a mock GitHub for
//! pull requests, the adam-acp fake agent standing in for OpenCode, and helpers
//! to script the model. Everything is offline.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

use adam_coder::opencode::OpenCodeLaunch;
use adam_coder::{CoderSettings, Redactor, ToolEnv};
use adam_model::ToolCall;
use adam_workspace::{
    CodeHost, DynCodeHost, GitHub, NewPullRequest, PullRequest, RepoRef, ScopedToken,
    WorkspaceError, Workspaces,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

pub mod pg;

/// The first pull request the mock GitHub creates.
pub const PR_URL: &str = "https://github.com/octo/widgets/pull/7";

/// Token of the fixture; every test can assert it never leaks.
pub const GITHUB_TOKEN: &str = "ghp_FAKEtoken0123456789abcdefghijklmnop";

/// The other secrets a coder process holds; the fixture registers all of them
/// with the redactor, and tests plant them where they must never surface.
pub const MODEL_KEY: &str = "sk-live-0123456789abcdefSECRETKEY";
pub const A2A_TOKEN: &str = "coder-test-token";
pub const DB_PASSWORD: &str = "pg-pa55w0rd-very-secret";

/// Every secret value above.
pub const SECRETS: [&str; 4] = [GITHUB_TOKEN, MODEL_KEY, A2A_TOKEN, DB_PASSWORD];

/// URL of pull request `number` of the mock repository.
pub fn pull_url(number: u64) -> String {
    format!("https://github.com/octo/widgets/pull/{number}")
}

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

/// The pull requests the mock GitHub has, by head branch: `number` starts at 7
/// and grows per new head. Creating a pull request for a head that has one
/// returns it again (GitHub would answer 422; the client resolves both the
/// same way).
#[derive(Default)]
struct Pulls {
    by_head: Mutex<BTreeMap<String, u64>>,
}

impl Pulls {
    fn create(&self, head: &str) -> u64 {
        let mut heads = self.by_head.lock().unwrap();
        let next = 7 + heads.len() as u64;
        *heads.entry(head.to_owned()).or_insert(next)
    }

    fn get(&self, head: &str) -> Option<u64> {
        self.by_head.lock().unwrap().get(head).copied()
    }
}

fn branch_of(head: &str) -> &str {
    head.split_once(':').map_or(head, |(_, b)| b)
}

fn api_pull(number: u64, branch: &str, base: &str) -> Value {
    json!({
        "number": number,
        "html_url": pull_url(number),
        "head": {"ref": branch},
        "base": {"ref": base},
        "state": "open",
    })
}

struct ListPulls {
    pulls: Arc<Pulls>,
}

impl Respond for ListPulls {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let head = request
            .url
            .query_pairs()
            .find(|(k, _)| k == "head")
            .map(|(_, v)| v.into_owned())
            .unwrap_or_default();
        let base = request
            .url
            .query_pairs()
            .find(|(k, _)| k == "base")
            .map_or_else(|| "main".to_owned(), |(_, v)| v.into_owned());
        let branch = branch_of(&head);
        let open: Vec<Value> = self
            .pulls
            .get(branch)
            .map(|n| api_pull(n, branch, &base))
            .into_iter()
            .collect();
        ResponseTemplate::new(200).set_body_json(open)
    }
}

struct CreatePull {
    pulls: Arc<Pulls>,
}

impl Respond for CreatePull {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let head = body["head"].as_str().unwrap_or_default();
        let base = body["base"].as_str().unwrap_or("main");
        let number = self.pulls.create(head);
        ResponseTemplate::new(201).set_body_json(api_pull(number, head, base))
    }
}

/// A mock GitHub for `octo/widgets`: lists and creates pull requests.
pub async fn mock_github() -> MockServer {
    let github = MockServer::start().await;
    let pulls = Arc::new(Pulls::default());
    Mock::given(method("GET"))
        .and(path("/repos/octo/widgets/pulls"))
        .respond_with(ListPulls {
            pulls: pulls.clone(),
        })
        .mount(&github)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/octo/widgets/pulls"))
        .respond_with(CreatePull { pulls })
        .mount(&github)
        .await;
    // Comments on a pull request (the issue's comments).
    Mock::given(method("POST"))
        .and(wiremock::matchers::path_regex(
            r"^/repos/octo/widgets/issues/\d+/comments$",
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 1})))
        .mount(&github)
        .await;
    github
}

/// Make the mock GitHub answer every API call with `status` from now on.
pub async fn github_fails_with(github: &MockServer, status: u16, message: &str) {
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(status).set_body_json(json!({"message": message})))
        .with_priority(1)
        .mount(github)
        .await;
}

/// Make the mock GitHub refuse every comment on a pull request with `status` from now on.
pub async fn comments_fail_with(github: &MockServer, status: u16) {
    Mock::given(method("POST"))
        .and(wiremock::matchers::path_regex(
            r"^/repos/octo/widgets/issues/\d+/comments$",
        ))
        .respond_with(
            ResponseTemplate::new(status).set_body_json(json!({"message": "comments are closed"})),
        )
        .with_priority(1)
        .mount(github)
        .await;
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
        pr.repo = RepoRef::new(self.slug.url.clone(), pr.repo.base_branch.clone());
        self.inner.open_pull_request(pr).await
    }

    async fn find_pull_request(
        &self,
        repo: &RepoRef,
        head: &str,
    ) -> Result<Option<PullRequest>, WorkspaceError> {
        // The mock repository's base is whatever the caller works against; the slug stands in
        // for the repository only.
        let repo = RepoRef::new(self.slug.url.clone(), repo.base_branch.clone());
        self.inner.find_pull_request(&repo, head).await
    }

    async fn find_pull_request_on_head(
        &self,
        repo: &RepoRef,
        head: &str,
    ) -> Result<Option<PullRequest>, WorkspaceError> {
        let repo = RepoRef::new(self.slug.url.clone(), repo.base_branch.clone());
        self.inner.find_pull_request_on_head(&repo, head).await
    }

    async fn comment_on_pull_request(
        &self,
        _repo: &RepoRef,
        number: u64,
        body: &str,
    ) -> Result<(), WorkspaceError> {
        self.inner
            .comment_on_pull_request(&self.slug, number, body)
            .await
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

        let github = mock_github().await;

        let root = tmp.path().join("work");
        let creds = Arc::new(ScopedToken::new("github.com", GITHUB_TOKEN));
        // What the binary does, with `ALLOW_LOCAL_REPOS=true` (the remote here
        // is a local bare repository).
        let workspaces = Workspaces::new(root.clone(), creds.clone())
            .allow_hosts(["github.com"])
            .allow_local(true);
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
        let env = Arc::new(
            ToolEnv::new(workspaces, code_host, settings).with_redactor(Redactor::new(SECRETS)),
        );
        Self {
            tmp,
            remote,
            root,
            github,
            env,
        }
    }

    /// The same tools as `env` but with the production repository policy:
    /// only `github.com`, no local paths (`ALLOW_LOCAL_REPOS` unset), in a
    /// workspace root of its own.
    pub fn production_env(&self) -> Arc<ToolEnv> {
        let creds = Arc::new(ScopedToken::new("github.com", GITHUB_TOKEN));
        let workspaces = Workspaces::new(self.tmp.path().join("production-work"), creds)
            .allow_hosts(["github.com"])
            .allow_local(false);
        Arc::new(
            ToolEnv::new(
                workspaces,
                self.env.code_host.clone(),
                self.env.settings.clone(),
            )
            .with_redactor(Redactor::new(SECRETS)),
        )
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

    /// The bodies of the comments the mock GitHub received, as `(pull request number, text)`.
    pub async fn comments(&self) -> Vec<(u64, String)> {
        self.github
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.method.as_str() == "POST" && r.url.path().ends_with("/comments"))
            .map(|r| {
                let number = r
                    .url
                    .path()
                    .split('/')
                    .rev()
                    .nth(1)
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
                let body: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
                (number, body["body"].as_str().unwrap_or_default().to_owned())
            })
            .collect()
    }

    /// The JSON bodies of every `POST /pulls` the mock GitHub received.
    pub async fn created_pulls(&self) -> Vec<Value> {
        self.github
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.method.as_str() == "POST" && r.url.path().ends_with("/pulls"))
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
    happy_script_titled(mock, remote_url, "feat: add hello.txt");
}

/// [`happy_script`] with the pull request title `title`.
pub fn happy_script_titled(mock: &adam_model::MockModel, remote_url: &str, title: &str) {
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
        json!({"message": title}),
    )])
    .push_tool_calls(vec![call(
        "c5",
        "open_pull_request",
        json!({"title": title, "body": "Adds hello.txt.\n\n## Verification\n- `test -f hello.txt`: passed"}),
    )])
    .push_text("Opened the pull request.");
}

/// An OpenAI chat-completions reply that calls one tool.
pub fn tool_reply(id: &str, name: &str, arguments: Value) -> Value {
    json!({
        "choices": [{
            "message": {"role": "assistant", "content": null, "tool_calls": [{
                "id": id, "type": "function",
                "function": {"name": name, "arguments": arguments.to_string()}
            }]},
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1}
    })
}

/// A reply of [`text_reply`] or [`tool_reply`] as the server-sent-events stream a request with
/// `"stream": true` is answered with (the agent streams its model calls): a text in two deltas, a
/// tool call whole, the finish chunk, the usage chunk and `[DONE]`.
pub fn sse_of(reply: &Value) -> String {
    let message = &reply["choices"][0]["message"];
    let mut events = vec![
        json!({"choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": null}]}),
    ];
    if let Some(text) = message["content"].as_str() {
        let middle = text
            .char_indices()
            .nth(text.chars().count() / 2)
            .map_or(text.len(), |(at, _)| at);
        for piece in [&text[..middle], &text[middle..]] {
            events.push(
                json!({"choices": [{"index": 0, "delta": {"content": piece}, "finish_reason": null}]}),
            );
        }
    }
    for (index, call) in message["tool_calls"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        events.push(json!({"choices": [{"index": 0, "delta": {"tool_calls": [{
            "index": index, "id": call["id"], "type": "function", "function": call["function"]
        }]}, "finish_reason": null}]}));
    }
    events.push(
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": reply["choices"][0]["finish_reason"]}]}),
    );
    events.push(json!({"choices": [], "usage": reply["usage"]}));
    let mut body: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    body
}

/// The response to a chat-completions `request` that is answered with `reply`: the stream the
/// request asks for with `"stream": true` (the agent streams its model calls), or JSON.
pub fn chat_response(request: &wiremock::Request, reply: &Value) -> wiremock::ResponseTemplate {
    let streaming =
        serde_json::from_slice::<Value>(&request.body).is_ok_and(|b| b["stream"] == true);
    if streaming {
        wiremock::ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(sse_of(reply))
    } else {
        wiremock::ResponseTemplate::new(200).set_body_json(reply)
    }
}

/// An OpenAI chat-completions reply that answers with text.
pub fn text_reply(text: &str) -> Value {
    json!({
        "choices": [{"message": {"role": "assistant", "content": text}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1}
    })
}

/// An ACP "OpenCode" that is a shell script around the fake agent, so tests
/// can script what the real one would do across launches. The script runs with
/// `$AGENT` set to the fake agent (which reads the `FAKE_ACP_*` variables of
/// `base`) and `$LAUNCHES`, a file it appends one line to per launch.
///
/// Returns the launcher and the path of the launch log.
pub fn scripted_agent(dir: &Path, script: &str, base: OpenCodeLaunch) -> (OpenCodeLaunch, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let launches = dir.join("launches.log");
    let file = dir.join("opencode.sh");
    std::fs::write(
        &file,
        format!("#!/bin/sh\necho launched >> \"$LAUNCHES\"\n{script}\n"),
    )
    .unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut launch = OpenCodeLaunch::program("/bin/sh")
        .env("AGENT", fake_agent().to_string_lossy())
        .env("LAUNCHES", launches.to_string_lossy());
    launch.args = vec![file.to_string_lossy().into_owned()];
    launch.env.extend(base.env);
    (launch, launches)
}

/// How often the launch log says the agent was started.
pub fn launches(log: &Path) -> usize {
    std::fs::read_to_string(log).map_or(0, |t| t.lines().count())
}

/// The process id a test agent wrote to `file`, once it has (polls up to 30 s).
pub async fn wait_for_pid(file: &Path) -> u32 {
    for _ in 0..3000 {
        if let Some(pid) = std::fs::read_to_string(file)
            .ok()
            .and_then(|t| t.trim().parse().ok())
        {
            return pid;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("{} never got a pid", file.display());
}

/// Whether process `pid` is gone. With `reaped` a zombie still counts as
/// alive (its parent never collected it); without, a zombie is dead, which is
/// all one can ask of a grandchild that an init without a reaper adopted.
pub fn process_gone(pid: u32, reaped: bool) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) if Path::new("/proc/self/stat").exists() => true,
        Ok(stat) => {
            !reaped
                && stat
                    .rsplit(')')
                    .next()
                    .is_some_and(|rest| rest.trim_start().starts_with('Z'))
        }
        // No /proc: ask the shell.
        Err(_) => !Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success()),
    }
}

/// Wait up to `within` for `pid` to be gone (see [`process_gone`]).
pub async fn wait_gone(pid: u32, reaped: bool, within: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + within;
    loop {
        if process_gone(pid, reaped) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// One raw HTTP/1.1 request; `(status, whole response)`.
pub async fn raw(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    bearer: Option<&str>,
) -> (u16, String) {
    try_raw(addr, method, path, bearer)
        .await
        .expect("an HTTP response")
}

/// [`raw`], reporting a refused connection or a broken response as an error.
pub async fn try_raw(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    bearer: Option<&str>,
) -> std::io::Result<(u16, String)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"GetTask","params":{"id":"nope"}}"#;
    let auth = bearer.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    let payload = if method == "POST" {
        format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n{auth}\r\n{body}",
            body.len()
        )
    } else {
        format!("{auth}\r\n")
    };
    let mut stream = tokio::net::TcpStream::connect(addr).await?;
    stream
        .write_all(
            format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n{payload}")
                .as_bytes(),
        )
        .await?;
    let mut response = String::new();
    stream.read_to_string(&mut response).await?;
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| std::io::Error::other("no status line"))?;
    Ok((status, response))
}

/// The official A2A client for the server at `addr`, authenticating with
/// `token`. The card says where requests go (the server's `PUBLIC_URL`, which
/// in a test is not where it listens), so the card's URLs are pointed at
/// `addr` first.
pub async fn a2a_client(
    addr: std::net::SocketAddr,
    token: &str,
) -> a2a_client::A2AClient<Box<dyn a2a_client::Transport>> {
    let mut card = a2a_client::agent_card::AgentCardResolver::new(None)
        .resolve(&format!("http://{addr}"))
        .await
        .expect("the agent card");
    for interface in &mut card.supported_interfaces {
        interface.url = format!("http://{addr}/");
    }
    a2a_client::A2AClientFactory::builder()
        .with_interceptor(Arc::new(a2a_client::auth::AuthInterceptor::bearer(token)))
        .build()
        .create_from_card(&card)
        .await
        .expect("an A2A client")
}

/// A copy of the shipped `agent/` under `<tmp>/agent`: what a deployment mounts as `ADAM_AGENT_DIR`,
/// which a test then edits the way a deployment edits it.
pub fn folder() -> TempDir {
    fn copy(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).unwrap();
            }
        }
    }
    let tmp = tempfile::tempdir().unwrap();
    copy(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("agent"),
        &tmp.path().join("agent"),
    );
    tmp
}

/// `agent/instructions.md` of a folder made by [`folder`], rewritten by `edit`.
pub fn edit_instructions(folder: &TempDir, edit: impl FnOnce(String) -> String) {
    let path = folder.path().join("agent/instructions.md");
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(path, edit(text)).unwrap();
}

/// The JSON of a raw HTTP response (see [`raw`]): from the first `{` to the last `}`, which skips
/// the status line and the headers and any chunked-encoding framing.
pub fn json_of(response: &str) -> Value {
    let start = response.find('{').expect("a JSON body");
    let end = response.rfind('}').expect("a JSON body");
    serde_json::from_str(&response[start..=end]).expect("the body is JSON")
}
