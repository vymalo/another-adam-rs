//! The configuration every agent binary shares, read from the environment.
//!
//! | Variable | Meaning | Default |
//! |---|---|---|
//! | `ROLE` | what this process runs: `all`, `control-plane` or `worker` ([`adam_host::Role`]) | `all` |
//! | `DATABASE_URL` | Postgres for the run store (`adam-store-postgres`) | required |
//! | `A2A_BEARER_TOKENS` | comma-separated tokens accepted by the A2A server | required for `all` and `control-plane`, non-empty (fail closed) |
//! | `PUBLIC_URL` | URL clients reach the JSON-RPC endpoint at (agent card) | required for `all` and `control-plane` |
//! | `LISTEN_ADDR` | bind address: the A2A server, or for `worker` its `/healthz` listener | `0.0.0.0:8080` |
//! | `WORKERS` | runs advanced concurrently by this process | `4` |
//! | `WORKER_ID` | stable identity of this worker (the lease identity; the run owner for a binary that pins runs); letters, digits, `.`, `_`, `-` | random per process |
//! | `MODEL_BASE_URL` | OpenAI-compatible gateway, with its `/v1` prefix ([`ModelConfig`]) | required for `all` and `worker` |
//! | `MODEL_API_KEY` | bearer token for it (may be empty for local servers) | required for `all` and `worker` |
//! | `MODEL` | model alias of the agent | required for `all` and `worker` |
//! | `MCP_ALLOW_STDIO`, `MCP_ALLOW_INSECURE`, `MCP_ALLOW_URL_VARS` | what the MCP servers of an agent folder may be ([`McpSettings`], feature `mcp`) | `false` each |
//! | `THREAD_TOOLS_MAX_CALL_SECS` | the longest a call to a tool of the thread's tools endpoint is waited for, whatever time the tool says it may take (1 to 86400; [`McpSettings`], feature `mcp`) | `3600` |
//!
//! A binary reads what it needs with the `parse` functions of this module, each of which adds
//! one line to a list of problems for every missing or malformed variable instead of stopping at
//! the first, so a misconfigured deployment is fixed in one round trip: it parses its own
//! variables with the same list, and only then turns a non-empty list into a [`ConfigError`].
//!
//! # Roles
//!
//! `ROLE` picks the halves a process runs, and each half brings its own variables:
//!
//! * **Every role** needs `DATABASE_URL`, and reads `LISTEN_ADDR`.
//! * **The control plane** (`all`, `control-plane`) also needs `A2A_BEARER_TOKENS` and
//!   `PUBLIC_URL`: it serves A2A and starts, delivers to, cancels and views runs, which needs only
//!   the agent's name and its `init`, so it holds no model configuration.
//! * **The workers** (`all`, `worker`) read `WORKERS` and `WORKER_ID` ([`ServiceConfig::worker`],
//!   a [`WorkerSettings`], `Some` exactly when [`Role::runs_workers`]) and, in a binary that
//!   runs an LLM agent, the `MODEL*` variables and the `MCP_ALLOW_*` flags.
//!
//! What a role does not use is not validated: a chart may set a variable for every role, and a
//! malformed `WORKERS` does not stop a control plane.
//!
//! Secrets are wrapped in [`SecretString`] and never appear in `Debug` output.

use std::net::SocketAddr;
use std::str::FromStr;

use adam_error::{Classify, ErrorClass};
use adam_host::Role;
use adam_model::DynModel;
use adam_model_openai::{OpenAiCompatible, OpenAiConfig, OpenAiConfigError};
use secrecy::SecretString;
use url::Url;

use crate::service::RuntimeOptions;

/// One or more environment variables are missing or unusable.
///
/// Classified as [`ErrorClass::Invalid`]: the same environment never works. It lists every
/// problem at once and never a secret's value.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
#[error("invalid configuration:\n  - {}", problems.join("\n  - "))]
pub struct ConfigError {
    /// One line per problem.
    pub problems: Vec<String>,
}

impl ConfigError {
    /// An error listing `problems`.
    pub fn new(problems: Vec<String>) -> Self {
        Self { problems }
    }

    /// `Ok(())` when `problems` is empty, otherwise the error that lists them all.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] when `problems` is not empty.
    pub fn check(problems: Vec<String>) -> Result<(), Self> {
        if problems.is_empty() {
            Ok(())
        } else {
            Err(Self::new(problems))
        }
    }
}

impl Classify for ConfigError {
    fn class(&self) -> ErrorClass {
        ErrorClass::Invalid
    }
}

/// `lookup(name)` with a blank value counting as unset (every variable but `MODEL_API_KEY`, which
/// may be empty on purpose).
fn not_blank(lookup: &impl Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    lookup(name).filter(|v| !v.trim().is_empty())
}

/// The value of the variable `name` parsed as a `T`, or `default` when it is unset (or blank).
/// A value that does not parse adds a problem and also gives `default`.
pub fn parse_or<T: FromStr>(
    get: &impl Fn(&str) -> Option<String>,
    name: &str,
    default: T,
    problems: &mut Vec<String>,
) -> T
where
    T::Err: std::fmt::Display,
{
    match get(name) {
        None => default,
        Some(raw) => raw.trim().parse().unwrap_or_else(|e| {
            problems.push(format!("{name} is invalid ({raw:?}): {e}"));
            default
        }),
    }
}

/// A boolean variable: `true`/`1` or `false`/`0` (ASCII case ignored), `false` when it is unset.
/// Anything else adds a problem naming the variable.
pub fn parse_flag(
    get: &impl Fn(&str) -> Option<String>,
    name: &str,
    problems: &mut Vec<String>,
) -> bool {
    match get(name).as_deref() {
        None => false,
        Some(v) if v.eq_ignore_ascii_case("true") || v == "1" => true,
        Some(v) if v.eq_ignore_ascii_case("false") || v == "0" => false,
        Some(v) => {
            problems.push(format!("{name} must be true or false, got {v:?}"));
            false
        }
    }
}

/// A worker id is a lease identity and, for a binary that gives each worker a folder, a folder
/// name: no separator, no `..`.
pub fn is_worker_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// What the process is, whatever agent it serves: its role, its database, how it is reached and
/// how many runs it advances at once.
#[derive(Clone)]
pub struct ServiceConfig {
    /// `ROLE`: which halves this process runs.
    pub role: Role,
    /// `DATABASE_URL`.
    pub database_url: SecretString,
    /// `A2A_BEARER_TOKENS`. Empty unless [`Role::runs_control_plane`].
    pub a2a_bearer_tokens: Vec<SecretString>,
    /// `PUBLIC_URL`. `Some` exactly when [`Role::runs_control_plane`].
    pub public_url: Option<Url>,
    /// `LISTEN_ADDR`: the A2A server, or the `/healthz` listener of a worker.
    pub listen_addr: SocketAddr,
    /// `WORKERS` and `WORKER_ID`. `Some` exactly when [`Role::runs_workers`].
    pub worker: Option<WorkerSettings>,
}

impl std::fmt::Debug for ServiceConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceConfig")
            .field("role", &self.role)
            .field("database_url", &"[REDACTED]")
            .field("a2a_bearer_tokens", &self.a2a_bearer_tokens.len())
            .field("public_url", &self.public_url.as_ref().map(Url::as_str))
            .field("listen_addr", &self.listen_addr)
            .field("worker", &self.worker)
            .finish()
    }
}

impl ServiceConfig {
    /// Read the variables of the table in the [module docs](self) that [`ServiceConfig`] holds
    /// (`ROLE`, `DATABASE_URL`, `A2A_BEARER_TOKENS`, `PUBLIC_URL`, `LISTEN_ADDR` and, for a role
    /// that runs workers, `WORKERS` and `WORKER_ID`) through `lookup` (`None` = unset). A blank
    /// value counts as unset.
    ///
    /// Every missing or malformed variable adds a line to `problems`; the returned value then
    /// holds placeholders for it and must not be used. A role does not read the variables of the
    /// halves it does not run.
    pub fn parse(lookup: &impl Fn(&str) -> Option<String>, problems: &mut Vec<String>) -> Self {
        let get = |name: &str| not_blank(lookup, name);
        let role = match Role::from_optional(lookup("ROLE").as_deref()) {
            Ok(role) => role,
            Err(e) => {
                problems.push(format!("ROLE is invalid: {e}"));
                Role::default()
            }
        };

        let mut required = |name: &str| {
            let value = get(name);
            if value.is_none() {
                problems.push(format!("{name} is required"));
            }
            value.unwrap_or_default()
        };
        let database_url = required("DATABASE_URL");
        // The front's variables: only a role that serves A2A needs them.
        let front = role.runs_control_plane();
        let public_url_raw = if front {
            required("PUBLIC_URL")
        } else {
            String::new()
        };
        let tokens_raw = if front {
            required("A2A_BEARER_TOKENS")
        } else {
            String::new()
        };

        let public_url = if front {
            match Url::parse(&public_url_raw) {
                Ok(u) if matches!(u.scheme(), "http" | "https") => Some(u),
                Ok(_) => {
                    problems.push("PUBLIC_URL must be an http(s) URL".into());
                    None
                }
                Err(e) if !public_url_raw.is_empty() => {
                    problems.push(format!("PUBLIC_URL is not a URL: {e}"));
                    None
                }
                Err(_) => None,
            }
        } else {
            None
        };

        let a2a_bearer_tokens: Vec<SecretString> = tokens_raw
            .split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(|t| SecretString::from(t.to_owned()))
            .collect();
        if a2a_bearer_tokens.is_empty() && !tokens_raw.is_empty() {
            problems.push("A2A_BEARER_TOKENS has no usable token".into());
        }

        let listen_addr = parse_or(
            &get,
            "LISTEN_ADDR",
            SocketAddr::from(([0, 0, 0, 0], 8080)),
            problems,
        );

        let worker = role
            .runs_workers()
            .then(|| WorkerSettings::parse(lookup, problems));

        Self {
            role,
            database_url: SecretString::from(database_url),
            a2a_bearer_tokens,
            public_url,
            listen_addr,
            worker,
        }
    }
}

/// `WORKERS` and `WORKER_ID`: how a worker process takes runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerSettings {
    /// `WORKERS`: runs advanced concurrently.
    pub workers: usize,
    /// `WORKER_ID`: the lease identity of this worker. `None`: a random one per process.
    pub worker_id: Option<String>,
}

impl WorkerSettings {
    /// Read `WORKERS` (at least 1) and `WORKER_ID` (see [`is_worker_id`]), adding a problem for
    /// each one that is unusable.
    pub fn parse(lookup: &impl Fn(&str) -> Option<String>, problems: &mut Vec<String>) -> Self {
        let get = |name: &str| not_blank(lookup, name);
        let workers = parse_or(&get, "WORKERS", 4usize, problems);
        if workers == 0 {
            problems.push("WORKERS must be at least 1".into());
        }
        let worker_id = get("WORKER_ID").map(|id| id.trim().to_owned());
        if let Some(id) = &worker_id
            && !is_worker_id(id)
        {
            problems.push(format!(
                "WORKER_ID {id:?} is not usable: 1 to 128 letters, digits, `.`, `_` or `-`, not starting with `.`"
            ));
        }
        Self { workers, worker_id }
    }

    /// The runtime options these settings give: this many runs at once, under this id. The claim
    /// scope is any run's, as for a worker that keeps no files of a run; a binary that pins runs
    /// sets [`RuntimeOptions::claim_scope`] itself.
    pub fn options(&self) -> RuntimeOptions {
        RuntimeOptions {
            worker_id: self.worker_id.clone(),
            concurrency: self.workers,
            ..RuntimeOptions::default()
        }
    }
}

/// The model of an LLM agent: an OpenAI-compatible gateway, its key and the alias of the model
/// (`MODEL_BASE_URL`, `MODEL_API_KEY`, `MODEL`). Read by the roles that run workers.
#[derive(Clone)]
pub struct ModelConfig {
    /// `MODEL_BASE_URL`: the gateway, with its `/v1` prefix.
    pub base_url: String,
    /// `MODEL_API_KEY`: may be empty, for a gateway without auth.
    pub api_key: SecretString,
    /// `MODEL`: the model alias the gateway knows.
    pub alias: String,
}

impl std::fmt::Debug for ModelConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelConfig")
            .field("base_url", &self.base_url)
            .field("alias", &self.alias)
            .finish_non_exhaustive()
    }
}

impl ModelConfig {
    /// Read `MODEL_BASE_URL`, `MODEL_API_KEY` (which may be set empty, but not unset) and
    /// `MODEL`, adding a problem for each one that is missing.
    pub fn parse(lookup: &impl Fn(&str) -> Option<String>, problems: &mut Vec<String>) -> Self {
        let get = |name: &str| not_blank(lookup, name);
        let mut required = |name: &str| {
            let value = get(name);
            if value.is_none() {
                problems.push(format!("{name} is required"));
            }
            value.unwrap_or_default()
        };
        let base_url = required("MODEL_BASE_URL");
        let alias = required("MODEL");
        let api_key = match lookup("MODEL_API_KEY") {
            Some(v) => v,
            None => {
                problems.push(
                    "MODEL_API_KEY is required (set it empty for a gateway without auth)".into(),
                );
                String::new()
            }
        };
        Self {
            base_url,
            api_key: SecretString::from(api_key),
            alias,
        }
    }

    /// The client for the gateway. Nothing is sent until a worker steps a run.
    ///
    /// # Errors
    ///
    /// [`OpenAiConfigError`] for a `MODEL_BASE_URL` that is not an absolute http(s) URL or a key
    /// that cannot be a header value.
    pub fn client(&self) -> Result<DynModel, OpenAiConfigError> {
        Ok(std::sync::Arc::new(OpenAiCompatible::new(
            OpenAiConfig::new(self.base_url.clone(), self.api_key.clone()),
        )?))
    }
}

/// The default of `THREAD_TOOLS_MAX_CALL_SECS`: one hour.
#[cfg(feature = "mcp")]
pub const DEFAULT_THREAD_TOOLS_MAX_CALL_SECS: u64 = 3600;

/// The most `THREAD_TOOLS_MAX_CALL_SECS` may say: a day.
#[cfg(feature = "mcp")]
pub const MAX_THREAD_TOOLS_MAX_CALL_SECS: u64 = 86_400;

/// What the deployment lets an agent folder's `mcp.json` do (`MCP_ALLOW_STDIO`,
/// `MCP_ALLOW_INSECURE`, `MCP_ALLOW_URL_VARS`), and how long it waits for the thread's tools
/// (`THREAD_TOOLS_MAX_CALL_SECS`). The files say which servers an agent uses; these say which kinds
/// may be used. Every flag is off by default, as in [`McpPolicy`](adam_mcp::McpPolicy).
#[cfg(feature = "mcp")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McpSettings {
    /// `MCP_ALLOW_STDIO`: a server may be a local process (`command` in `mcp.json`). The file
    /// would decide what this process runs, with this process's rights.
    pub allow_stdio: bool,
    /// `MCP_ALLOW_INSECURE`: a server may be at a plain `http` URL that is not this machine.
    /// Development only: the requests and their headers cross the network in the clear.
    pub allow_insecure: bool,
    /// `MCP_ALLOW_URL_VARS`: a server's `url` may contain `${VAR}`. Off by default because the
    /// MCP client library logs the URL it dials; a credential belongs in `headers`, which are
    /// never logged, and `${VAR}` in a header works without this flag.
    pub allow_url_vars: bool,
    /// `THREAD_TOOLS_MAX_CALL_SECS`: the longest, in seconds, that a call to a tool of the thread's
    /// tools endpoint is waited for. A tool of the orchestration layer says how long it may take
    /// (`_meta["thread-tools/v1"].timeoutSecs`: 125 s for a relayed search, 30 minutes for an
    /// asked agent); the agent waits that long, **capped by this**. 1 to 86400, default 3600.
    pub thread_tools_max_call_secs: u64,
}

#[cfg(feature = "mcp")]
impl Default for McpSettings {
    fn default() -> Self {
        Self {
            allow_stdio: false,
            allow_insecure: false,
            allow_url_vars: false,
            thread_tools_max_call_secs: DEFAULT_THREAD_TOOLS_MAX_CALL_SECS,
        }
    }
}

#[cfg(feature = "mcp")]
impl McpSettings {
    /// Read the three flags (`true`/`1`, `false`/`0`), adding a problem for a value that is
    /// neither, and the cap on a call to the thread's tools (a whole number of seconds from 1 to
    /// 86400, else a problem and the default).
    pub fn parse(lookup: &impl Fn(&str) -> Option<String>, problems: &mut Vec<String>) -> Self {
        let get = |name: &str| not_blank(lookup, name);
        Self {
            allow_stdio: parse_flag(&get, "MCP_ALLOW_STDIO", problems),
            allow_insecure: parse_flag(&get, "MCP_ALLOW_INSECURE", problems),
            allow_url_vars: parse_flag(&get, "MCP_ALLOW_URL_VARS", problems),
            thread_tools_max_call_secs: parse_max_call_secs(&get, problems),
        }
    }

    /// The policy `AgentDef::connect_mcp` is given.
    pub fn policy(&self) -> adam_mcp::McpPolicy {
        adam_mcp::McpPolicy::default()
            .allow_stdio(self.allow_stdio)
            .allow_insecure(self.allow_insecure)
            .allow_url_secrets(self.allow_url_vars)
            .thread_tools_max_call(std::time::Duration::from_secs(
                self.thread_tools_max_call_secs,
            ))
    }
}

/// `THREAD_TOOLS_MAX_CALL_SECS`: 1 to 86400, [`DEFAULT_THREAD_TOOLS_MAX_CALL_SECS`] when unset or
/// blank; anything else is a problem and the default.
#[cfg(feature = "mcp")]
fn parse_max_call_secs(get: &impl Fn(&str) -> Option<String>, problems: &mut Vec<String>) -> u64 {
    const NAME: &str = "THREAD_TOOLS_MAX_CALL_SECS";
    let secs = parse_or(get, NAME, DEFAULT_THREAD_TOOLS_MAX_CALL_SECS, problems);
    if (1..=MAX_THREAD_TOOLS_MAX_CALL_SECS).contains(&secs) {
        return secs;
    }
    problems.push(format!(
        "{NAME} must be between 1 and {MAX_THREAD_TOOLS_MAX_CALL_SECS} seconds, got {secs}"
    ));
    DEFAULT_THREAD_TOOLS_MAX_CALL_SECS
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use secrecy::ExposeSecret;

    use super::*;

    type Vars = HashMap<&'static str, &'static str>;

    fn full() -> Vars {
        HashMap::from([
            ("DATABASE_URL", "postgres://u:hunter2@db/adam"),
            ("A2A_BEARER_TOKENS", "one, two ,,"),
            ("PUBLIC_URL", "http://agent.svc:8080/"),
        ])
    }

    fn with_role(role: &'static str) -> Vars {
        let mut vars = full();
        vars.insert("ROLE", role);
        vars
    }

    fn lookup(vars: &Vars) -> impl Fn(&str) -> Option<String> + '_ {
        |k| vars.get(k).map(|v| (*v).to_owned())
    }

    fn parse(vars: &Vars) -> Result<ServiceConfig, ConfigError> {
        let mut problems = Vec::new();
        let config = ServiceConfig::parse(&lookup(vars), &mut problems);
        ConfigError::check(problems).map(|()| config)
    }

    fn problems_of(vars: &Vars) -> Vec<String> {
        parse(vars).unwrap_err().problems
    }

    fn mentions(problems: &[String], name: &str) -> bool {
        problems.iter().any(|p| p.starts_with(name))
    }

    #[test]
    fn defaults_apply_and_tokens_are_split() {
        let c = parse(&full()).expect("valid");
        assert_eq!(c.role, Role::All);
        assert_eq!(c.listen_addr, "0.0.0.0:8080".parse().unwrap());
        let tokens: Vec<_> = c
            .a2a_bearer_tokens
            .iter()
            .map(|t| t.expose_secret().to_owned())
            .collect();
        assert_eq!(tokens, ["one", "two"]);
        assert_eq!(c.public_url.unwrap().as_str(), "http://agent.svc:8080/");
        assert_eq!(
            c.worker,
            Some(WorkerSettings {
                workers: 4,
                worker_id: None
            })
        );
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let problems = problems_of(&HashMap::new());
        for name in ["DATABASE_URL", "PUBLIC_URL", "A2A_BEARER_TOKENS"] {
            assert!(
                mentions(&problems, name),
                "{name} missing from {problems:?}"
            );
        }
        let mut vars = full();
        vars.insert("LISTEN_ADDR", "everywhere");
        vars.insert("WORKERS", "0");
        vars.insert("WORKER_ID", "a/b");
        vars.insert("PUBLIC_URL", "ftp://x");
        let problems = problems_of(&vars);
        for name in ["LISTEN_ADDR", "WORKERS", "WORKER_ID", "PUBLIC_URL"] {
            assert!(
                mentions(&problems, name),
                "{name} missing from {problems:?}"
            );
        }
    }

    #[test]
    fn the_role_defaults_to_all_and_each_value_parses() {
        // Blank counts as unset, like every other variable.
        assert_eq!(parse(&with_role("  ")).unwrap().role, Role::All);
        for role in Role::VALUES {
            assert_eq!(parse(&with_role(role.as_str())).unwrap().role, role);
        }
        // `adam_host` trims and ignores ASCII case.
        assert_eq!(
            parse(&with_role(" Control-Plane ")).unwrap().role,
            Role::ControlPlane
        );
    }

    #[test]
    fn an_unknown_role_names_the_variable_and_the_accepted_values() {
        for bad in ["boss", "controlplane", "workers"] {
            let problems = problems_of(&with_role(bad));
            let problem = problems
                .iter()
                .find(|p| p.starts_with("ROLE"))
                .unwrap_or_else(|| panic!("{bad:?} accepted or misreported: {problems:?}"));
            assert!(problem.contains(&format!("{bad:?}")), "{problem}");
            for accepted in ["all", "control-plane", "worker"] {
                assert!(problem.contains(accepted), "{problem}");
            }
        }
    }

    #[test]
    fn the_roles_that_serve_a2a_need_the_front_variables_and_fail_closed() {
        for role in ["all", "control-plane"] {
            let mut vars = with_role(role);
            vars.remove("A2A_BEARER_TOKENS");
            vars.remove("PUBLIC_URL");
            let problems = problems_of(&vars);
            for name in ["A2A_BEARER_TOKENS", "PUBLIC_URL"] {
                assert!(mentions(&problems, name), "{role}: {problems:?}");
            }
            // A blank list is no list.
            let mut vars = with_role(role);
            vars.insert("A2A_BEARER_TOKENS", " , ");
            assert!(mentions(&problems_of(&vars), "A2A_BEARER_TOKENS"), "{role}");
        }
    }

    #[test]
    fn a_worker_needs_no_front_variables_and_a_control_plane_no_worker_ones() {
        let mut vars = with_role("worker");
        vars.remove("A2A_BEARER_TOKENS");
        vars.remove("PUBLIC_URL");
        let c = parse(&vars).expect("a worker serves no A2A");
        assert!(c.a2a_bearer_tokens.is_empty() && c.public_url.is_none());
        // What a role does not use is not validated: a chart may set it for all roles.
        vars.insert("PUBLIC_URL", "ftp://not-used");
        vars.insert("A2A_BEARER_TOKENS", " , ");
        assert!(parse(&vars).is_ok());

        let mut vars = with_role("control-plane");
        vars.insert("WORKERS", "0");
        vars.insert("WORKER_ID", "a/b");
        let c = parse(&vars).expect("a control plane steps no run");
        assert!(c.worker.is_none());
    }

    #[test]
    fn the_worker_settings_are_some_exactly_when_the_role_runs_workers() {
        for role in Role::VALUES {
            let c = parse(&with_role(role.as_str())).unwrap();
            assert_eq!(c.worker.is_some(), role.runs_workers(), "{role}");
        }
    }

    #[test]
    fn workers_and_the_worker_id_are_checked() {
        let mut vars = full();
        vars.insert("WORKERS", "8");
        vars.insert("WORKER_ID", " agent-0 ");
        let w = parse(&vars).unwrap().worker.unwrap();
        assert_eq!(w.workers, 8);
        assert_eq!(w.worker_id.as_deref(), Some("agent-0"));
        let options = w.options();
        assert_eq!(options.concurrency, 8);
        assert_eq!(options.worker_id.as_deref(), Some("agent-0"));

        let long = "x".repeat(128);
        assert!(is_worker_id(&long));
        for good in ["a", "coder-0", "adam.coder_1", "A-b_C.9"] {
            assert!(is_worker_id(good), "{good}");
        }
        for bad in [
            ".hidden",
            "..",
            "a/b",
            "a b",
            "pod:0",
            "ünï",
            "",
            &"x".repeat(129),
        ] {
            assert!(!is_worker_id(bad), "{bad:?}");
        }
        for (name, bad) in [("WORKERS", "0"), ("WORKERS", "many"), ("WORKER_ID", "a/b")] {
            let mut vars = full();
            vars.insert(name, bad);
            assert!(mentions(&problems_of(&vars), name), "{name}={bad}");
        }
    }

    #[test]
    fn debug_output_hides_secrets() {
        let c = parse(&full()).unwrap();
        let text = format!("{c:?}");
        // The short tokens are checked in their quoted form: a bare "one" is inside "None".
        for secret in ["hunter2", "\"one\"", "\"two\""] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
    }

    #[test]
    fn a_config_error_lists_every_problem_and_is_invalid() {
        let e = ConfigError::new(vec!["A is required".into(), "B is invalid".into()]);
        assert_eq!(
            e.to_string(),
            "invalid configuration:\n  - A is required\n  - B is invalid"
        );
        assert_eq!(e.class(), ErrorClass::Invalid);
        assert!(ConfigError::check(Vec::new()).is_ok());
        assert!(ConfigError::check(vec!["x".into()]).is_err());
    }

    #[test]
    fn parse_or_and_parse_flag_collect_problems_and_fall_back() {
        let vars: Vars = HashMap::from([
            ("N", " 7 "),
            ("BAD", "seven"),
            ("YES", "TRUE"),
            ("ONE", "1"),
            ("NO", "false"),
            ("ZERO", "0"),
            ("MAYBE", "maybe"),
        ]);
        let get = |name: &str| vars.get(name).map(|v| (*v).to_owned());
        let mut problems = Vec::new();
        assert_eq!(parse_or(&get, "N", 1u32, &mut problems), 7);
        assert_eq!(parse_or(&get, "UNSET", 1u32, &mut problems), 1);
        assert!(problems.is_empty());
        assert_eq!(parse_or(&get, "BAD", 3u32, &mut problems), 3);
        assert!(mentions(&problems, "BAD"), "{problems:?}");

        let mut problems = Vec::new();
        assert!(parse_flag(&get, "YES", &mut problems));
        assert!(parse_flag(&get, "ONE", &mut problems));
        assert!(!parse_flag(&get, "NO", &mut problems));
        assert!(!parse_flag(&get, "ZERO", &mut problems));
        assert!(!parse_flag(&get, "UNSET", &mut problems));
        assert!(problems.is_empty());
        assert!(!parse_flag(&get, "MAYBE", &mut problems));
        assert!(
            problems[0].starts_with("MAYBE must be true or false"),
            "{problems:?}"
        );
    }

    fn model_problems(vars: &Vars) -> (ModelConfig, Vec<String>) {
        let mut problems = Vec::new();
        let model = ModelConfig::parse(&lookup(vars), &mut problems);
        (model, problems)
    }

    #[test]
    fn the_model_needs_a_url_an_alias_and_a_key_that_may_be_empty() {
        let mut vars: Vars = HashMap::from([
            ("MODEL_BASE_URL", "https://gw.example/v1"),
            ("MODEL_API_KEY", "sk-secret"),
            ("MODEL", "large"),
        ]);
        let (model, problems) = model_problems(&vars);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(model.base_url, "https://gw.example/v1");
        assert_eq!(model.alias, "large");
        assert!(!format!("{model:?}").contains("sk-secret"));
        model.client().expect("a usable client");

        // An empty key is allowed, an unset one is not.
        vars.insert("MODEL_API_KEY", "");
        assert!(model_problems(&vars).1.is_empty());
        vars.remove("MODEL_API_KEY");
        assert!(mentions(&model_problems(&vars).1, "MODEL_API_KEY"));
        let (_, problems) = model_problems(&HashMap::new());
        for name in ["MODEL_BASE_URL", "MODEL_API_KEY", "MODEL"] {
            assert!(
                mentions(&problems, name),
                "{name} missing from {problems:?}"
            );
        }
    }

    #[test]
    fn a_gateway_that_is_not_a_url_is_a_client_error_with_the_invalid_class() {
        let vars: Vars = HashMap::from([
            ("MODEL_BASE_URL", "not a url"),
            ("MODEL_API_KEY", ""),
            ("MODEL", "large"),
        ]);
        let (model, problems) = model_problems(&vars);
        assert!(
            problems.is_empty(),
            "the URL is checked when the client is built"
        );
        let error = model.client().err().expect("not a URL");
        assert_eq!(error.class(), ErrorClass::Invalid);
    }

    #[cfg(feature = "mcp")]
    mod mcp {
        use super::*;

        fn settings(vars: &Vars) -> (McpSettings, Vec<String>) {
            let mut problems = Vec::new();
            let settings = McpSettings::parse(&lookup(vars), &mut problems);
            (settings, problems)
        }

        #[test]
        fn the_flags_are_off_by_default_and_each_one_is_read() {
            let (s, problems) = settings(&HashMap::new());
            assert!(problems.is_empty());
            assert_eq!(s, McpSettings::default());
            let policy = s.policy();
            assert!(!policy.stdio_allowed());
            assert!(!policy.insecure_allowed());
            assert!(!policy.url_secrets_allowed());

            for (name, expected) in [
                ("MCP_ALLOW_STDIO", (true, false, false)),
                ("MCP_ALLOW_INSECURE", (false, true, false)),
                ("MCP_ALLOW_URL_VARS", (false, false, true)),
            ] {
                let (s, problems) = settings(&HashMap::from([(name, "true")]));
                assert!(problems.is_empty());
                assert_eq!(
                    (s.allow_stdio, s.allow_insecure, s.allow_url_vars),
                    expected
                );
                assert_eq!(s.thread_tools_max_call_secs, 3600);
                let policy = s.policy();
                assert_eq!(
                    (
                        policy.stdio_allowed(),
                        policy.insecure_allowed(),
                        policy.url_secrets_allowed()
                    ),
                    expected
                );
            }
        }

        #[test]
        fn the_cap_on_a_thread_tools_call_is_an_hour_and_can_be_set() {
            let (s, problems) = settings(&HashMap::new());
            assert!(problems.is_empty());
            assert_eq!(s.thread_tools_max_call_secs, 3600);
            assert_eq!(
                s.policy().thread_tools_max_call_value(),
                std::time::Duration::from_secs(3600)
            );
            // Blank is unset; a number is read (spaces trimmed), at both ends of the range.
            for (value, secs) in [("", 3600), (" 900 ", 900), ("1", 1), ("86400", 86_400)] {
                let (s, problems) =
                    settings(&HashMap::from([("THREAD_TOOLS_MAX_CALL_SECS", value)]));
                assert!(problems.is_empty(), "{value}: {problems:?}");
                assert_eq!(s.thread_tools_max_call_secs, secs, "{value}");
                assert_eq!(
                    s.policy().thread_tools_max_call_value(),
                    std::time::Duration::from_secs(secs)
                );
            }
        }

        #[test]
        fn a_bad_cap_names_the_variable_and_falls_back_to_the_default() {
            for bad in ["0", "86401", "-5", "an hour", "1.5"] {
                let (s, problems) = settings(&HashMap::from([("THREAD_TOOLS_MAX_CALL_SECS", bad)]));
                assert!(
                    problems
                        .iter()
                        .any(|p| p.starts_with("THREAD_TOOLS_MAX_CALL_SECS")),
                    "{bad}: {problems:?}"
                );
                assert_eq!(s.thread_tools_max_call_secs, 3600, "{bad}");
            }
        }

        #[test]
        fn a_bad_flag_names_the_variable() {
            let vars: Vars = HashMap::from([
                ("MCP_ALLOW_STDIO", "sometimes"),
                ("MCP_ALLOW_INSECURE", "yes"),
                ("MCP_ALLOW_URL_VARS", "2"),
            ]);
            let (_, problems) = settings(&vars);
            for name in [
                "MCP_ALLOW_STDIO",
                "MCP_ALLOW_INSECURE",
                "MCP_ALLOW_URL_VARS",
            ] {
                assert!(
                    problems
                        .iter()
                        .any(|p| p.starts_with(name) && p.contains("true or false")),
                    "{name} missing from {problems:?}"
                );
            }
        }
    }
}
