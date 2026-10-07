//! Shared fixture: a local bare git repository as the remote, a mock GitHub for
//! pull requests, the adam-acp fake agent standing in for OpenCode, and helpers
//! to script the model. Everything is offline.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use adam_coder::opencode::OpenCodeLaunch;
use adam_coder::{CoderSettings, Redactor, ToolEnv};
use adam_model::ToolCall;
use adam_workspace::{
    CodeHost, CreatedRepository, DynCodeHost, DynEnvironment, EnvDescription, EnvError, EnvKind,
    EnvProgress, EnvSession, EnvStep, Environment, ExecId, ExecSpec, GitHub, LocalSession,
    NewPullRequest, NewRepository, OwnerKind, PreparedCommand, PullRequest, RepoRef, RunWorkspace,
    ScopedToken, SecretRef, WorkspaceError, Workspaces,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

pub mod pg;

/// A check command that fails in a run's worktree and passes on the checkout of the base
/// (`.adam-base`), so that its failure is the run's and costs a check cycle: `then` is what it
/// does when it fails. A plain `exit 1` fails on the base too, which is a pre-existing failure.
pub fn red(then: &str) -> String {
    format!("case \"$PWD\" in */.adam-base/*) exit 0;; esac; {then}")
}

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

/// The directory of the slot of the fixture's repository (`remote.git`) in a run's workspace: the
/// repository's name.
pub const SLOT: &str = "remote";

/// The worktree of the repository `remote.git` in the workspace of `run`, under `root`:
/// `<root>/workspaces/<run>/remote`.
pub fn slot_dir(root: &Path, run: &str) -> PathBuf {
    root.join("workspaces").join(run).join(SLOT)
}

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
    mount_repository(&github, "octo/widgets").await;
    github
}

/// Make `github` list and create pull requests, and take comments on them, for the repository
/// `slug` (`owner/name`), with its own pull requests.
pub async fn mount_repository(github: &MockServer, slug: &str) {
    let pulls = Arc::new(Pulls::default());
    Mock::given(method("GET"))
        .and(path(format!("/repos/{slug}/pulls")))
        .respond_with(ListPulls {
            pulls: pulls.clone(),
        })
        .mount(github)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{slug}/pulls")))
        .respond_with(CreatePull { pulls })
        .mount(github)
        .await;
    // Comments on a pull request (the issue's comments).
    Mock::given(method("POST"))
        .and(wiremock::matchers::path_regex(format!(
            r"^/repos/{}/issues/\d+/comments$",
            slug.replace('.', r"\.")
        )))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 1})))
        .mount(github)
        .await;
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

/// A code host that can create repositories: every other call goes to the fixture's host, and a
/// created repository is an empty bare repository under `<tmp>/created/<owner>/<name>.git` whose
/// path is its clone URL (what a local remote is here). It records what it was asked.
pub struct CreatingHost {
    inner: DynCodeHost,
    root: PathBuf,
    organizations: Vec<String>,
    login: Option<String>,
    created: Mutex<Vec<NewRepository>>,
    taken: Mutex<Vec<String>>,
    clone_url: Mutex<Option<String>>,
    /// Creations to answer with a transient error, after which the repository **was made** (the
    /// answer was lost on its way): what a timeout looks like.
    answer_lost: Mutex<u32>,
    owner_kinds_asked: Mutex<Vec<String>>,
}

impl CreatingHost {
    /// The repositories it was asked to create, in order.
    pub fn created(&self) -> Vec<NewRepository> {
        self.created.lock().unwrap().clone()
    }

    /// Say that `owner/name` exists already, on the host, from before the run.
    pub fn take(&self, owner: &str, name: &str) {
        self.taken.lock().unwrap().push(format!("{owner}/{name}"));
    }

    /// The next `times` creations make the repository and then fail with a transient error, as a
    /// host does whose answer never arrives.
    pub fn answer_is_lost(&self, times: u32) {
        *self.answer_lost.lock().unwrap() = times;
    }

    /// Answer creations with `url` as the clone URL (no repository is made there).
    pub fn clone_url_is(&self, url: &str) {
        *self.clone_url.lock().unwrap() = Some(url.to_owned());
    }

    /// The owners it was asked the kind of.
    pub fn owner_kinds_asked(&self) -> Vec<String> {
        self.owner_kinds_asked.lock().unwrap().clone()
    }

    /// Where the repository `owner/name` is made.
    pub fn path_of(&self, owner: &str, name: &str) -> PathBuf {
        self.root
            .join("created")
            .join(owner)
            .join(format!("{name}.git"))
    }
}

#[async_trait]
impl CodeHost for CreatingHost {
    async fn open_pull_request(&self, pr: NewPullRequest) -> Result<PullRequest, WorkspaceError> {
        self.inner.open_pull_request(pr).await
    }

    async fn find_pull_request(
        &self,
        repo: &RepoRef,
        head: &str,
    ) -> Result<Option<PullRequest>, WorkspaceError> {
        self.inner.find_pull_request(repo, head).await
    }

    async fn find_pull_request_on_head(
        &self,
        repo: &RepoRef,
        head: &str,
    ) -> Result<Option<PullRequest>, WorkspaceError> {
        self.inner.find_pull_request_on_head(repo, head).await
    }

    async fn comment_on_pull_request(
        &self,
        repo: &RepoRef,
        number: u64,
        body: &str,
    ) -> Result<(), WorkspaceError> {
        self.inner.comment_on_pull_request(repo, number, body).await
    }

    async fn create_repository(
        &self,
        new: NewRepository,
    ) -> Result<CreatedRepository, WorkspaceError> {
        let loc = new.repo.locate()?;
        let full_name = format!("{}/{}", loc.owner, loc.name);
        if self.taken.lock().unwrap().contains(&full_name) {
            return Err(WorkspaceError::Invalid(
                "name already exists on this account".to_owned(),
            ));
        }
        let clone_url = match self.clone_url.lock().unwrap().clone() {
            Some(url) => url,
            None => {
                let path = self.path_of(&loc.owner, &loc.name);
                std::fs::create_dir_all(&path).unwrap();
                git(
                    &path,
                    &["init", "--bare", "--quiet", "--initial-branch=main"],
                );
                path.to_string_lossy().into_owned()
            }
        };
        self.taken.lock().unwrap().push(full_name.clone());
        self.created.lock().unwrap().push(new);
        {
            let mut lost = self.answer_lost.lock().unwrap();
            if *lost > 0 {
                *lost -= 1;
                return Err(WorkspaceError::Transient {
                    message: "the host did not answer".to_owned(),
                    source: None,
                });
            }
        }
        Ok(CreatedRepository {
            html_url: format!("https://example.invalid/{full_name}"),
            full_name,
            clone_url,
            default_branch: "main".to_owned(),
        })
    }

    async fn find_repository(
        &self,
        repo: &RepoRef,
    ) -> Result<Option<CreatedRepository>, WorkspaceError> {
        let loc = repo.locate()?;
        let full_name = format!("{}/{}", loc.owner, loc.name);
        if !self.taken.lock().unwrap().contains(&full_name) {
            return Ok(None);
        }
        let clone_url = match self.clone_url.lock().unwrap().clone() {
            Some(url) => url,
            None => self
                .path_of(&loc.owner, &loc.name)
                .to_string_lossy()
                .into_owned(),
        };
        Ok(Some(CreatedRepository {
            html_url: format!("https://example.invalid/{full_name}"),
            full_name,
            clone_url,
            default_branch: "main".to_owned(),
        }))
    }

    async fn owner_kind(
        &self,
        owner: &str,
        _host_repo: &RepoRef,
    ) -> Result<OwnerKind, WorkspaceError> {
        self.owner_kinds_asked
            .lock()
            .unwrap()
            .push(owner.to_owned());
        Ok(if self.organizations.iter().any(|o| o == owner) {
            OwnerKind::Organization
        } else {
            OwnerKind::User
        })
    }

    async fn authenticated_login(
        &self,
        _host_repo: &RepoRef,
    ) -> Result<Option<String>, WorkspaceError> {
        Ok(self.login.clone())
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

    /// The same fixture with an owner that may create repositories (`CREATE_REPO_OWNERS=acme`), so
    /// that `coder_tools` offers `create_repository` (it does not with the default settings, which
    /// name nobody).
    #[must_use]
    pub fn offering_creation(mut self) -> Self {
        Arc::get_mut(&mut self.env)
            .expect("the tools are not shared yet")
            .settings
            .create_repo_owners = vec!["acme".to_owned()];
        self
    }

    /// The same fixture with the processes of runs going through `environment`.
    #[must_use]
    pub fn using(mut self, environment: DynEnvironment) -> Self {
        Arc::get_mut(&mut self.env)
            .expect("the tools are not shared yet")
            .environment = environment;
        self
    }

    /// The same fixture whose tools wait up to `wait` for an environment that is not available now
    /// (`RUN_POD_WAIT_SECS`), instead of failing at the first `Unavailable`.
    #[must_use]
    pub fn waiting_for_slots(mut self, wait: std::time::Duration) -> Self {
        Arc::get_mut(&mut self.env)
            .expect("the tools are not shared yet")
            .unavailable_wait = Some(wait);
        self
    }

    /// The same fixture whose code host can create repositories ([`CreatingHost`]), for the
    /// organisations `organizations` (any other owner is a user), acting as `login` (`None`: an
    /// installation, which has no user), and for the owners `owners` (`CREATE_REPO_OWNERS`).
    #[must_use]
    pub fn creating(
        mut self,
        owners: &[&str],
        organizations: &[&str],
        login: Option<&str>,
    ) -> (Self, Arc<CreatingHost>) {
        let env = Arc::get_mut(&mut self.env).expect("the tools are not shared yet");
        let host = Arc::new(CreatingHost {
            inner: env.code_host.clone(),
            root: self.tmp.path().to_owned(),
            organizations: organizations.iter().map(|o| (*o).to_owned()).collect(),
            login: login.map(str::to_owned),
            created: Mutex::default(),
            taken: Mutex::default(),
            clone_url: Mutex::default(),
            answer_lost: Mutex::default(),
            owner_kinds_asked: Mutex::default(),
        });
        env.code_host = host.clone();
        env.settings.create_repo_owners = owners.iter().map(|o| (*o).to_owned()).collect();
        (self, host)
    }

    /// The same fixture whose tools tell a run that GitHub rejected its credentials to check `hint`
    /// (what a process with a GitHub App says instead of `GITHUB_TOKEN`).
    #[must_use]
    pub fn with_credentials_hint(mut self, hint: &'static str) -> Self {
        Arc::get_mut(&mut self.env)
            .expect("the tools are not shared yet")
            .credentials_hint = hint;
        self
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

    /// Commit `content` as `file` on `main` of the remote (through a clone of its own), as a
    /// repository that already has something to fix.
    pub fn commit_to_main(&self, file: &str, content: &str) {
        let clone = self
            .tmp
            .path()
            .join(format!("clone-{}", file.replace('/', "_")));
        git(
            self.tmp.path(),
            &[
                "clone",
                "--quiet",
                self.remote.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        if let Some(parent) = std::path::Path::new(file).parent() {
            std::fs::create_dir_all(clone.join(parent)).unwrap();
        }
        std::fs::write(clone.join(file), content).unwrap();
        git(&clone, &["add", "-A"]);
        git(
            &clone,
            &["commit", "--quiet", "-m", &format!("seed {file}")],
        );
        git(&clone, &["push", "--quiet", "origin", "main"]);
    }

    /// Another bare remote, `<tmp>/other/<name>.git`, on `main` with `files`: the second repository
    /// of a workspace. Returns its path.
    pub fn extra_remote(&self, name: &str, files: &[(&str, &str)]) -> PathBuf {
        let remote = self.tmp.path().join("other").join(format!("{name}.git"));
        let seed = self.tmp.path().join(format!("seed-{name}"));
        std::fs::create_dir_all(&remote).unwrap();
        std::fs::create_dir_all(&seed).unwrap();
        git(
            &remote,
            &["init", "--bare", "--quiet", "--initial-branch=main"],
        );
        git(&seed, &["init", "--quiet", "--initial-branch=main"]);
        for (file, content) in files {
            std::fs::write(seed.join(file), content).unwrap();
        }
        git(&seed, &["add", "-A"]);
        git(&seed, &["commit", "--quiet", "-m", "seed"]);
        git(
            &seed,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&seed, &["push", "--quiet", "origin", "main"]);
        remote
    }

    /// A bare remote with no ref at all, `<tmp>/other/<name>.git`: a repository that was just
    /// created. Returns its path.
    pub fn empty_remote(&self, name: &str) -> PathBuf {
        let remote = self.tmp.path().join("other").join(format!("{name}.git"));
        std::fs::create_dir_all(&remote).unwrap();
        git(
            &remote,
            &["init", "--bare", "--quiet", "--initial-branch=main"],
        );
        remote
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

/// [`folder`] without its `agent/mcp.json`: the shipped agent as a deployment that connects no MCP
/// server mounts it. The shipped file names the GitHub server's sidecar over http
/// (127.0.0.1:8082, with the coder's credentials bound to it by the deployment), which has to be
/// running, so a test that starts a worker on a folder, or assembles one, takes this and says itself
/// which servers it connects (`tests/binary.rs`, `tests/agent_files.rs`); the shipped file is
/// tested as it is.
pub fn plain_folder() -> TempDir {
    let tmp = folder();
    std::fs::remove_file(tmp.path().join("agent/mcp.json"))
        .expect("the shipped agent has an mcp.json");
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

/// An [`Environment`] for tests: commands still run in this container, but through a session that
/// records every spec it was asked to prepare and every process it was told to kill, and puts
/// `FAKE_ENV=<name>` in the environment of each (so a command can say where it ran). It also
/// records what it was asked to ensure and release, and can be made to fail or to say what it
/// holds.
pub struct FakeEnvironment {
    /// The workspace root, to say whether a run's workspace was there when it was released.
    root: Option<PathBuf>,
    pub session: Arc<FakeSession>,
    /// The runs `ensure` was called for.
    pub ensured: Mutex<Vec<String>>,
    /// The runs `release` was called for, and whether their workspace still existed then.
    pub released: Mutex<Vec<(String, bool)>>,
    /// What `held_runs` says.
    pub held: Mutex<Vec<String>>,
    /// The steps `ensure` reports before it returns.
    pub steps: Mutex<Vec<EnvStep>>,
    /// `ensure` fails with this, once set.
    pub ensure_fails: Mutex<Option<fn() -> EnvError>>,
    /// `release` fails while this is set.
    pub release_fails: AtomicBool,
    /// `ensure` never returns while this is set (an environment that takes forever to build).
    pub hang: AtomicBool,
    /// The next this many calls of `ensure` fail with `Unavailable` (a quota that is used up), and
    /// then it works.
    pub unavailable_for: AtomicUsize,
    /// The runs `rebuild` was called for, and whether it was to use the default.
    pub rebuilt: Mutex<Vec<(String, bool)>>,
    /// What `rebuild` says: whether there is anything of its own to make again.
    pub rebuild_says: AtomicBool,
    /// `ensure` fails with this after a `rebuild` (a build that goes wrong again).
    pub ensure_fails_after_rebuild: Mutex<Option<fn() -> EnvError>>,
}

/// The session of a [`FakeEnvironment`].
#[derive(Default)]
pub struct FakeSession {
    /// The specs it was asked to prepare, in order.
    pub specs: Mutex<Vec<ExecSpec>>,
    /// What it prepared.
    pub prepared: Mutex<Vec<ExecId>>,
    /// What it was told to kill.
    pub killed: Mutex<Vec<ExecId>>,
    /// Variables added to every prepared command.
    pub extra_env: Mutex<BTreeMap<String, String>>,
    /// What the session says it is; a devcontainer of an image `fake` when not set.
    pub kind: Mutex<Option<EnvKind>>,
    /// Where the session says the coder's OpenCode is (`EnvSession::tool_path`).
    pub opencode_at: Mutex<Option<PathBuf>>,
}

impl FakeEnvironment {
    /// A fake with nothing held, no steps, and nothing failing. `root` is the workspace root of the
    /// fixture, for [`released`](Self::released).
    pub fn new(root: Option<&Path>) -> Arc<Self> {
        let session = Arc::new(FakeSession::default());
        session
            .extra_env
            .lock()
            .unwrap()
            .insert("FAKE_ENV".to_owned(), "fake".to_owned());
        Arc::new(Self {
            root: root.map(Path::to_path_buf),
            session,
            ensured: Mutex::new(Vec::new()),
            released: Mutex::new(Vec::new()),
            held: Mutex::new(Vec::new()),
            steps: Mutex::new(Vec::new()),
            ensure_fails: Mutex::new(None),
            release_fails: AtomicBool::new(false),
            hang: AtomicBool::new(false),
            unavailable_for: AtomicUsize::new(0),
            rebuilt: Mutex::new(Vec::new()),
            rebuild_says: AtomicBool::new(true),
            ensure_fails_after_rebuild: Mutex::new(None),
        })
    }
}

#[async_trait]
impl Environment for FakeEnvironment {
    async fn ensure(
        &self,
        workspace: &RunWorkspace,
        progress: &dyn EnvProgress,
    ) -> Result<Arc<dyn EnvSession>, EnvError> {
        self.ensured
            .lock()
            .unwrap()
            .push(workspace.run().to_owned());
        for step in self.steps.lock().unwrap().iter() {
            progress.step(step.clone());
        }
        if self.hang.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        let left = self.unavailable_for.load(Ordering::SeqCst);
        if left > 0 {
            self.unavailable_for.store(left - 1, Ordering::SeqCst);
            return Err(EnvError::Unavailable(
                "the namespace's quota of run pods is used up".to_owned(),
            ));
        }
        if let Some(fail) = *self.ensure_fails.lock().unwrap() {
            return Err(fail());
        }
        Ok(self.session.clone())
    }

    async fn release(&self, run: &str) -> Result<(), EnvError> {
        let present = self
            .root
            .as_ref()
            .is_some_and(|root| root.join("workspaces").join(run).exists());
        self.released
            .lock()
            .unwrap()
            .push((run.to_owned(), present));
        if self.release_fails.load(Ordering::SeqCst) {
            return Err(EnvError::Unavailable("the runtime is down".to_owned()));
        }
        self.held.lock().unwrap().retain(|held| held != run);
        Ok(())
    }

    async fn held_runs(&self) -> Result<Vec<String>, EnvError> {
        Ok(self.held.lock().unwrap().clone())
    }

    async fn rebuild(&self, run: &str, use_default: bool) -> Result<bool, EnvError> {
        self.rebuilt
            .lock()
            .unwrap()
            .push((run.to_owned(), use_default));
        // What a rebuild clears: a broken environment is made again at the next `ensure`.
        *self.ensure_fails.lock().unwrap() = self.ensure_fails_after_rebuild.lock().unwrap().take();
        Ok(self.rebuild_says.load(Ordering::SeqCst))
    }
}

#[async_trait]
impl EnvSession for FakeSession {
    fn describe(&self) -> EnvDescription {
        EnvDescription {
            kind: self
                .kind
                .lock()
                .unwrap()
                .clone()
                .unwrap_or(EnvKind::DevContainer {
                    source: None,
                    image: "fake".to_owned(),
                }),
            summary: "a fake environment".to_owned(),
        }
    }

    fn tool_path(&self, name: &str) -> Option<PathBuf> {
        if name == "opencode" {
            self.opencode_at.lock().unwrap().clone()
        } else {
            None
        }
    }

    fn prepare(&self, spec: &ExecSpec) -> Result<PreparedCommand, EnvError> {
        let mut prepared = LocalSession.prepare(spec)?;
        for (name, value) in self.extra_env.lock().unwrap().iter() {
            prepared.env.insert(name.into(), value.into());
        }
        self.specs.lock().unwrap().push(spec.clone());
        self.prepared.lock().unwrap().push(prepared.exec.clone());
        Ok(prepared)
    }

    async fn kill(&self, exec: &ExecId) {
        self.killed.lock().unwrap().push(exec.clone());
    }

    fn secret_ref(&self, name: &str) -> Option<SecretRef> {
        (name == "model-key").then(|| SecretRef::File(PathBuf::from("/run/secrets/model-key")))
    }
}
