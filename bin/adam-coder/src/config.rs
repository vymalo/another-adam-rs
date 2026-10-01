//! Configuration of the `adam-coder` binary, read from the environment.
//!
//! The variables every agent binary shares (`ROLE`, `DATABASE_URL`, `A2A_BEARER_TOKENS`, `PUBLIC_URL`,
//! `LISTEN_ADDR`, `WORKERS`, `WORKER_ID`, `MODEL_*`, `MCP_ALLOW_*`) are read by
//! [`adam_service`] (`ServiceConfig`, `ModelConfig`, `McpSettings`), with the same names, defaults and
//! messages for every binary; the coder reads its own beside them.
//!
//! | Variable | Meaning | Default |
//! |---|---|---|
//! | `ROLE` | what this process runs: `all`, `control-plane` or `worker` ([`adam_host::Role`]) | `all` |
//! | `DATABASE_URL` | Postgres for the run store (`adam-store-postgres`) | required |
//! | `A2A_BEARER_TOKENS` | comma-separated tokens accepted by the A2A server | required for `all` and `control-plane`, non-empty (fail closed) |
//! | `PUBLIC_URL` | URL clients reach the JSON-RPC endpoint at (agent card) | required for `all` and `control-plane` |
//! | `LISTEN_ADDR` | bind address: the A2A server, or for `worker` its `/healthz` listener | `0.0.0.0:8080` |
//! | `MODEL_BASE_URL` | OpenAI-compatible gateway, with its `/v1` prefix | required for `all` and `worker` |
//! | `MODEL_API_KEY` | bearer token for it (may be empty for local servers) | required for `all` and `worker` |
//! | `MODEL` | model alias of the agent itself | required for `all` and `worker` |
//! | `OPENCODE_MODEL` | model alias OpenCode uses through the same gateway | `MODEL` |
//! | `GITHUB_TOKEN` | git push and pull request token; only ever sent to the `ALLOWED_REPO_HOSTS` | one of this or the App's three, for `all` and `worker` |
//! | `GITHUB_APP_ID` | the GitHub App's application ID or client ID (the JWT's `iss`); App mode, instead of `GITHUB_TOKEN` | required with the App's other two |
//! | `GITHUB_APP_INSTALLATION_ID` | the installation's ID, a positive integer | required in App mode |
//! | `GITHUB_APP_PRIVATE_KEY_PATH`, `GITHUB_APP_PRIVATE_KEY` | the App's private key, a PEM (PKCS#1 or PKCS#8), as a file or inline (`\n` escapes accepted); exactly one; parsed at startup | one required in App mode |
//! | `ALLOWED_REPO_HOSTS` | comma-separated hosts (`name` for any port, or `name:port`) repositories may live on; the token is scoped to them; the first is the host `owner/name` stands for | `github.com` |
//! | `ALLOW_LOCAL_REPOS` | also accept local paths, `file://` and plain `http://` repositories (development and tests only) | `false` |
//! | `GITHUB_API_URL` | GitHub REST API root (GitHub Enterprise: `https://<host>/api/v3`; tests: a mock) | `https://api.github.com` |
//! | `WORKSPACE_ROOT` | mirrors and worktrees (persistent storage) | `/work` |
//! | `WORKSPACE_PLACEMENT` | where the files of a run live: `shared`, `affinity` or `isolated` ([`adam_host::Placement`]); `a2a-only` is refused | `shared` |
//! | `WORKER_ID` | stable identity of this worker: the lease identity and, with `affinity` or `isolated`, the run owner; letters, digits, `.`, `_`, `-` | random per process; required for `affinity` and `isolated` |
//! | `WORKERS` | runs advanced concurrently by this process | `4` |
//! | `MAX_CHECK_CYCLES` | failed `run_checks` before the agent must stop | `3` |
//! | `WORKSPACE_SWEEP_SECS` | how often the janitor removes the workspaces of finished runs; `0` turns it off | `300` |
//! | `CHECK_TIMEOUT_SECS` | time limit of one `run_checks` command | `900` |
//! | `CHECK_OUTPUT_TAIL_BYTES` | output tail `run_checks` returns | `16384` |
//! | `GIT_AUTHOR_NAME`, `GIT_AUTHOR_EMAIL` | identity of the commits | `adam-coder`, `adam-coder@users.noreply.github.com` |
//! | `PR_DRAFT` | open pull requests as drafts (`true`/`false`) | `false` |
//! | `OPENCODE_COMMAND` | program that speaks ACP on stdio (arguments follow, whitespace-separated) | `opencode acp` |
//! | `MCP_ALLOW_STDIO` | let an agent folder's `mcp.json` start local processes (`command` servers) | `false` |
//! | `MCP_ALLOW_INSECURE` | let it reach plain-`http` MCP servers on other machines (development only) | `false` |
//! | `MCP_ALLOW_URL_VARS` | let it write `${VAR}` in a server's `url` (headers may always) | `false` |
//! | `ADAM_AGENT_DIR` | the folder that holds `agent/` (or `agent/` itself): the coder's instructions, card, skills and subagents, read once at startup by every role ([`AgentFiles`](crate::AgentFiles)); it must exist | unset: the copy embedded in the binary |
//!
//! # Roles
//!
//! `ROLE` picks the halves this process runs, and each half brings its own variables:
//!
//! * **Every role** needs `DATABASE_URL`.
//! * **The control plane** (`all`, `control-plane`) also needs `A2A_BEARER_TOKENS` and
//!   `PUBLIC_URL`: it serves A2A and starts, delivers to, cancels and views runs, which needs only
//!   the agent's name and its `init` ([`CoderStarter`](crate::CoderStarter)), so it holds no model
//!   or GitHub configuration.
//! * **The workers** (`all`, `worker`) need everything a step uses, the `MODEL_*`, `MODEL`
//!   variables and a GitHub credential (`GITHUB_TOKEN`, or the `GITHUB_APP_*` variables: both, or a
//!   partial App set, is refused with every problem listed), and read the rest of the table above (the workspace, the
//!   checks, the commit identity, OpenCode, what MCP servers a folder may start or reach). They arrive in [`Config::worker`] as a
//!   [`WorkerConfig`], which is `Some` exactly when [`Role::runs_workers`](adam_host::Role::runs_workers).
//!
//! # Workspace placement
//!
//! `WORKSPACE_PLACEMENT` and `WORKER_ID` are read by the roles that run workers (see
//! [ADR 0002](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0002-workspace-placement.md)):
//!
//! | `WORKSPACE_PLACEMENT` | Workspace root of the worker | Runs |
//! |---|---|---|
//! | `shared` (default) | `WORKSPACE_ROOT`, a volume every worker mounts | any worker steps any run |
//! | `affinity` | `WORKSPACE_ROOT/<WORKER_ID>` | pinned to the worker that first claimed them |
//! | `isolated` | `WORKSPACE_ROOT`, a volume of this worker only | pinned, as above |
//! | `a2a-only` | | refused: the coder's tools all need a workspace |
//!
//! A pinning placement without `WORKER_ID` is refused, because a random id would strand every
//! run at the next restart. A pinned run whose worker never returns is stranded (nothing adopts it).
//!
//! `ADAM_AGENT_DIR` is read by every role: the control plane serves the card from the folder, the workers
//! run its prompt. Its contents are checked when `serve` loads them (`name: coder`, valid files), before
//! anything connects; here only that the path is a directory.
//!
//! What a role does not use is not validated: a chart may set a variable for every role, and a
//! malformed `GITHUB_API_URL` does not stop a control plane. `LISTEN_ADDR` is read by every role.
//!
//! Every problem is reported at once, so a misconfigured deployment is fixed
//! in one round trip. Secrets are wrapped in [`SecretString`] and never appear
//! in `Debug` output.

use std::path::PathBuf;
use std::time::Duration;

use adam::AGENT_DIR_ENV;
use adam_host::Placement;
pub use adam_service::{ConfigError, McpSettings};
use adam_service::{ModelConfig, ServiceConfig, WorkerSettings, parse_flag, parse_or};
use adam_workspace::{AppKey, WorkspaceError};
use secrecy::{ExposeSecret as _, SecretString};
use url::Url;

/// The binary's configuration.
#[derive(Clone)]
pub struct Config {
    /// What every agent binary reads: the role, the database, how the process is reached.
    pub service: ServiceConfig,
    /// `ADAM_AGENT_DIR`: the folder the agent's files are read from, an existing directory.
    /// `None`: the copy embedded in the binary. Every role reads it.
    pub agent_dir: Option<PathBuf>,
    /// What stepping a run needs: the model, GitHub, the workspaces and the checks. `Some` exactly
    /// when [`Role::runs_workers`](adam_host::Role::runs_workers); a control plane holds none of it.
    pub worker: Option<WorkerConfig>,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("service", &self.service)
            .field("agent_dir", &self.agent_dir)
            .field("worker", &self.worker)
            .finish()
    }
}

/// The configuration of the roles that run workers (`all`, `worker`): everything a step of the
/// coder uses.
#[derive(Clone)]
pub struct WorkerConfig {
    /// `MODEL_BASE_URL`, `MODEL_API_KEY` and `MODEL`.
    pub model: ModelConfig,
    /// `OPENCODE_MODEL`.
    pub opencode_model: String,
    /// How the coder authenticates to GitHub: `GITHUB_TOKEN`, or the `GITHUB_APP_*` variables.
    pub github: GitHubAuth,
    /// `ALLOWED_REPO_HOSTS`, lowercased.
    pub allowed_repo_hosts: Vec<String>,
    /// `ALLOW_LOCAL_REPOS`.
    pub allow_local_repos: bool,
    /// `GITHUB_API_URL`.
    pub github_api_url: Url,
    /// `WORKSPACE_ROOT`. Use [`WorkerConfig::placed_root`] for the folder the worker works in.
    pub workspace_root: PathBuf,
    /// `WORKSPACE_PLACEMENT`. Never [`Placement::A2aOnly`]: the parser refuses it.
    pub placement: Placement,
    /// `WORKER_ID`. `Some` whenever [`Placement::pins_runs`].
    pub worker_id: Option<String>,
    /// `WORKERS`.
    pub workers: usize,
    /// `MAX_CHECK_CYCLES`.
    pub max_check_cycles: u32,
    /// `WORKSPACE_SWEEP_SECS`: how often the janitor sweeps the workspaces of finished runs;
    /// `None` (the variable is `0`) turns it off.
    pub workspace_sweep: Option<Duration>,
    /// `CHECK_TIMEOUT_SECS`.
    pub check_timeout: Duration,
    /// `CHECK_OUTPUT_TAIL_BYTES`.
    pub check_output_tail: usize,
    /// `GIT_AUTHOR_NAME`.
    pub git_author_name: String,
    /// `GIT_AUTHOR_EMAIL`.
    pub git_author_email: String,
    /// `PR_DRAFT`.
    pub pr_draft: bool,
    /// `OPENCODE_COMMAND`, split into program and arguments.
    pub opencode_command: Vec<String>,
    /// `MCP_ALLOW_*`: what the MCP servers of the agent folder's `mcp.json` may be.
    pub mcp: McpSettings,
}

/// How the coder authenticates to GitHub, chosen per installation: **exactly one** of a personal
/// access token and a GitHub App.
#[derive(Clone)]
pub enum GitHubAuth {
    /// `GITHUB_TOKEN`: one token for every repository, for as long as it is valid.
    Token(SecretString),
    /// `GITHUB_APP_ID`, `GITHUB_APP_INSTALLATION_ID` and a private key: installation access tokens,
    /// minted as they are needed and good for an hour.
    App(GitHubAppConfig),
}

/// The GitHub App of [`GitHubAuth::App`].
#[derive(Clone)]
pub struct GitHubAppConfig {
    /// `GITHUB_APP_ID`: the App's application ID, or its client ID (the JWT's `iss`).
    pub app_id: String,
    /// `GITHUB_APP_INSTALLATION_ID`: a positive integer.
    pub installation_id: u64,
    /// The private key, parsed at startup (`GITHUB_APP_PRIVATE_KEY_PATH` or `GITHUB_APP_PRIVATE_KEY`).
    pub key: AppKey,
    /// The PEM the key was read from, kept so that the redactor can register it.
    pub pem: SecretString,
}

impl GitHubAuth {
    /// The secrets of this way of authenticating that are known at startup, for the redactor: the
    /// token, or the App's PEM and the base64 body of it (what a log line that flattened it would
    /// carry). The installation tokens an App mints are only known when they are minted.
    pub fn secrets(&self) -> Vec<String> {
        match self {
            Self::Token(token) => vec![token.expose_secret().to_owned()],
            Self::App(app) => {
                let pem = app.pem.expose_secret();
                let body: String = pem
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty() && !line.starts_with("-----"))
                    .collect();
                vec![pem.trim().to_owned(), body]
            }
        }
    }

    /// What to tell a model (and a person) to check when GitHub rejects the credentials: the
    /// variables of this way of authenticating.
    pub fn check_hint(&self) -> &'static str {
        match self {
            Self::Token(_) => {
                "GITHUB_TOKEN is valid and may push and open pull requests for the repository"
            }
            Self::App(_) => {
                "the GitHub App's credentials (GITHUB_APP_ID, GITHUB_APP_INSTALLATION_ID and the \
                 private key) are valid and that the App is installed on the repository with write \
                 access to its contents and pull requests"
            }
        }
    }

    /// Read the variables: `GITHUB_TOKEN` for a personal access token, or the three `GITHUB_APP_*`
    /// for an App. One of the two, never both and never a part of the App's. Every problem is added
    /// to `problems` and names its variable, never a value; `None` only after one was.
    fn parse(get: &impl Fn(&str) -> Option<String>, problems: &mut Vec<String>) -> Option<Self> {
        let token = get("GITHUB_TOKEN");
        let app_id = get("GITHUB_APP_ID");
        let installation = get("GITHUB_APP_INSTALLATION_ID");
        let key_path = get("GITHUB_APP_PRIVATE_KEY_PATH");
        let key_inline = get("GITHUB_APP_PRIVATE_KEY");
        let any_app = app_id.is_some()
            || installation.is_some()
            || key_path.is_some()
            || key_inline.is_some();
        match (token, any_app) {
            (Some(token), false) => Some(Self::Token(SecretString::from(token))),
            (None, false) => {
                problems.push(
                    "GITHUB_TOKEN is required (a personal access token), or GITHUB_APP_ID, \
                     GITHUB_APP_INSTALLATION_ID and GITHUB_APP_PRIVATE_KEY_PATH (a GitHub App)"
                        .into(),
                );
                None
            }
            (Some(_), true) => {
                problems.push(
                    "GITHUB_TOKEN and the GITHUB_APP_* variables are both set: configure one way, \
                     a personal access token or a GitHub App (leave GITHUB_TOKEN unset or empty \
                     for an App)"
                        .into(),
                );
                None
            }
            (None, true) => Self::parse_app(app_id, installation, key_path, key_inline, problems),
        }
    }

    fn parse_app(
        app_id: Option<String>,
        installation: Option<String>,
        key_path: Option<String>,
        key_inline: Option<String>,
        problems: &mut Vec<String>,
    ) -> Option<Self> {
        let before = problems.len();
        let app_id = app_id.map(|id| id.trim().to_owned());
        if app_id.is_none() {
            problems.push("GITHUB_APP_ID is required for a GitHub App".into());
        }
        let installation_id = match installation.as_deref().map(str::trim) {
            None => {
                problems.push("GITHUB_APP_INSTALLATION_ID is required for a GitHub App".into());
                None
            }
            Some(raw) => match raw.parse::<u64>() {
                Ok(id) if id > 0 => Some(id),
                _ => {
                    problems.push("GITHUB_APP_INSTALLATION_ID must be a positive integer".into());
                    None
                }
            },
        };
        // Exactly one source of the key. The file is what a deployment mounts; the variable holds
        // the PEM itself, often with its newlines written as `\n` by a secret store or an env file.
        let (var, pem) = match (key_path, key_inline) {
            (Some(_), Some(_)) => {
                problems.push(
                    "GITHUB_APP_PRIVATE_KEY_PATH and GITHUB_APP_PRIVATE_KEY are both set: use one"
                        .into(),
                );
                return None;
            }
            (None, None) => {
                problems.push(
                    "GITHUB_APP_PRIVATE_KEY_PATH (a file) or GITHUB_APP_PRIVATE_KEY (the PEM) is \
                     required for a GitHub App"
                        .into(),
                );
                return None;
            }
            (Some(path), None) => match std::fs::read_to_string(path.trim()) {
                Ok(pem) => ("GITHUB_APP_PRIVATE_KEY_PATH", pem),
                Err(e) => {
                    problems.push(format!(
                        "GITHUB_APP_PRIVATE_KEY_PATH {:?} cannot be read as text ({})",
                        path.trim(),
                        e.kind()
                    ));
                    return None;
                }
            },
            (None, Some(inline)) => ("GITHUB_APP_PRIVATE_KEY", inline.replace("\\n", "\n")),
        };
        let key = match AppKey::from_pem(&pem) {
            Ok(key) => Some(key),
            Err(WorkspaceError::Invalid(why) | WorkspaceError::Auth(why)) => {
                problems.push(format!("{var}: {why}"));
                None
            }
            Err(e) => {
                problems.push(format!("{var}: {e}"));
                None
            }
        };
        if problems.len() > before {
            return None;
        }
        Some(Self::App(GitHubAppConfig {
            app_id: app_id?,
            installation_id: installation_id?,
            key: key?,
            pem: SecretString::from(pem),
        }))
    }
}

impl std::fmt::Debug for GitHubAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Token(_) => f.write_str("GitHubAuth::Token(<redacted>)"),
            Self::App(app) => f
                .debug_struct("GitHubAuth::App")
                .field("app_id", &app.app_id)
                .field("installation_id", &app.installation_id)
                .finish_non_exhaustive(),
        }
    }
}

impl std::fmt::Debug for WorkerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerConfig")
            .field("model", &self.model)
            .field("opencode_model", &self.opencode_model)
            .field("github", &self.github)
            .field("allowed_repo_hosts", &self.allowed_repo_hosts)
            .field("allow_local_repos", &self.allow_local_repos)
            .field("github_api_url", &self.github_api_url.as_str())
            .field("workspace_root", &self.workspace_root)
            .field("placement", &self.placement)
            .field("worker_id", &self.worker_id)
            .field("workers", &self.workers)
            .field("max_check_cycles", &self.max_check_cycles)
            .field("workspace_sweep", &self.workspace_sweep)
            .field("check_timeout", &self.check_timeout)
            .field("check_output_tail", &self.check_output_tail)
            .field("pr_draft", &self.pr_draft)
            .field("opencode_command", &self.opencode_command)
            .field("mcp", &self.mcp)
            .finish_non_exhaustive()
    }
}

impl Config {
    /// Read the process environment.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] listing every missing or malformed variable.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Read variables through `lookup` (`None` = unset). Blank values count as
    /// unset, except `MODEL_API_KEY`, which may be empty.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] listing every missing or malformed variable.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let mut problems = Vec::new();
        let get = |name: &str| lookup(name).filter(|v| !v.trim().is_empty());

        // The role, the database, the front and the worker settings: the same for every binary.
        let service = ServiceConfig::parse(&lookup, &mut problems);

        let agent_dir = match get(AGENT_DIR_ENV) {
            None => None,
            Some(raw) => {
                let path = PathBuf::from(raw);
                if path.is_dir() {
                    Some(path)
                } else {
                    problems.push(format!(
                        "{AGENT_DIR_ENV} {:?} is not a directory (name the folder that holds \
                         `agent/`, or `agent/` itself; unset it to use the copy embedded in the binary)",
                        path.display().to_string()
                    ));
                    None
                }
            }
        };

        // The workers' variables: a control plane neither needs nor validates them.
        let worker = match &service.worker {
            Some(settings) => WorkerConfig::parse(&lookup, &get, settings, &mut problems),
            None => None,
        };

        ConfigError::check(problems)?;
        Ok(Self {
            service,
            agent_dir,
            worker,
        })
    }
}

impl WorkerConfig {
    /// The folder this worker keeps mirrors and worktrees in: `WORKSPACE_ROOT` itself, or with
    /// [`Placement::Affinity`] the folder `WORKSPACE_ROOT/<WORKER_ID>` it owns.
    pub fn placed_root(&self) -> PathBuf {
        match self.placement {
            Placement::Affinity => match &self.worker_id {
                Some(id) => self.workspace_root.join(id),
                // `Config` refuses this combination.
                None => self.workspace_root.clone(),
            },
            Placement::Shared | Placement::Isolated | Placement::A2aOnly => {
                self.workspace_root.clone()
            }
        }
    }

    /// Read the workers' variables, adding one line to `problems` per missing or malformed one.
    /// `settings` are `WORKERS` and `WORKER_ID`, which the service read (and checked) already.
    /// `None` only after a problem was recorded.
    fn parse(
        lookup: &impl Fn(&str) -> Option<String>,
        get: &impl Fn(&str) -> Option<String>,
        settings: &WorkerSettings,
        problems: &mut Vec<String>,
    ) -> Option<Self> {
        let github = GitHubAuth::parse(get, problems);
        let model = ModelConfig::parse(lookup, problems);

        let opencode_model = get("OPENCODE_MODEL").unwrap_or_else(|| model.alias.clone());
        let workspace_root =
            PathBuf::from(get("WORKSPACE_ROOT").unwrap_or_else(|| "/work".to_owned()));

        let placement = match Placement::from_optional(lookup("WORKSPACE_PLACEMENT").as_deref()) {
            Ok(Placement::A2aOnly) => {
                problems.push(
                    "WORKSPACE_PLACEMENT=a2a-only is not supported by adam-coder's worker roles: \
                     its tools need a workspace (use shared, affinity or isolated)"
                        .into(),
                );
                Placement::default()
            }
            Ok(placement) => placement,
            Err(e) => {
                problems.push(format!("WORKSPACE_PLACEMENT is invalid: {e}"));
                Placement::default()
            }
        };
        // A pinning placement needs an id that survives a restart (the format was checked with
        // the other service variables).
        if settings.worker_id.is_none() && placement.pins_runs() {
            problems.push(format!(
                "WORKER_ID is required when WORKSPACE_PLACEMENT is {placement}: a run stays on the \
                 worker that owns it, so the id must be stable across restarts (a StatefulSet pod name)"
            ));
        }

        let max_check_cycles = parse_or(get, "MAX_CHECK_CYCLES", 3u32, problems);
        if max_check_cycles == 0 {
            problems.push("MAX_CHECK_CYCLES must be at least 1".into());
        }
        // The janitor of the workspaces of finished runs: every five minutes, `0` is off.
        let workspace_sweep =
            Some(parse_or(get, "WORKSPACE_SWEEP_SECS", 300u64, problems)).filter(|secs| *secs > 0);
        let check_timeout =
            Duration::from_secs(parse_or(get, "CHECK_TIMEOUT_SECS", 900u64, problems).max(1));
        let check_output_tail =
            parse_or(get, "CHECK_OUTPUT_TAIL_BYTES", 16_384usize, problems).max(256);
        let pr_draft = parse_flag(get, "PR_DRAFT", problems);
        let allow_local_repos = parse_flag(get, "ALLOW_LOCAL_REPOS", problems);
        let mcp = McpSettings::parse(lookup, problems);
        let allowed_repo_hosts = match get("ALLOWED_REPO_HOSTS") {
            None => vec![DEFAULT_REPO_HOST.to_owned()],
            Some(raw) => {
                let hosts: Vec<String> = raw
                    .split(',')
                    .map(|h| h.trim().to_ascii_lowercase())
                    .filter(|h| !h.is_empty())
                    .collect();
                if hosts.is_empty() {
                    problems.push("ALLOWED_REPO_HOSTS has no usable host".into());
                }
                for host in &hosts {
                    if !is_host_entry(host) {
                        problems.push(format!(
                            "ALLOWED_REPO_HOSTS entry {host:?} is not a host name (use `github.com` or `host:port`, no scheme or path)"
                        ));
                    }
                }
                hosts
            }
        };
        let github_api_url = match get("GITHUB_API_URL") {
            None => Url::parse(DEFAULT_GITHUB_API_URL).ok(),
            Some(raw) => match Url::parse(raw.trim()) {
                Ok(u) if matches!(u.scheme(), "http" | "https") && u.host().is_some() => Some(u),
                Ok(_) => {
                    problems.push("GITHUB_API_URL must be an http(s) URL".into());
                    None
                }
                Err(e) => {
                    problems.push(format!("GITHUB_API_URL is not a URL: {e}"));
                    None
                }
            },
        };
        let opencode_command: Vec<String> = get("OPENCODE_COMMAND")
            .unwrap_or_else(|| "opencode acp".to_owned())
            .split_whitespace()
            .map(str::to_owned)
            .collect();

        // `github` and `github_api_url` are `None` only after a problem was recorded above.
        let github = github?;
        let github_api_url = github_api_url?;
        Some(Self {
            model,
            opencode_model,
            github,
            allowed_repo_hosts,
            allow_local_repos,
            github_api_url,
            workspace_root,
            placement,
            worker_id: settings.worker_id.clone(),
            workers: settings.workers,
            max_check_cycles,
            workspace_sweep: workspace_sweep.map(Duration::from_secs),
            check_timeout,
            check_output_tail,
            git_author_name: get("GIT_AUTHOR_NAME").unwrap_or_else(|| "adam-coder".to_owned()),
            git_author_email: get("GIT_AUTHOR_EMAIL")
                .unwrap_or_else(|| "adam-coder@users.noreply.github.com".to_owned()),
            pr_draft,
            opencode_command,
            mcp,
        })
    }
}

/// Repository host used when `ALLOWED_REPO_HOSTS` is unset.
const DEFAULT_REPO_HOST: &str = "github.com";
/// API root used when `GITHUB_API_URL` is unset.
const DEFAULT_GITHUB_API_URL: &str = "https://api.github.com";

/// `name` or `name:port`: letters, digits, dots and dashes, nothing that could
/// smuggle a scheme, path, userinfo or wildcard into an allowlist.
fn is_host_entry(entry: &str) -> bool {
    let (name, port) = match entry.split_once(':') {
        Some((name, port)) => (name, Some(port)),
        None => (entry, None),
    };
    !name.is_empty()
        && !name.starts_with(['.', '-'])
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
        && port.is_none_or(|p| !p.is_empty() && p.parse::<u16>().is_ok())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use adam_error::{Classify, ErrorClass};
    use adam_host::Role;
    use secrecy::ExposeSecret;

    use super::*;

    fn full() -> HashMap<&'static str, &'static str> {
        HashMap::from([
            ("DATABASE_URL", "postgres://u:hunter2@db/adam"),
            ("MODEL_BASE_URL", "https://gw.example/v1"),
            ("MODEL_API_KEY", "sk-secret"),
            ("MODEL", "coder-large"),
            ("GITHUB_TOKEN", "ghp_secret"),
            ("A2A_BEARER_TOKENS", "one, two ,,"),
            ("PUBLIC_URL", "http://coder.svc:8080/"),
        ])
    }

    fn parse(vars: &HashMap<&'static str, &'static str>) -> Result<Config, ConfigError> {
        Config::from_lookup(|k| vars.get(k).map(|v| (*v).to_owned()))
    }

    #[test]
    fn defaults_apply_and_tokens_are_split() {
        let c = parse(&full()).expect("valid");
        assert_eq!(c.service.listen_addr, "0.0.0.0:8080".parse().unwrap());
        let tokens: Vec<_> = c
            .service
            .a2a_bearer_tokens
            .iter()
            .map(|t| t.expose_secret().to_owned())
            .collect();
        assert_eq!(tokens, ["one", "two"]);
        let c = c.worker.expect("the default role runs workers");
        assert_eq!(c.workspace_root, PathBuf::from("/work"));
        assert_eq!(c.workers, 4);
        assert_eq!(c.max_check_cycles, 3);
        assert_eq!(c.opencode_model, "coder-large");
        assert_eq!(c.opencode_command, ["opencode", "acp"]);
        assert_eq!(c.allowed_repo_hosts, ["github.com"]);
        assert!(!c.allow_local_repos, "local repositories are opt-in");
        assert_eq!(c.github_api_url.as_str(), "https://api.github.com/");
        assert!(!c.pr_draft);
        assert_eq!(
            c.workspace_sweep,
            Some(Duration::from_secs(300)),
            "the janitor sweeps every five minutes"
        );
    }

    #[test]
    fn the_sweep_of_finished_workspaces_is_every_n_seconds_and_zero_is_off() {
        let sweep = |value: &'static str| {
            let mut vars = full();
            vars.insert("WORKSPACE_SWEEP_SECS", value);
            parse(&vars).map(|c| c.worker.unwrap().workspace_sweep)
        };
        assert_eq!(sweep("60").unwrap(), Some(Duration::from_secs(60)));
        assert_eq!(sweep("0").unwrap(), None, "zero turns the janitor off");
        for bad in ["often", "-1", "1.5"] {
            let err = sweep(bad).unwrap_err();
            assert!(
                err.problems
                    .iter()
                    .any(|p| p.starts_with("WORKSPACE_SWEEP_SECS")),
                "{bad}: {:?}",
                err.problems
            );
        }
        // A control plane runs no workers, so it has no use for it and does not read it.
        let mut vars = full();
        vars.insert("ROLE", "control-plane");
        vars.insert("WORKSPACE_SWEEP_SECS", "often");
        assert!(parse(&vars).is_ok());
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let err = parse(&HashMap::new()).unwrap_err();
        for name in [
            "DATABASE_URL",
            "MODEL_BASE_URL",
            "MODEL_API_KEY",
            "MODEL",
            "GITHUB_TOKEN",
            "PUBLIC_URL",
            "A2A_BEARER_TOKENS",
        ] {
            assert!(
                err.problems.iter().any(|p| p.starts_with(name)),
                "{name} missing from {:?}",
                err.problems
            );
        }
    }

    #[test]
    fn fail_closed_on_tokens_and_validate_numbers() {
        let mut vars = full();
        vars.insert("A2A_BEARER_TOKENS", " , ");
        assert!(
            parse(&vars).is_err(),
            "no usable token must not start the server"
        );

        let mut vars = full();
        vars.insert("WORKERS", "0");
        vars.insert("MAX_CHECK_CYCLES", "many");
        vars.insert("PR_DRAFT", "maybe");
        vars.insert("PUBLIC_URL", "ftp://x");
        let err = parse(&vars).unwrap_err();
        assert!(err.problems.len() >= 4, "{:?}", err.problems);
    }

    #[test]
    fn an_empty_api_key_is_allowed_but_an_unset_one_is_not() {
        let mut vars = full();
        vars.insert("MODEL_API_KEY", "");
        assert!(parse(&vars).is_ok());
        vars.remove("MODEL_API_KEY");
        assert!(parse(&vars).is_err());
    }

    #[test]
    fn debug_output_hides_secrets() {
        let c = parse(&full()).unwrap();
        let text = format!("{c:?}");
        // The short tokens are checked in their quoted form: a bare "one" is inside "None".
        for secret in ["hunter2", "sk-secret", "ghp_secret", "\"one\"", "\"two\""] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
    }

    #[test]
    fn repository_hosts_are_normalised_and_validated() {
        let mut vars = full();
        vars.insert("ALLOWED_REPO_HOSTS", " GitHub.com, ghe.example.com:8443 ,,");
        let c = parse(&vars).expect("valid").worker.unwrap();
        assert_eq!(c.allowed_repo_hosts, ["github.com", "ghe.example.com:8443"]);

        for bad in [
            "https://github.com",
            "github.com/o/r",
            "*.github.com",
            "user@github.com",
            "github.com:",
            "github.com:notaport",
            "-x.com",
            " , ",
        ] {
            let mut vars = full();
            vars.insert("ALLOWED_REPO_HOSTS", bad);
            let err = parse(&vars).unwrap_err();
            assert!(
                err.problems
                    .iter()
                    .any(|p| p.starts_with("ALLOWED_REPO_HOSTS")),
                "{bad:?} accepted or misreported: {:?}",
                err.problems
            );
        }
    }

    #[test]
    fn github_api_url_and_local_repos_are_configurable_and_checked() {
        let mut vars = full();
        vars.insert("GITHUB_API_URL", "http://127.0.0.1:9999/api/v3");
        vars.insert("ALLOW_LOCAL_REPOS", "true");
        let c = parse(&vars).expect("valid").worker.unwrap();
        assert_eq!(c.github_api_url.as_str(), "http://127.0.0.1:9999/api/v3");
        assert!(c.allow_local_repos);

        let mut vars = full();
        vars.insert("GITHUB_API_URL", "ftp://api.example");
        vars.insert("ALLOW_LOCAL_REPOS", "yes please");
        let err = parse(&vars).unwrap_err();
        for name in ["GITHUB_API_URL", "ALLOW_LOCAL_REPOS"] {
            assert!(
                err.problems.iter().any(|p| p.starts_with(name)),
                "{name} missing from {:?}",
                err.problems
            );
        }
        let mut vars = full();
        vars.insert("GITHUB_API_URL", "not a url");
        assert!(parse(&vars).is_err());
    }

    #[test]
    fn the_mcp_flags_are_off_by_default_and_each_one_is_read() {
        let w = worker(&full());
        assert_eq!(w.mcp, McpSettings::default());
        let policy = w.mcp.policy();
        assert!(!policy.stdio_allowed());
        assert!(!policy.insecure_allowed());
        assert!(!policy.url_secrets_allowed());

        let mut vars = full();
        vars.insert("MCP_ALLOW_STDIO", "true");
        vars.insert("MCP_ALLOW_INSECURE", "1");
        vars.insert("MCP_ALLOW_URL_VARS", "TRUE");
        let w = worker(&vars);
        assert_eq!(
            w.mcp,
            McpSettings {
                allow_stdio: true,
                allow_insecure: true,
                allow_url_vars: true
            }
        );
        let policy = w.mcp.policy();
        assert!(
            policy.stdio_allowed() && policy.insecure_allowed() && policy.url_secrets_allowed()
        );

        // One at a time: each variable maps to its own flag.
        for (name, expected) in [
            ("MCP_ALLOW_STDIO", (true, false, false)),
            ("MCP_ALLOW_INSECURE", (false, true, false)),
            ("MCP_ALLOW_URL_VARS", (false, false, true)),
        ] {
            let mut vars = full();
            vars.insert(name, "true");
            let m = worker(&vars).mcp;
            assert_eq!(
                (m.allow_stdio, m.allow_insecure, m.allow_url_vars),
                expected
            );
        }
    }

    #[test]
    fn a_bad_mcp_flag_names_the_variable() {
        let mut vars = full();
        vars.insert("MCP_ALLOW_STDIO", "sometimes");
        vars.insert("MCP_ALLOW_INSECURE", "yes");
        vars.insert("MCP_ALLOW_URL_VARS", "2");
        let err = parse(&vars).unwrap_err();
        for name in [
            "MCP_ALLOW_STDIO",
            "MCP_ALLOW_INSECURE",
            "MCP_ALLOW_URL_VARS",
        ] {
            assert!(
                err.problems
                    .iter()
                    .any(|p| p.starts_with(name) && p.contains("true or false")),
                "{name} missing from {:?}",
                err.problems
            );
        }
        // A control plane connects no MCP servers, so it does not validate them.
        vars.insert("ROLE", "control-plane");
        assert!(parse(&vars).is_ok());
    }

    fn with_role(role: &'static str) -> HashMap<&'static str, &'static str> {
        let mut vars = full();
        vars.insert("ROLE", role);
        vars
    }

    #[test]
    fn the_role_defaults_to_all_and_each_value_parses() {
        assert_eq!(parse(&full()).unwrap().service.role, Role::All);
        // Blank counts as unset, like every other variable.
        assert_eq!(parse(&with_role("  ")).unwrap().service.role, Role::All);
        for role in Role::VALUES {
            assert_eq!(parse(&with_role(role.as_str())).unwrap().service.role, role);
        }
        // `adam_host` trims and ignores ASCII case.
        assert_eq!(
            parse(&with_role(" Control-Plane ")).unwrap().service.role,
            Role::ControlPlane
        );
    }

    #[test]
    fn an_unknown_role_names_the_variable_and_the_accepted_values() {
        for bad in ["boss", "controlplane", "workers", "front"] {
            let err = parse(&with_role(bad)).unwrap_err();
            let problem = err
                .problems
                .iter()
                .find(|p| p.starts_with("ROLE"))
                .unwrap_or_else(|| panic!("{bad:?} accepted or misreported: {:?}", err.problems));
            assert!(problem.contains(&format!("{bad:?}")), "{problem}");
            for accepted in ["all", "control-plane", "worker"] {
                assert!(problem.contains(accepted), "{problem}");
            }
            assert_eq!(err.class(), ErrorClass::Invalid);
        }
    }

    #[test]
    fn a_worker_needs_no_front_variables() {
        let mut vars = with_role("worker");
        vars.remove("A2A_BEARER_TOKENS");
        vars.remove("PUBLIC_URL");
        let c = parse(&vars).expect("a worker serves no A2A");
        assert_eq!(c.service.role, Role::Worker);
        assert!(c.service.a2a_bearer_tokens.is_empty());
        assert!(c.service.public_url.is_none());

        // What a worker does not use is not validated either: a chart may set it for all roles.
        vars.insert("PUBLIC_URL", "ftp://not-used");
        vars.insert("A2A_BEARER_TOKENS", " , ");
        assert!(parse(&vars).is_ok());
    }

    #[test]
    fn the_roles_that_run_the_control_plane_need_the_front_variables() {
        for role in ["all", "control-plane"] {
            let mut vars = with_role(role);
            vars.remove("A2A_BEARER_TOKENS");
            vars.remove("PUBLIC_URL");
            let err = parse(&vars).unwrap_err();
            for name in ["A2A_BEARER_TOKENS", "PUBLIC_URL"] {
                assert!(
                    err.problems.iter().any(|p| p.starts_with(name)),
                    "{role}: {name} missing from {:?}",
                    err.problems
                );
            }
            // Fail closed: a blank list is no list.
            let mut vars = with_role(role);
            vars.insert("A2A_BEARER_TOKENS", " , ");
            assert!(parse(&vars).is_err(), "{role}");
            let c = parse(&with_role(role)).unwrap();
            assert_eq!(c.service.a2a_bearer_tokens.len(), 2);
            assert_eq!(
                c.service.public_url.unwrap().as_str(),
                "http://coder.svc:8080/"
            );
        }
    }

    #[test]
    fn the_worker_roles_need_the_model_and_github_variables() {
        for role in Role::VALUES.into_iter().filter(|r| r.runs_workers()) {
            for name in ["MODEL_BASE_URL", "MODEL_API_KEY", "MODEL", "GITHUB_TOKEN"] {
                let mut vars = with_role(role.as_str());
                vars.remove(name);
                let err = parse(&vars).unwrap_err();
                assert!(
                    err.problems.iter().any(|p| p.starts_with(name)),
                    "{role}: {name} missing from {:?}",
                    err.problems
                );
            }
            // What a worker uses is validated, every problem at once.
            let mut vars = with_role(role.as_str());
            vars.insert("GITHUB_API_URL", "garbage");
            vars.insert("WORKERS", "0");
            let err = parse(&vars).unwrap_err();
            for name in ["GITHUB_API_URL", "WORKERS"] {
                assert!(
                    err.problems.iter().any(|p| p.starts_with(name)),
                    "{role}: {name} missing from {:?}",
                    err.problems
                );
            }
        }
    }

    #[test]
    fn a_control_plane_needs_no_model_github_or_workspace_variables() {
        let mut vars = with_role("control-plane");
        for name in ["MODEL_BASE_URL", "MODEL_API_KEY", "MODEL", "GITHUB_TOKEN"] {
            vars.remove(name);
        }
        let c = parse(&vars).expect("a control plane starts runs, it does not step them");
        assert_eq!(c.service.role, Role::ControlPlane);
        assert!(c.worker.is_none());
        assert_eq!(c.service.a2a_bearer_tokens.len(), 2);
        assert!(c.service.public_url.is_some());

        // What only a worker uses is not validated either: a chart may set it for every role.
        vars.insert("GITHUB_API_URL", "garbage");
        vars.insert("WORKERS", "0");
        vars.insert("MAX_CHECK_CYCLES", "many");
        vars.insert("PR_DRAFT", "maybe");
        vars.insert("ALLOWED_REPO_HOSTS", "https://x");
        assert!(parse(&vars).unwrap().worker.is_none());

        // The database and the front's own variables are still required.
        for name in ["DATABASE_URL", "A2A_BEARER_TOKENS", "PUBLIC_URL"] {
            let mut vars = vars.clone();
            vars.remove(name);
            let err = parse(&vars).unwrap_err();
            assert!(
                err.problems.iter().any(|p| p.starts_with(name)),
                "{name} missing from {:?}",
                err.problems
            );
        }
    }

    #[test]
    fn worker_config_is_some_exactly_when_the_role_runs_workers() {
        for role in Role::VALUES {
            let c = parse(&with_role(role.as_str())).unwrap();
            assert_eq!(c.worker.is_some(), role.runs_workers(), "{role}");
        }
    }

    #[test]
    fn a_worker_configs_debug_output_hides_secrets() {
        let c = parse(&with_role("worker")).unwrap();
        let text = format!("{:?}", c.worker.unwrap());
        assert!(text.contains("WorkerConfig"), "{text}");
        for secret in ["sk-secret", "ghp_secret"] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
    }

    #[test]
    fn the_role_is_shown_in_debug_output() {
        let c = parse(&with_role("worker")).unwrap();
        assert!(format!("{c:?}").contains("role: Worker"));
    }

    // -- workspace placement ------------------------------------------------------------------

    fn with_placement(
        placement: &'static str,
        worker_id: Option<&'static str>,
    ) -> HashMap<&'static str, &'static str> {
        let mut vars = full();
        vars.insert("WORKSPACE_PLACEMENT", placement);
        if let Some(id) = worker_id {
            vars.insert("WORKER_ID", id);
        }
        vars
    }

    fn worker(vars: &HashMap<&'static str, &'static str>) -> WorkerConfig {
        parse(vars).expect("valid").worker.expect("runs workers")
    }

    #[test]
    fn placement_defaults_to_shared_with_no_worker_id() {
        let w = worker(&full());
        assert_eq!(w.placement, Placement::Shared);
        assert_eq!(w.worker_id, None);
        assert_eq!(w.placed_root(), PathBuf::from("/work"));
        // Blank counts as unset.
        let mut vars = full();
        vars.insert("WORKSPACE_PLACEMENT", "  ");
        vars.insert("WORKER_ID", "  ");
        let w = worker(&vars);
        assert_eq!((w.placement, w.worker_id), (Placement::Shared, None));
    }

    #[test]
    fn shared_placement_accepts_a_worker_id_and_keeps_the_root() {
        let w = worker(&with_placement("Shared", Some("coder-0")));
        assert_eq!(w.placement, Placement::Shared);
        assert_eq!(w.worker_id.as_deref(), Some("coder-0"));
        assert_eq!(w.placed_root(), PathBuf::from("/work"));
    }

    #[test]
    fn affinity_joins_the_worker_id_under_the_root() {
        let w = worker(&with_placement(" affinity ", Some("coder-1")));
        assert_eq!(w.placement, Placement::Affinity);
        assert!(w.placement.pins_runs());
        assert_eq!(w.workspace_root, PathBuf::from("/work"));
        assert_eq!(w.placed_root(), PathBuf::from("/work/coder-1"));
    }

    #[test]
    fn isolated_uses_the_root_as_it_is_and_pins_runs() {
        let mut vars = with_placement("isolated", Some("coder-2"));
        vars.insert("WORKSPACE_ROOT", "/pvc");
        let w = worker(&vars);
        assert_eq!(w.placement, Placement::Isolated);
        assert!(w.placement.pins_runs());
        assert_eq!(w.worker_id.as_deref(), Some("coder-2"));
        assert_eq!(w.placed_root(), PathBuf::from("/pvc"));
    }

    #[test]
    fn a_pinning_placement_without_a_worker_id_names_the_variable() {
        for placement in ["affinity", "isolated"] {
            for id in [None, Some("   ")] {
                let err = parse(&with_placement(placement, id)).unwrap_err();
                let problem = err
                    .problems
                    .iter()
                    .find(|p| p.starts_with("WORKER_ID"))
                    .unwrap_or_else(|| panic!("{placement}: {:?}", err.problems));
                assert!(problem.contains("required"), "{problem}");
                assert!(problem.contains(placement), "{problem}");
                assert_eq!(err.class(), ErrorClass::Invalid);
            }
        }
    }

    #[test]
    fn a2a_only_is_refused_by_the_worker_roles() {
        for role in ["all", "worker"] {
            let mut vars = with_placement("a2a-only", Some("coder-0"));
            vars.insert("ROLE", role);
            let err = parse(&vars).unwrap_err();
            let problem = err
                .problems
                .iter()
                .find(|p| p.starts_with("WORKSPACE_PLACEMENT"))
                .unwrap_or_else(|| panic!("{role}: {:?}", err.problems));
            assert!(problem.contains("a2a-only"), "{problem}");
            assert!(problem.contains("workspace"), "{problem}");
            assert_eq!(err.class(), ErrorClass::Invalid);
        }
    }

    #[test]
    fn an_unknown_placement_names_the_variable_and_the_accepted_values() {
        let err = parse(&with_placement("pinned", None)).unwrap_err();
        let problem = err
            .problems
            .iter()
            .find(|p| p.starts_with("WORKSPACE_PLACEMENT"))
            .unwrap_or_else(|| panic!("{:?}", err.problems));
        assert!(problem.contains("\"pinned\""), "{problem}");
        for placement in Placement::VALUES {
            assert!(problem.contains(placement.as_str()), "{problem}");
        }
    }

    #[test]
    fn a_worker_id_is_a_safe_name() {
        // Leaked: the test maps hold `&'static str`.
        let (long, too_long): (&'static str, &'static str) = (
            Box::leak("x".repeat(128).into_boxed_str()),
            Box::leak("x".repeat(129).into_boxed_str()),
        );
        for good in ["a", "coder-0", "adam.coder_1", "A-b_C.9", long] {
            let w = worker(&with_placement("affinity", Some(good)));
            assert_eq!(w.worker_id.as_deref(), Some(good));
        }
        for bad in [
            ".hidden", "..", "a/b", "a\\b", "a b", "pod:0", "ünï", too_long,
        ] {
            let err = parse(&with_placement("affinity", Some(bad))).unwrap_err();
            assert!(
                err.problems.iter().any(|p| p.starts_with("WORKER_ID")),
                "{bad:?}: {:?}",
                err.problems
            );
        }
        // Checked for every placement, not only the ones that make a folder of it.
        let err = parse(&with_placement("shared", Some("a/b"))).unwrap_err();
        assert!(err.problems.iter().any(|p| p.starts_with("WORKER_ID")));
    }

    #[test]
    fn a_control_plane_does_not_read_the_placement_variables() {
        let mut vars = with_role("control-plane");
        vars.insert("WORKSPACE_PLACEMENT", "a2a-only");
        vars.insert("WORKER_ID", "a/b");
        let c = parse(&vars).expect("a control plane ignores them");
        assert!(c.worker.is_none());
    }

    #[test]
    fn placement_problems_come_with_the_others() {
        let mut vars = with_placement("isolated", None);
        vars.remove("GITHUB_TOKEN");
        let err = parse(&vars).unwrap_err();
        assert!(err.problems.iter().any(|p| p.starts_with("GITHUB_TOKEN")));
        assert!(err.problems.iter().any(|p| p.starts_with("WORKER_ID")));
    }

    #[test]
    fn the_placement_shows_in_debug_output() {
        let shown = format!("{:?}", worker(&with_placement("affinity", Some("coder-3"))));
        assert!(shown.contains("Affinity"), "{shown}");
        assert!(shown.contains("coder-3"), "{shown}");
    }

    /// `full()` plus `ADAM_AGENT_DIR=<dir>`, for `role`.
    fn with_agent_dir(role: &str, dir: &str) -> Result<Config, ConfigError> {
        let mut vars: HashMap<String, String> = full()
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        vars.insert("ROLE".into(), role.to_owned());
        vars.insert("ADAM_AGENT_DIR".into(), dir.to_owned());
        Config::from_lookup(|k| vars.get(k).cloned())
    }

    #[test]
    fn without_an_agent_dir_the_embedded_copy_is_used() {
        assert_eq!(parse(&full()).unwrap().agent_dir, None);
        // Blank counts as unset.
        assert_eq!(with_agent_dir("all", "  ").unwrap().agent_dir, None);
    }

    #[test]
    fn every_role_reads_the_agent_dir_and_it_must_be_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap();
        for role in Role::VALUES {
            let c = with_agent_dir(role.as_str(), path).expect("an existing directory");
            assert_eq!(c.agent_dir.as_deref(), Some(dir.path()), "{role}");
        }
        let missing = dir.path().join("nowhere");
        let file = dir.path().join("file");
        std::fs::write(&file, "x").unwrap();
        for role in Role::VALUES {
            for bad in [&missing, &file] {
                let err = with_agent_dir(role.as_str(), bad.to_str().unwrap()).unwrap_err();
                let problem = err
                    .problems
                    .iter()
                    .find(|p| p.starts_with("ADAM_AGENT_DIR"))
                    .unwrap_or_else(|| panic!("{role}: {:?}", err.problems));
                assert!(problem.contains("is not a directory"), "{problem}");
                assert!(problem.contains(bad.to_str().unwrap()), "{problem}");
            }
        }
    }

    #[test]
    fn the_agent_dir_problem_comes_with_the_others() {
        let mut vars = full();
        vars.remove("DATABASE_URL");
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nowhere").display().to_string();
        let err = Config::from_lookup(|k| match k {
            "ADAM_AGENT_DIR" => Some(missing.clone()),
            other => vars.get(other).map(|v| (*v).to_owned()),
        })
        .unwrap_err();
        assert!(err.problems.iter().any(|p| p.starts_with("DATABASE_URL")));
        assert!(err.problems.iter().any(|p| p.starts_with("ADAM_AGENT_DIR")));
    }

    #[test]
    fn the_agent_dir_shows_in_debug_output() {
        let dir = tempfile::tempdir().unwrap();
        let shown = format!(
            "{:?}",
            with_agent_dir("control-plane", dir.path().to_str().unwrap()).unwrap()
        );
        assert!(shown.contains("agent_dir"), "{shown}");
    }

    // ------------------------------------------------------------------ GitHub credentials

    use adam_workspace::testing::TestAppKey;

    /// `full()` without the token, as owned strings, plus `extra`.
    fn without_token(extra: &[(&str, String)]) -> HashMap<String, String> {
        let mut vars: HashMap<String, String> = full()
            .into_iter()
            .filter(|(k, _)| *k != "GITHUB_TOKEN")
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        vars.extend(extra.iter().map(|(k, v)| ((*k).to_owned(), v.clone())));
        vars
    }

    fn parse_owned(vars: &HashMap<String, String>) -> Result<Config, ConfigError> {
        Config::from_lookup(|k| vars.get(k).cloned())
    }

    /// The problems of `vars`, which must not be valid.
    fn problems_of(vars: &HashMap<String, String>) -> Vec<String> {
        parse_owned(vars)
            .expect_err("the variables are not valid")
            .problems
    }

    fn app_vars(key_var: (&str, String)) -> Vec<(&'static str, String)> {
        vec![
            ("GITHUB_APP_ID", "12345".to_owned()),
            ("GITHUB_APP_INSTALLATION_ID", "67890".to_owned()),
            (
                match key_var.0 {
                    "GITHUB_APP_PRIVATE_KEY" => "GITHUB_APP_PRIVATE_KEY",
                    _ => "GITHUB_APP_PRIVATE_KEY_PATH",
                },
                key_var.1,
            ),
        ]
    }

    #[test]
    fn a_personal_access_token_is_one_way_to_authenticate() {
        let c = parse(&full()).unwrap().worker.unwrap();
        let GitHubAuth::Token(token) = &c.github else {
            panic!("a token: {:?}", c.github)
        };
        assert_eq!(token.expose_secret(), "ghp_secret");
        assert!(!format!("{c:?}").contains("ghp_secret"), "Debug hides it");
        assert_eq!(c.github.secrets(), ["ghp_secret"]);
        assert!(c.github.check_hint().starts_with("GITHUB_TOKEN"));
    }

    #[test]
    fn a_github_app_is_the_other_way_with_a_key_from_a_file_or_from_the_variable() {
        let key = TestAppKey::generate();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("app.pem");
        std::fs::write(&file, &key.pkcs1_pem).unwrap();
        // A file, in GitHub's form; the variable in PKCS#8, with its newlines written as `\n`
        // (what a secret store or an env file does to a PEM); and the same with real newlines.
        let flattened = key.pkcs8_pem.trim().replace('\n', "\\n");
        for (label, key_var, pem) in [
            (
                "file",
                (
                    "GITHUB_APP_PRIVATE_KEY_PATH",
                    file.to_string_lossy().into_owned(),
                ),
                &key.pkcs1_pem,
            ),
            (
                "escaped",
                ("GITHUB_APP_PRIVATE_KEY", flattened),
                &key.pkcs8_pem,
            ),
            (
                "multi-line",
                ("GITHUB_APP_PRIVATE_KEY", key.pkcs8_pem.clone()),
                &key.pkcs8_pem,
            ),
        ] {
            let mut extra = app_vars(key_var);
            // An empty token is no token: a deployment that sets both to blank for the other way.
            extra.push(("GITHUB_TOKEN", "  ".to_owned()));
            let c = parse_owned(&without_token(&extra)).unwrap_or_else(|e| panic!("{label}: {e}"));
            let GitHubAuth::App(app) = &c.worker.as_ref().unwrap().github else {
                panic!("{label}: an App")
            };
            assert_eq!(app.app_id, "12345");
            assert_eq!(app.installation_id, 67890);
            assert_eq!(app.pem.expose_secret().trim(), pem.trim(), "{label}");
            // What the redactor must know: the PEM, and its body on one line.
            let secrets = c.worker.as_ref().unwrap().github.secrets();
            assert_eq!(secrets.len(), 2, "{label}");
            assert!(secrets[0].starts_with("-----BEGIN"), "{label}");
            assert!(
                !secrets[1].contains(['\n', '-']) && secrets[1].len() > 1000,
                "{label}"
            );
            // Nothing of the key, or of the App's secrets, in Debug output.
            let debug = format!("{c:?}");
            assert!(
                !debug.contains("BEGIN") && !debug.contains(&secrets[1][..40]),
                "{label}: {debug}"
            );
            assert!(debug.contains("installation_id: 67890"), "{debug}");
            assert!(
                c.worker
                    .unwrap()
                    .github
                    .check_hint()
                    .contains("GITHUB_APP_ID")
            );
        }
        // The App's id may be a client id.
        let mut extra = app_vars((
            "GITHUB_APP_PRIVATE_KEY_PATH",
            file.to_string_lossy().into_owned(),
        ));
        extra[0].1 = "Iv23liClientId".to_owned();
        let c = parse_owned(&without_token(&extra)).unwrap();
        let GitHubAuth::App(app) = &c.worker.unwrap().github else {
            panic!("an App")
        };
        assert_eq!(app.app_id, "Iv23liClientId");
    }

    #[test]
    fn exactly_one_way_every_problem_at_once_and_never_a_value() {
        let key = TestAppKey::generate();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("app.pem");
        std::fs::write(&file, &key.pkcs1_pem).unwrap();
        let file = file.to_string_lossy().into_owned();
        let full_app = || app_vars(("GITHUB_APP_PRIVATE_KEY_PATH", file.clone()));
        let said = |problems: &[String], name: &str| problems.iter().any(|p| p.starts_with(name));

        // Neither: both ways are named.
        let problems = problems_of(&without_token(&[]));
        assert!(said(&problems, "GITHUB_TOKEN is required"), "{problems:?}");
        assert!(problems[0].contains("GITHUB_APP_ID"), "{problems:?}");
        // Both, and a token with a part of an App: the same refusal.
        let mut both = full_app();
        both.push(("GITHUB_TOKEN", "ghp_secret".to_owned()));
        for extra in [
            both,
            vec![
                ("GITHUB_TOKEN", "ghp_secret".to_owned()),
                ("GITHUB_APP_ID", "1".to_owned()),
            ],
        ] {
            let problems = problems_of(&without_token(&extra));
            assert!(
                said(&problems, "GITHUB_TOKEN and the GITHUB_APP_*"),
                "{problems:?}"
            );
            assert!(
                problems.iter().all(|p| !p.contains("ghp_secret")),
                "{problems:?}"
            );
        }
        // A part of an App: everything that is missing, in one report, with the other problems.
        let mut vars = without_token(&[
            ("GITHUB_APP_ID", "1".to_owned()),
            ("GITHUB_API_URL", "garbage".to_owned()),
        ]);
        let problems = problems_of(&vars);
        for name in [
            "GITHUB_APP_INSTALLATION_ID is required",
            "GITHUB_APP_PRIVATE_KEY_PATH (a file)",
            "GITHUB_API_URL",
        ] {
            assert!(said(&problems, name), "{name}: {problems:?}");
        }
        vars.remove("GITHUB_APP_ID");
        vars.insert("GITHUB_APP_INSTALLATION_ID".into(), "7".into());
        let problems = problems_of(&vars);
        assert!(said(&problems, "GITHUB_APP_ID is required"), "{problems:?}");
        // The installation id is a positive integer.
        for bad in ["abc", "0", "-5", "1.5", "12 34"] {
            let mut extra = full_app();
            extra[1].1 = bad.to_owned();
            let problems = problems_of(&without_token(&extra));
            assert!(
                said(
                    &problems,
                    "GITHUB_APP_INSTALLATION_ID must be a positive integer"
                ),
                "{bad}: {problems:?}"
            );
        }
        // One key source.
        let mut extra = full_app();
        extra.push(("GITHUB_APP_PRIVATE_KEY", key.pkcs8_pem.clone()));
        let problems = problems_of(&without_token(&extra));
        assert!(
            said(
                &problems,
                "GITHUB_APP_PRIVATE_KEY_PATH and GITHUB_APP_PRIVATE_KEY are both set"
            ),
            "{problems:?}"
        );
        assert!(
            problems.iter().all(|p| !p.contains("BEGIN")),
            "{problems:?}"
        );
    }

    #[test]
    fn a_key_that_cannot_be_used_is_refused_at_startup_naming_the_variable_and_not_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, text: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, text).unwrap();
            path.to_string_lossy().into_owned()
        };
        let ec = "-----BEGIN EC PRIVATE KEY-----\nTOPSECRETBODYofAnECkey\n-----END EC PRIVATE KEY-----\n";
        let junk = "TOPSECRETnotAPemAtAll";
        let truncated = "-----BEGIN RSA PRIVATE KEY-----\nTOPSECRETtruncatedBody\n";
        let cases = [
            (
                "a missing file",
                (
                    "GITHUB_APP_PRIVATE_KEY_PATH",
                    dir.path().join("nope.pem").to_string_lossy().into_owned(),
                ),
                "GITHUB_APP_PRIVATE_KEY_PATH",
            ),
            (
                "a directory",
                (
                    "GITHUB_APP_PRIVATE_KEY_PATH",
                    dir.path().to_string_lossy().into_owned(),
                ),
                "GITHUB_APP_PRIVATE_KEY_PATH",
            ),
            (
                "an EC key",
                ("GITHUB_APP_PRIVATE_KEY_PATH", write("ec.pem", ec)),
                "GITHUB_APP_PRIVATE_KEY_PATH: the GitHub App's private key is not usable",
            ),
            (
                "junk in a file",
                ("GITHUB_APP_PRIVATE_KEY_PATH", write("junk.pem", junk)),
                "GITHUB_APP_PRIVATE_KEY_PATH: the GitHub App's private key is not usable",
            ),
            (
                "a truncated key",
                ("GITHUB_APP_PRIVATE_KEY_PATH", write("cut.pem", truncated)),
                "GITHUB_APP_PRIVATE_KEY_PATH: the GitHub App's private key is not usable",
            ),
            (
                "junk in the variable",
                ("GITHUB_APP_PRIVATE_KEY", junk.to_owned()),
                "GITHUB_APP_PRIVATE_KEY: the GitHub App's private key is not usable",
            ),
            (
                "an EC key in the variable",
                ("GITHUB_APP_PRIVATE_KEY", ec.replace('\n', "\\n")),
                "GITHUB_APP_PRIVATE_KEY: the GitHub App's private key is not usable",
            ),
        ];
        for (what, key_var, expected) in cases {
            let problems = problems_of(&without_token(&app_vars(key_var)));
            assert!(
                problems.iter().any(|p| p.starts_with(expected)),
                "{what}: {problems:?}"
            );
            let all = problems.join("\n");
            assert!(
                !all.contains("TOPSECRET"),
                "{what}: the key is not in the message: {all}"
            );
        }
    }

    #[test]
    fn a_control_plane_reads_no_github_variables_not_even_bad_ones() {
        let mut vars = without_token(&[
            ("ROLE", "control-plane".to_owned()),
            ("GITHUB_APP_ID", "1".to_owned()),
            ("GITHUB_APP_PRIVATE_KEY", "junk".to_owned()),
            ("GITHUB_TOKEN", "x".to_owned()),
        ]);
        assert!(parse_owned(&vars).is_ok());
        vars.insert("ROLE".into(), "worker".into());
        assert!(parse_owned(&vars).is_err(), "a worker does");
    }
}
