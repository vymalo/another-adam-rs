//! The configuration every agent binary shares, read from the environment.
//!
//! | Variable | Meaning | Default |
//! |---|---|---|
//! | `ROLE` | what this process runs: `all`, `control-plane` or `worker` ([`adam_host::Role`]) | `all` |
//! | `DATABASE_URL` | the run store: `postgres://`/`postgresql://`, `mongodb://`/`mongodb+srv://` or `sqlite://<path>` (one file, one process; `ROLE=all` only) | required |
//! | `NOTIFY_LOCAL_ONLY` | `true`/`1` or `false`/`0`: keep this process's run events local, without `NOTIFY` (one process; `ROLE=all`, one pod); signals still cross. Never detected ([`ServiceConfig::notify_local_only`]) | `false` |
//! | `A2A_BEARER_TOKENS` | comma-separated tokens accepted by the A2A server | required for `all` and `control-plane`, non-empty (fail closed) |
//! | `PUBLIC_URL` | URL clients reach the JSON-RPC endpoint at (agent card) | required for `all` and `control-plane` |
//! | `A2A_PUSH_ALLOWED_URLS` | turns A2A push notifications **on** and says which webhooks they may reach: comma-separated URL prefixes (`https://hooks.example.com/a2a/`) or hosts (`hooks.example.com`, `*.example.com`, `host:8443`) ([`PushSettings`]) | unset: push notifications are off, the card says so |
//! | `A2A_PUSH_ALLOW_PRIVATE` | also allow webhooks on loopback, private and link-local addresses, and `http` to loopback: **for development only** | `false` |
//! | `A2A_PUSH_GIVE_UP_AFTER_SECS` | how long one notification may keep failing before delivery to that webhook is abandoned (1 to 604800) | `3600` |
//! | `A2A_PUSH_REQUEST_TIMEOUT_SECS` | how long one request to a webhook may take (1 to 120) | `15` |
//! | `A2A_CARD_SIGNING_KEY_FILE` | a PKCS#8 PEM private key (ECDSA P-256 or Ed25519) that signs the agent card; mount it from a Secret ([`CardSigning`]) | unset: the card is unsigned |
//! | `A2A_CARD_SIGNING_KEY_ID` | the `kid` of the signature | the key's RFC 7638 thumbprint |
//! | `A2A_CARD_SIGNING_JKU` | the `jku` (URL of the key set) in the signature header; the server serves the key set at `/.well-known/jwks.json` | unset: no `jku` |
//! | `A2A_DOCS` | Swagger UI at `/docs` and the OpenAPI document at `/openapi.json`, both public (the calls still need a token); `false` turns them off ([`A2aSettings::docs`]) | `true` |
//! | `LISTEN_ADDR` | bind address: the A2A server, or for `worker` its `/healthz` listener | `0.0.0.0:8080` |
//! | `WORKERS` | runs advanced concurrently by this process | `4` |
//! | `MAX_PARALLEL_TOOL_CALLS` | the most tool calls of one model turn that run at the same time, 1 to 32 (`1` is one at a time); an agent file's `limits.max_parallel` may lower it for that agent; read by the roles that run workers ([`WorkerSettings::max_parallel_tool_calls`]) | `10` |
//! | `SESSION_MAX_OPEN` | the most sessions (a subagent that runs in the background) of one conversation open at once, 1 to 100; read by **every role**; the root agent's `limits.max_open_sessions` may lower it ([`SessionSettings::limits`]) | `16` |
//! | `SESSION_MAX_WORKING` | the most rounds of one conversation working at once, 1 to 32 and at most `SESSION_MAX_OPEN`; `limits.max_working_sessions` may lower it | `4` |
//! | `SESSION_MAX_ROUNDS` | the most rounds one session may have, 2 to 200; `limits.max_session_rounds` may lower it | `20` |
//! | `SESSION_IDLE_DAYS` | days a kept-open session may sit idle before it reads closed, 1 to 365; the same on every process that serves one store ([`SessionSettings::idle_ttl`]) | `7` |
//! | `WORKER_ID` | stable identity of this worker (the lease identity; the run owner for a binary that pins runs); letters, digits, `.`, `_`, `-` | random per process |
//! | `MODEL_BASE_URL` | OpenAI-compatible gateway, with its `/v1` prefix ([`ModelConfig`]) | required for `all` and `worker` |
//! | `MODEL_API_KEY` | bearer token for it (may be empty for local servers) | required for `all` and `worker`, unless `MODEL_AUTH=device` |
//! | `MODEL_AUTH` | `key` (the token of `MODEL_API_KEY`) or `device`: the process signs in to an OAuth 2.0 issuer with the device grant and renews its token (ADR 0058, [`ModelConfig::device_grant`]); the code and the link are a `WARN` log line at startup. With `device`, `MODEL_API_KEY` must be unset or empty | `key` |
//! | `MODEL_OAUTH_ISSUER` | with `MODEL_AUTH=device`: the issuer, whose `/.well-known/openid-configuration` names the device and token endpoints (on its own origin); `https`, or `http` on a loopback address | required with `device` |
//! | `MODEL_OAUTH_CLIENT_ID` | with `MODEL_AUTH=device`: the public client the issuer knows the process as | required with `device` |
//! | `MODEL_OAUTH_SCOPE` | with `MODEL_AUTH=device`: the scope asked for | `offline_access` |
//! | `MODEL` | model alias of the agent | required for `all` and `worker` |
//! | `MODEL_EXTRA_BODY` | a JSON object merged into every chat-completions request, to make a gateway or a model emit its reasoning (`{"reasoning_effort":"medium"}`); **not secret**; an invalid value is a startup error ([`ModelConfig::extra_body`]) | unset (nothing added) |
//! | `MODEL_ECHO_REASONING` | send the reasoning of earlier turns back, under this member name (`reasoning_content` or `reasoning`), for a provider that requires it (DeepSeek's thinking mode with tools); `false` or unset sends none ([`ModelConfig::echo_reasoning`]) | unset (never sent) |
//! | `MODEL_CONTEXT_WINDOW` | the context window of the model `MODEL` names, in tokens (1 to 9007199254740991): what each `usage/v1` call report of a call on that alias says as `contextWindow`; an invalid value is a startup error ([`ModelConfig::context_window`]) | unset (no `contextWindow`) |
//! | `MODEL_PROMPT_CACHE_KEY` | `off` or `conversation`: with `conversation` each request carries a `prompt_cache_key` that is the same for every request of one conversation (a SHA-256 of its id cut to 32 hex digits), so that a gateway with a prompt cache routes them together; anything else is a startup error ([`ModelConfig::prompt_cache_key`]) | `off` |
//! | `MODEL_INPUT_MODALITIES` | `text` or `text,image`: what the model `MODEL` names can read. With `image`, an image the person sends or a tool returns reaches the model, within the four limits below; without it a request carries a line of text in the image's place. Never guessed from a model name ([`ModelConfig::images`]) | `text` |
//! | `MODEL_IMAGE_MAX_BYTES` | the most bytes of one image sent to the model (1 to 4194304); needs `image` in `MODEL_INPUT_MODALITIES` | `1048576` |
//! | `MODEL_IMAGE_MAX_COUNT` | the most images one run admits (1 to 32); needs `image`. `MODEL_IMAGE_MAX_BYTES` times this may be at most 32 MiB, since a request resends every admitted image; over that is a startup error | `8` |
//! | `MODEL_IMAGE_MAX_SIDE` | the longest side of an image sent, in pixels (64 to 16384); needs `image` | `4096` |
//! | `MODEL_IMAGE_SOURCES` | which images may reach the model: a subset of `person`, `workspace`, `mcp` (an MCP server's result can carry expanded credentials, so it is opt-in); needs `image` | `person,workspace` |
//! | `MODEL_OUTPUT_MODALITIES` | `text`. `image` is refused: a model that makes images inline is not built for any provider yet (an image is made by an MCP server) | `text` |
//! | `MCP_ALLOW_STDIO`, `MCP_ALLOW_INSECURE`, `MCP_ALLOW_URL_VARS` | what the MCP servers of an agent folder may be ([`McpSettings`], feature `mcp`) | `false` each |
//! | `MCP_MAX_CALL_SECS` | the longest one MCP server is waited for, a call and a connection each: what `timeoutSecs` and `connectTimeoutSecs` ask for and the defaults (60 s and 30 s) are lowered to it, and a value the file asks for is at least 1 s; a larger value is lowered to it with a warning (1 to 86400; [`McpSettings`], feature `mcp`) | `900` |
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
//! * **Every role** needs `DATABASE_URL`, and reads `LISTEN_ADDR` and the `SESSION_*` limits
//!   ([`ServiceConfig::sessions`]): the control plane checks a person's start of a session and
//!   reads the listing, the workers run the tools.
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

use adam_model_oauth::{DeviceGrant, DeviceGrantSettings, LogPrompt, OAuthConfigError};
use std::net::SocketAddr;
use std::str::FromStr;

use adam_a2a::CardSigner;
use adam_a2a::push::PushPolicy;
use adam_error::{Classify, ErrorClass};
use adam_host::Role;
use adam_model::{DynModel, ImageLimits, ImageOrigin, InputModalities};
use adam_model_openai::{
    OpenAiCompatible, OpenAiConfig, OpenAiConfigError, ReasoningField, endpoint_for_logs,
};
use secrecy::SecretString;
use std::collections::BTreeSet;
use url::Url;

use crate::service::RuntimeOptions;
use adam_runtime::SessionLimits;
use std::time::Duration;

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

/// The path in the variable `name`, which must be an existing file: `None` when it is unset (or
/// blank), and also, after adding a problem naming the variable, when it is not a file (a path
/// that does not exist, or a directory). Reads the file system, not the file.
pub fn parse_file(
    get: &impl Fn(&str) -> Option<String>,
    name: &str,
    problems: &mut Vec<String>,
) -> Option<std::path::PathBuf> {
    let raw = get(name)?;
    let path = std::path::PathBuf::from(raw.trim());
    if path.is_file() {
        Some(path)
    } else {
        problems.push(format!(
            "{name} {:?} is not a file (name an existing file, or unset it)",
            path.display().to_string()
        ));
        None
    }
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
    /// `NOTIFY_LOCAL_ONLY`: keep this process's run events local, without
    /// `NOTIFY`. Read by every role: every role runs the `notify` component.
    pub notify_local_only: bool,
    /// `A2A_BEARER_TOKENS`. Empty unless [`Role::runs_control_plane`].
    pub a2a_bearer_tokens: Vec<SecretString>,
    /// `PUBLIC_URL`. `Some` exactly when [`Role::runs_control_plane`].
    pub public_url: Option<Url>,
    /// `LISTEN_ADDR`: the A2A server, or the `/healthz` listener of a worker.
    pub listen_addr: SocketAddr,
    /// `WORKERS` and `WORKER_ID`. `Some` exactly when [`Role::runs_workers`].
    pub worker: Option<WorkerSettings>,
    /// The optional A2A features (push notifications, the signature of the card): read only by a
    /// role that serves A2A, and nothing is on unless the environment turns it on.
    pub a2a: A2aSettings,
    /// `SESSION_*`: the limits on the sessions of one conversation and how long an idle one stays
    /// open. Read by every role.
    pub sessions: SessionSettings,
}

impl std::fmt::Debug for ServiceConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceConfig")
            .field("role", &self.role)
            .field("database_url", &"[REDACTED]")
            .field("notify_local_only", &self.notify_local_only)
            .field("a2a_bearer_tokens", &self.a2a_bearer_tokens.len())
            .field(
                "public_url",
                &self
                    .public_url
                    .as_ref()
                    .map(|u| endpoint_for_logs(u.as_str())),
            )
            .field("listen_addr", &self.listen_addr)
            .field("worker", &self.worker)
            .field("a2a", &self.a2a)
            .field("sessions", &self.sessions)
            .finish()
    }
}

impl ServiceConfig {
    /// Read the variables of the table in the [module docs](self) that [`ServiceConfig`] holds
    /// (`ROLE`, `DATABASE_URL`, `NOTIFY_LOCAL_ONLY`, `A2A_BEARER_TOKENS`, `PUBLIC_URL`,
    /// `LISTEN_ADDR` and, for a role that runs workers, `WORKERS` and `WORKER_ID`) through
    /// `lookup` (`None` = unset). A blank value counts as unset.
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

        let notify_local_only = parse_flag(&get, "NOTIFY_LOCAL_ONLY", problems);

        let worker = role
            .runs_workers()
            .then(|| WorkerSettings::parse(lookup, problems));
        let a2a = if front {
            A2aSettings::parse(lookup, problems)
        } else {
            A2aSettings::default()
        };

        Self {
            role,
            database_url: SecretString::from(database_url),
            notify_local_only,
            a2a_bearer_tokens,
            public_url,
            listen_addr,
            worker,
            a2a,
            sessions: SessionSettings::parse(lookup, problems),
        }
    }
}

/// The default of `SESSION_IDLE_DAYS`.
pub const DEFAULT_SESSION_IDLE_DAYS: u64 = 7;

/// The most `SESSION_IDLE_DAYS` may say.
pub const MAX_SESSION_IDLE_DAYS: u64 = 365;

/// `SESSION_MAX_OPEN`, `SESSION_MAX_WORKING`, `SESSION_MAX_ROUNDS` and `SESSION_IDLE_DAYS`
/// ([ADR 0045](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0045-a-subagent-can-run-in-the-background-as-a-session.md)
/// decision 5): what the deployment allows of the sessions of one conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionSettings {
    /// The most open, the most working and the most rounds. The root agent's file may lower each
    /// (`AgentDef::session_limits` of `adam-assembly` takes these).
    pub limits: SessionLimits,
    /// How long a kept-open session may sit idle (its latest round ended, nobody wrote to it)
    /// before it reads closed ([`RuntimeOptions::session_idle_ttl`]).
    pub idle_ttl: Duration,
}

impl Default for SessionSettings {
    fn default() -> Self {
        Self {
            limits: SessionLimits::default(),
            idle_ttl: Duration::from_secs(DEFAULT_SESSION_IDLE_DAYS * 24 * 3600),
        }
    }
}

impl SessionSettings {
    /// Read the four variables, each the default when unset or blank; a value out of its range,
    /// and a `SESSION_MAX_WORKING` that is set and above `SESSION_MAX_OPEN`, are problems (and the
    /// default). A `SESSION_MAX_WORKING` that is not set is cut down to `SESSION_MAX_OPEN`, so
    /// setting a small `SESSION_MAX_OPEN` alone is not an error.
    pub fn parse(lookup: &impl Fn(&str) -> Option<String>, problems: &mut Vec<String>) -> Self {
        let get = |name: &str| not_blank(lookup, name);
        let default = Self::default();
        // The value, and whether it is usable: a bad one is a problem and the default.
        let mut bounded = |name: &str, default: u32, low: u32, high: u32| {
            let before = problems.len();
            let n = parse_or(&get, name, default, problems);
            if problems.len() != before {
                return (default, false);
            }
            if (low..=high).contains(&n) {
                return (n, true);
            }
            problems.push(format!("{name} must be between {low} and {high}, got {n}"));
            (default, false)
        };
        let page = u32::try_from(adam_runtime::MAX_SESSION_PAGE).unwrap_or(100);
        let (max_open, open_ok) = bounded("SESSION_MAX_OPEN", default.limits.max_open(), 1, page);
        let working_set = get("SESSION_MAX_WORKING").is_some();
        let (max_working, working_ok) = bounded(
            "SESSION_MAX_WORKING",
            // Unset: the default, never more than may be open.
            default.limits.max_working().min(max_open),
            1,
            SessionLimits::MAX_WORKING_BOUND,
        );
        let (low, high) = SessionLimits::MAX_ROUNDS_BOUNDS;
        let (max_rounds, _) = bounded("SESSION_MAX_ROUNDS", default.limits.max_rounds(), low, high);
        let days = parse_or(
            &get,
            "SESSION_IDLE_DAYS",
            DEFAULT_SESSION_IDLE_DAYS,
            problems,
        );
        let days = if (1..=MAX_SESSION_IDLE_DAYS).contains(&days) {
            days
        } else {
            problems.push(format!(
                "SESSION_IDLE_DAYS must be between 1 and {MAX_SESSION_IDLE_DAYS}, got {days}"
            ));
            DEFAULT_SESSION_IDLE_DAYS
        };
        // Only two values that are each fine can conflict; a bad one is already a problem.
        if open_ok && working_ok && working_set && max_working > max_open {
            problems.push(format!(
                "SESSION_MAX_WORKING ({max_working}) is more than SESSION_MAX_OPEN ({max_open})"
            ));
        }
        let limits =
            SessionLimits::new(max_open, max_working, max_rounds).unwrap_or(default.limits);
        Self {
            limits,
            idle_ttl: Duration::from_secs(days * 24 * 3600),
        }
    }
}

/// The optional features of the A2A server: **nothing is on by default** but the docs.
#[derive(Clone, Debug)]
pub struct A2aSettings {
    /// `A2A_PUSH_*`: push notifications. `None`: off.
    pub push: Option<PushSettings>,
    /// `A2A_CARD_SIGNING_*`: the signature of the agent card. `None`: unsigned.
    pub card_signing: Option<CardSigning>,
    /// `A2A_DOCS`: Swagger UI at `/docs` and the OpenAPI document at `/openapi.json`, public.
    /// **On** (`true`) unless the variable says `false`.
    pub docs: bool,
}

impl Default for A2aSettings {
    fn default() -> Self {
        Self {
            push: None,
            card_signing: None,
            docs: true,
        }
    }
}

/// Push notifications the deployment turned on (`A2A_PUSH_*`): which webhooks they may reach and
/// how delivery behaves. The webhooks' credentials are the clients', in the store, never here.
#[derive(Clone, Debug)]
pub struct PushSettings {
    /// `A2A_PUSH_ALLOWED_URLS`, as written (already checked).
    pub allowed_urls: Vec<String>,
    /// `A2A_PUSH_ALLOW_PRIVATE`.
    pub allow_private_addresses: bool,
    /// `A2A_PUSH_GIVE_UP_AFTER_SECS`.
    pub give_up_after: std::time::Duration,
    /// `A2A_PUSH_REQUEST_TIMEOUT_SECS`.
    pub request_timeout: std::time::Duration,
}

impl PushSettings {
    /// The policy these settings state.
    ///
    /// # Errors
    ///
    /// An allow-list entry that cannot be read ([`PushSettings::parse`] has checked them, so this
    /// does not happen for settings that came from it).
    pub fn policy(&self) -> Result<PushPolicy, adam_a2a::push::PolicyEntryError> {
        Ok(PushPolicy::new(&self.allowed_urls)?
            .allow_private_addresses(self.allow_private_addresses))
    }

    /// Read the `A2A_PUSH_*` variables: `None` when `A2A_PUSH_ALLOWED_URLS` is unset or blank.
    pub fn parse(
        lookup: &impl Fn(&str) -> Option<String>,
        problems: &mut Vec<String>,
    ) -> Option<Self> {
        let get = |name: &str| not_blank(lookup, name);
        let raw = get("A2A_PUSH_ALLOWED_URLS")?;
        let allowed_urls: Vec<String> = raw
            .split(',')
            .map(str::trim)
            .filter(|e| !e.is_empty())
            .map(str::to_owned)
            .collect();
        if let Err(e) = PushPolicy::new(&allowed_urls) {
            problems.push(format!("A2A_PUSH_ALLOWED_URLS: {e}"));
        }
        let allow_private_addresses = parse_flag(&get, "A2A_PUSH_ALLOW_PRIVATE", problems);
        let mut secs = |name: &str, default: u64, max: u64| {
            let n = parse_or(&get, name, default, problems);
            if n == 0 || n > max {
                problems.push(format!("{name} must be between 1 and {max}, got {n}"));
                return std::time::Duration::from_secs(default);
            }
            std::time::Duration::from_secs(n)
        };
        let give_up_after = secs("A2A_PUSH_GIVE_UP_AFTER_SECS", 3600, 604_800);
        let request_timeout = secs("A2A_PUSH_REQUEST_TIMEOUT_SECS", 15, 120);
        Some(Self {
            allowed_urls,
            allow_private_addresses,
            give_up_after,
            request_timeout,
        })
    }
}

/// The key that signs the agent card (`A2A_CARD_SIGNING_*`), read and checked at startup. The
/// key never appears in `Debug` output.
#[derive(Clone, Debug)]
pub struct CardSigning {
    /// The signer: the key, its `kid` and the `jku` if there is one.
    pub signer: CardSigner,
}

impl CardSigning {
    /// Read `A2A_CARD_SIGNING_KEY_FILE` (and `_KEY_ID`, `_JKU`): `None` when the file is not
    /// named. A key id or a `jku` without a key is a problem, and so is a key that cannot be read
    /// or used.
    pub fn parse(
        lookup: &impl Fn(&str) -> Option<String>,
        problems: &mut Vec<String>,
    ) -> Option<Self> {
        let get = |name: &str| not_blank(lookup, name);
        let kid = get("A2A_CARD_SIGNING_KEY_ID");
        let jku = get("A2A_CARD_SIGNING_JKU");
        if get("A2A_CARD_SIGNING_KEY_FILE").is_none() {
            for (name, set) in [
                ("A2A_CARD_SIGNING_KEY_ID", kid.is_some()),
                ("A2A_CARD_SIGNING_JKU", jku.is_some()),
            ] {
                if set {
                    problems.push(format!("{name} needs A2A_CARD_SIGNING_KEY_FILE"));
                }
            }
            return None;
        }
        let path = parse_file(&get, "A2A_CARD_SIGNING_KEY_FILE", problems)?;
        let pem = match std::fs::read_to_string(&path) {
            Ok(pem) => pem,
            Err(e) => {
                problems.push(format!(
                    "A2A_CARD_SIGNING_KEY_FILE {:?} cannot be read: {}",
                    path.display().to_string(),
                    e.kind()
                ));
                return None;
            }
        };
        // The error names the problem with the key, never its content.
        let signer = match CardSigner::from_pem(&pem, kid.as_deref()) {
            Ok(signer) => signer,
            Err(e) => {
                problems.push(format!("A2A_CARD_SIGNING_KEY_FILE or _KEY_ID: {e}"));
                return None;
            }
        };
        let signer = match jku.as_deref() {
            None => signer,
            Some(jku) => match signer.with_jku(jku) {
                Ok(signer) => signer,
                Err(e) => {
                    problems.push(format!("A2A_CARD_SIGNING_JKU: {e}"));
                    return None;
                }
            },
        };
        Some(Self { signer })
    }
}

impl A2aSettings {
    /// Read the optional A2A variables, adding a problem for each unusable one.
    pub fn parse(lookup: &impl Fn(&str) -> Option<String>, problems: &mut Vec<String>) -> Self {
        let get = |name: &str| not_blank(lookup, name);
        let docs = get("A2A_DOCS").is_none() || parse_flag(&get, "A2A_DOCS", problems);
        Self {
            push: PushSettings::parse(lookup, problems),
            card_signing: CardSigning::parse(lookup, problems),
            docs,
        }
    }
}

/// The default of `MAX_PARALLEL_TOOL_CALLS`.
pub const DEFAULT_MAX_PARALLEL_TOOL_CALLS: usize = 10;

/// The most `MAX_PARALLEL_TOOL_CALLS` may say.
pub const MAX_PARALLEL_TOOL_CALLS_LIMIT: usize = 32;

/// `MAX_PARALLEL_TOOL_CALLS`: 1 to [`MAX_PARALLEL_TOOL_CALLS_LIMIT`], the default when unset or blank;
/// anything else is a problem and the default.
fn parse_max_parallel(get: &impl Fn(&str) -> Option<String>, problems: &mut Vec<String>) -> usize {
    const NAME: &str = "MAX_PARALLEL_TOOL_CALLS";
    let n = parse_or(get, NAME, DEFAULT_MAX_PARALLEL_TOOL_CALLS, problems);
    if (1..=MAX_PARALLEL_TOOL_CALLS_LIMIT).contains(&n) {
        return n;
    }
    problems.push(format!(
        "{NAME} must be between 1 and {MAX_PARALLEL_TOOL_CALLS_LIMIT}, got {n}"
    ));
    DEFAULT_MAX_PARALLEL_TOOL_CALLS
}

/// `WORKERS`, `WORKER_ID` and `MAX_PARALLEL_TOOL_CALLS`: how a worker process takes runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerSettings {
    /// `WORKERS`: runs advanced concurrently.
    pub workers: usize,
    /// `WORKER_ID`: the lease identity of this worker. `None`: a random one per process.
    pub worker_id: Option<String>,
    /// `MAX_PARALLEL_TOOL_CALLS`: the most tool calls of one model turn that run at the same time,
    /// 1 to [`MAX_PARALLEL_TOOL_CALLS_LIMIT`] (`1` is one at a time). An agent file's
    /// `limits.max_parallel` may lower it for that agent, never raise it.
    pub max_parallel_tool_calls: usize,
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
        let max_parallel_tool_calls = parse_max_parallel(&get, problems);
        Self {
            workers,
            worker_id,
            max_parallel_tool_calls,
        }
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
    /// `MODEL_AUTH=device` and `MODEL_OAUTH_*`: the provider that signs the process in with the
    /// OAuth 2.0 device grant and renews its token (ADR 0058), used instead of `api_key`. `None`
    /// (the default) sends `api_key`.
    pub device_grant: Option<DeviceGrant>,
    /// `MODEL`: the model alias the gateway knows.
    pub alias: String,
    /// `MODEL_EXTRA_BODY`: members merged into every request body (a flag that makes the model
    /// emit its reasoning). Not secret, and never a member the runtime owns (`model`, `messages`,
    /// `tools`, `tool_choice`, `stream`). `None` when unset or `{}`.
    pub extra_body: Option<serde_json::Map<String, serde_json::Value>>,
    /// `MODEL_ECHO_REASONING`: the member name under which the reasoning of earlier turns is sent
    /// back; `None` (the default) sends none.
    pub echo_reasoning: Option<ReasoningField>,
    /// `MODEL_CONTEXT_WINDOW`: the context window, in tokens, of the model [`alias`](Self::alias)
    /// names. The client says it for that alias only ([`OpenAiCompatible::with_context_window`]),
    /// so a call report says how full the context is; an agent whose files name another alias
    /// reports none. `None` when unset.
    pub context_window: Option<u64>,
    /// `MODEL_PROMPT_CACHE_KEY`: whether the client sends the conversation's `prompt_cache_key`.
    pub prompt_cache_key: PromptCacheKey,
    /// `MODEL_INPUT_MODALITIES` and the `MODEL_IMAGE_*` limits: what the model [`alias`](Self::alias)
    /// names may be sent as images. `None` (the default) is text only. The client says it for that
    /// alias only, so a subagent on another alias stays text only.
    pub images: Option<ImageLimits>,
}

/// The defaults of the `MODEL_IMAGE_*` variables, and their ranges.
pub const IMAGE_MAX_BYTES_DEFAULT: u64 = 1024 * 1024;
/// The largest `MODEL_IMAGE_MAX_BYTES`.
pub const IMAGE_MAX_BYTES_LIMIT: u64 = 4 * 1024 * 1024;
/// The default `MODEL_IMAGE_MAX_COUNT`.
pub const IMAGE_MAX_COUNT_DEFAULT: usize = 8;
/// The largest `MODEL_IMAGE_MAX_COUNT`.
pub const IMAGE_MAX_COUNT_LIMIT: usize = 32;
/// The default `MODEL_IMAGE_MAX_SIDE`.
pub const IMAGE_MAX_SIDE_DEFAULT: u32 = 4096;
/// The range of `MODEL_IMAGE_MAX_SIDE`.
pub const IMAGE_MAX_SIDE_RANGE: std::ops::RangeInclusive<u32> = 64..=16_384;
/// The most bytes of images one request may carry: `MODEL_IMAGE_MAX_BYTES` times
/// `MODEL_IMAGE_MAX_COUNT` over this is a startup error, because every request resends every image
/// the history holds.
pub const IMAGE_REQUEST_MAX_BYTES: u64 = 32 * 1024 * 1024;

/// `MODEL_PROMPT_CACHE_KEY`: whether the requests of a conversation carry a key that routes them to
/// the same prompt cache. Off by default: a gateway that rejects members it does not know would
/// fail every call (*unverified* for each).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PromptCacheKey {
    /// Send none.
    #[default]
    Off,
    /// Send `prompt_cache_key`, the same for every request of one conversation.
    Conversation,
}

impl std::fmt::Debug for ModelConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelConfig")
            // The gateway's address is a secret of the deployment: scheme and host only.
            .field("base_url", &endpoint_for_logs(&self.base_url))
            .field("alias", &self.alias)
            .field("device_grant", &self.device_grant)
            // The members, not their values.
            .field(
                "extra_body",
                &self
                    .extra_body
                    .as_ref()
                    .map(|m| m.keys().collect::<Vec<_>>()),
            )
            .field("echo_reasoning", &self.echo_reasoning)
            .field("context_window", &self.context_window)
            .field("prompt_cache_key", &self.prompt_cache_key)
            .field("images", &self.images)
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
        let (device, device_grant) = parse_device_grant(lookup, problems);
        let api_key = match (lookup("MODEL_API_KEY"), device) {
            (Some(v), false) => v,
            (Some(v), true) if !v.is_empty() => {
                problems.push(
                    "MODEL_API_KEY and MODEL_AUTH=device are exclusive: unset MODEL_API_KEY or leave it empty".into(),
                );
                String::new()
            }
            (_, true) => String::new(),
            (None, false) => {
                problems.push(
                    "MODEL_API_KEY is required (set it empty for a gateway without auth)".into(),
                );
                String::new()
            }
        };
        Self {
            base_url,
            api_key: SecretString::from(api_key),
            device_grant,
            alias,
            extra_body: parse_extra_body(&get("MODEL_EXTRA_BODY"), problems),
            echo_reasoning: parse_echo_reasoning(&get("MODEL_ECHO_REASONING"), problems),
            context_window: parse_context_window(&get("MODEL_CONTEXT_WINDOW"), problems),
            prompt_cache_key: parse_prompt_cache_key(&get("MODEL_PROMPT_CACHE_KEY"), problems),
            images: parse_images(&get, problems),
        }
    }

    /// The client for the gateway. Nothing is sent until a worker steps a run.
    ///
    /// # Errors
    ///
    /// [`OpenAiConfigError`] for a `MODEL_BASE_URL` that is not an absolute http(s) URL or a key
    /// that cannot be a header value.
    pub fn client(&self) -> Result<DynModel, OpenAiConfigError> {
        let mut client = OpenAiCompatible::new(OpenAiConfig::new(
            self.base_url.clone(),
            self.api_key.clone(),
        ))?;
        if let Some(grant) = &self.device_grant {
            // The code is asked for now, so the person can sign in before the first run.
            grant.start();
            client = client.with_credentials(std::sync::Arc::new(grant.clone()));
        }
        let mut client = client
            .with_echo_reasoning(self.echo_reasoning)
            .with_prompt_cache_key(self.prompt_cache_key == PromptCacheKey::Conversation);
        if let Some(window) = self.context_window {
            client = client.with_context_window(self.alias.clone(), window);
        }
        if let Some(limits) = &self.images {
            client = client.with_input_modalities(
                self.alias.clone(),
                InputModalities::with_images(limits.clone()),
            );
        }
        if let Some(extra) = &self.extra_body {
            client = client.with_extra_body(extra.clone())?;
        }
        Ok(std::sync::Arc::new(client))
    }
}

/// `MODEL_AUTH` and, with `device`, `MODEL_OAUTH_ISSUER`, `MODEL_OAUTH_CLIENT_ID` and
/// `MODEL_OAUTH_SCOPE`: whether `device` was chosen, and its provider (`None` for `key`, the
/// default, or when a setting has a problem). Each problem is added; nothing is sent.
fn parse_device_grant(
    lookup: &impl Fn(&str) -> Option<String>,
    problems: &mut Vec<String>,
) -> (bool, Option<DeviceGrant>) {
    match not_blank(lookup, "MODEL_AUTH")
        .as_deref()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        None | Some("key") => return (false, None),
        Some("device") => {}
        Some(_) => {
            problems.push("MODEL_AUTH must be `key` or `device`".into());
            return (false, None);
        }
    }
    let mut required = |name: &str| {
        let value = not_blank(lookup, name);
        if value.is_none() {
            problems.push(format!("{name} is required with MODEL_AUTH=device"));
        }
        value
    };
    let issuer = required("MODEL_OAUTH_ISSUER");
    let client_id = required("MODEL_OAUTH_CLIENT_ID");
    let (Some(issuer), Some(client_id)) = (issuer, client_id) else {
        return (true, None);
    };
    let mut settings = DeviceGrantSettings::new(issuer, client_id);
    if let Some(scope) = not_blank(lookup, "MODEL_OAUTH_SCOPE") {
        settings.scope = scope;
    }
    let grant = match DeviceGrant::new(settings, std::sync::Arc::new(LogPrompt)) {
        Ok(grant) => Some(grant),
        Err(OAuthConfigError::InvalidIssuer(_)) => {
            problems.push(
                "MODEL_OAUTH_ISSUER must be an absolute https URL (http only on a loopback address)".into(),
            );
            None
        }
        Err(e) => {
            problems.push(format!("MODEL_AUTH=device: {e}"));
            None
        }
    };
    (true, grant)
}

/// The words of a `MODEL_*_MODALITIES` value: a comma-separated list of `text` and `image`, which
/// must hold `text`. `None`, with a problem, for anything else.
fn parse_modalities(name: &str, raw: &str, problems: &mut Vec<String>) -> Option<Vec<String>> {
    let words: Vec<String> = raw
        .split(',')
        .map(|w| w.trim().to_ascii_lowercase())
        .collect();
    if words
        .iter()
        .any(|w| !matches!(w.as_str(), "text" | "image"))
        || words.is_empty()
    {
        problems.push(format!("{name} must be `text` or `text,image`"));
        return None;
    }
    if !words.iter().any(|w| w == "text") {
        problems.push(format!("{name} must include `text`"));
        return None;
    }
    Some(words)
}

/// `MODEL_INPUT_MODALITIES`, the `MODEL_IMAGE_*` limits and `MODEL_OUTPUT_MODALITIES`: the limits
/// when the model sees images, `None` for text only. An image variable set while the model is text
/// only is a problem that names it, so a deployer is never silently ignored; so is `image` in
/// `MODEL_OUTPUT_MODALITIES`.
fn parse_images(
    get: &impl Fn(&str) -> Option<String>,
    problems: &mut Vec<String>,
) -> Option<ImageLimits> {
    if let Some(raw) = get("MODEL_OUTPUT_MODALITIES")
        && let Some(words) = parse_modalities("MODEL_OUTPUT_MODALITIES", &raw, problems)
        && words.iter().any(|w| w == "image")
    {
        problems.push(
            "MODEL_OUTPUT_MODALITIES: `image` is not built for any provider yet; an image is made by an MCP server"
                .into(),
        );
    }
    let sees = match get("MODEL_INPUT_MODALITIES") {
        None => false,
        Some(raw) => parse_modalities("MODEL_INPUT_MODALITIES", &raw, problems)
            .is_some_and(|words| words.iter().any(|w| w == "image")),
    };
    let limits = [
        "MODEL_IMAGE_MAX_BYTES",
        "MODEL_IMAGE_MAX_COUNT",
        "MODEL_IMAGE_MAX_SIDE",
        "MODEL_IMAGE_SOURCES",
    ];
    if !sees {
        for name in limits {
            if get(name).is_some() {
                problems.push(format!(
                    "{name} is set but MODEL_INPUT_MODALITIES does not include `image`: set it to `text,image` or remove {name}"
                ));
            }
        }
        return None;
    }
    let number = |name: &str,
                  default: u64,
                  range: std::ops::RangeInclusive<u64>,
                  problems: &mut Vec<String>| {
        match get(name) {
            None => default,
            Some(raw) => match raw.trim().parse::<u64>() {
                Ok(n) if range.contains(&n) => n,
                _ => {
                    problems.push(format!(
                        "{name} must be a whole number from {} to {}",
                        range.start(),
                        range.end()
                    ));
                    default
                }
            },
        }
    };
    let max_bytes = number(
        "MODEL_IMAGE_MAX_BYTES",
        IMAGE_MAX_BYTES_DEFAULT,
        1..=IMAGE_MAX_BYTES_LIMIT,
        problems,
    );
    let max_count = number(
        "MODEL_IMAGE_MAX_COUNT",
        IMAGE_MAX_COUNT_DEFAULT as u64,
        1..=IMAGE_MAX_COUNT_LIMIT as u64,
        problems,
    );
    let max_side = number(
        "MODEL_IMAGE_MAX_SIDE",
        u64::from(IMAGE_MAX_SIDE_DEFAULT),
        u64::from(*IMAGE_MAX_SIDE_RANGE.start())..=u64::from(*IMAGE_MAX_SIDE_RANGE.end()),
        problems,
    );
    if max_bytes.saturating_mul(max_count) > IMAGE_REQUEST_MAX_BYTES {
        problems.push(format!(
            "MODEL_IMAGE_MAX_BYTES ({max_bytes}) times MODEL_IMAGE_MAX_COUNT ({max_count}) is over {} MiB, the most a request may carry: lower one of them",
            IMAGE_REQUEST_MAX_BYTES / (1024 * 1024)
        ));
    }
    let sources = match get("MODEL_IMAGE_SOURCES") {
        None => BTreeSet::from([ImageOrigin::Person, ImageOrigin::Workspace]),
        Some(raw) => {
            let mut set = BTreeSet::new();
            for word in raw.split(',').map(str::trim) {
                match word {
                    "person" => set.insert(ImageOrigin::Person),
                    "workspace" => set.insert(ImageOrigin::Workspace),
                    "mcp" => set.insert(ImageOrigin::Mcp),
                    _ => {
                        problems.push(
                            "MODEL_IMAGE_SOURCES must be a comma-separated subset of `person`, `workspace`, `mcp`"
                                .into(),
                        );
                        return None;
                    }
                };
            }
            set
        }
    };
    Some(ImageLimits {
        max_bytes,
        max_count: usize::try_from(max_count).unwrap_or(IMAGE_MAX_COUNT_DEFAULT),
        max_side: u32::try_from(max_side).unwrap_or(IMAGE_MAX_SIDE_DEFAULT),
        sources,
    })
}

/// `MODEL_EXTRA_BODY`: a JSON object, or a problem. The message names what is wrong and never
/// repeats the value (a parse error of `serde_json` says where, not what).
fn parse_extra_body(
    raw: &Option<String>,
    problems: &mut Vec<String>,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let raw = raw.as_deref()?;
    let value: serde_json::Value = match serde_json::from_str(raw) {
        Ok(value) => value,
        Err(e) => {
            problems.push(format!(
                "MODEL_EXTRA_BODY is not valid JSON ({}, line {} column {})",
                match e.classify() {
                    serde_json::error::Category::Eof => "it ends too soon",
                    _ => "it does not parse",
                },
                e.line(),
                e.column()
            ));
            return None;
        }
    };
    let serde_json::Value::Object(map) = value else {
        problems.push(
            "MODEL_EXTRA_BODY must be a JSON object, like {\"reasoning_effort\":\"medium\"}".into(),
        );
        return None;
    };
    for key in ["model", "messages", "tools", "tool_choice", "stream"] {
        if map.contains_key(key) {
            problems.push(format!(
                "MODEL_EXTRA_BODY may not set `{key}`: the runtime owns it"
            ));
            return None;
        }
    }
    Some(map).filter(|map| !map.is_empty())
}

/// The largest `MODEL_CONTEXT_WINDOW`: the largest count a `usage/v1` report may carry (2^53 - 1,
/// the largest integer a JSON number keeps exactly).
pub const MAX_CONTEXT_WINDOW: u64 = 9_007_199_254_740_991;

/// `MODEL_CONTEXT_WINDOW`: a whole number of tokens from 1 to [`MAX_CONTEXT_WINDOW`], or a problem.
fn parse_context_window(raw: &Option<String>, problems: &mut Vec<String>) -> Option<u64> {
    let raw = raw.as_deref()?.trim();
    match raw.parse::<u64>() {
        Ok(window) if (1..=MAX_CONTEXT_WINDOW).contains(&window) => Some(window),
        _ => {
            problems.push(format!(
                "MODEL_CONTEXT_WINDOW must be a whole number of tokens from 1 to {MAX_CONTEXT_WINDOW}, like 131072"
            ));
            None
        }
    }
}

/// `MODEL_ECHO_REASONING`: `reasoning_content`, `reasoning`, or off (`false`, `off`, `no`, `0`).
fn parse_echo_reasoning(
    raw: &Option<String>,
    problems: &mut Vec<String>,
) -> Option<ReasoningField> {
    match raw.as_deref().map(str::trim) {
        None | Some("false" | "off" | "no" | "0") => None,
        Some("reasoning_content") => Some(ReasoningField::ReasoningContent),
        Some("reasoning") => Some(ReasoningField::Reasoning),
        Some(_) => {
            problems.push(
                "MODEL_ECHO_REASONING must be `reasoning_content`, `reasoning` or `false`".into(),
            );
            None
        }
    }
}

/// `MODEL_PROMPT_CACHE_KEY`: `off` (also unset) or `conversation`, or a problem.
fn parse_prompt_cache_key(raw: &Option<String>, problems: &mut Vec<String>) -> PromptCacheKey {
    match raw.as_deref().map(str::trim) {
        None | Some("off") => PromptCacheKey::Off,
        Some("conversation") => PromptCacheKey::Conversation,
        Some(_) => {
            problems.push("MODEL_PROMPT_CACHE_KEY must be `off` or `conversation`".into());
            PromptCacheKey::Off
        }
    }
}

/// The default of `THREAD_TOOLS_MAX_CALL_SECS`: one hour.
#[cfg(feature = "mcp")]
pub const DEFAULT_THREAD_TOOLS_MAX_CALL_SECS: u64 = 3600;

/// The default of `MCP_MAX_CALL_SECS`: fifteen minutes.
#[cfg(feature = "mcp")]
pub const DEFAULT_MCP_MAX_CALL_SECS: u64 = 900;

/// The most `MCP_MAX_CALL_SECS` may say: a day.
#[cfg(feature = "mcp")]
pub const MAX_MCP_MAX_CALL_SECS: u64 = 86_400;

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
    /// `MCP_MAX_CALL_SECS`: the longest, in seconds, that an agent's own files may ask of one MCP
    /// server: a `timeoutSecs` or a `connectTimeoutSecs` of an `mcp.json` (or of the extra MCP
    /// file) above it is lowered to it, with a warning at startup. The files never decide alone
    /// how long a call may take. 1 to 86400, default 900.
    pub max_call_secs: u64,
}

#[cfg(feature = "mcp")]
impl Default for McpSettings {
    fn default() -> Self {
        Self {
            allow_stdio: false,
            allow_insecure: false,
            allow_url_vars: false,
            thread_tools_max_call_secs: DEFAULT_THREAD_TOOLS_MAX_CALL_SECS,
            max_call_secs: DEFAULT_MCP_MAX_CALL_SECS,
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
            max_call_secs: parse_mcp_max_call_secs(&get, problems),
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
            .max_call(std::time::Duration::from_secs(self.max_call_secs))
    }
}

/// `MCP_MAX_CALL_SECS`: 1 to 86400, [`DEFAULT_MCP_MAX_CALL_SECS`] when unset or blank; anything
/// else is a problem and the default.
#[cfg(feature = "mcp")]
fn parse_mcp_max_call_secs(
    get: &impl Fn(&str) -> Option<String>,
    problems: &mut Vec<String>,
) -> u64 {
    const NAME: &str = "MCP_MAX_CALL_SECS";
    let secs = parse_or(get, NAME, DEFAULT_MCP_MAX_CALL_SECS, problems);
    if (1..=MAX_MCP_MAX_CALL_SECS).contains(&secs) {
        return secs;
    }
    problems.push(format!(
        "{NAME} must be between 1 and {MAX_MCP_MAX_CALL_SECS} seconds, got {secs}"
    ));
    DEFAULT_MCP_MAX_CALL_SECS
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
    fn a_file_variable_must_name_an_existing_file() {
        let dir =
            std::env::temp_dir().join(format!("adam-service-parse-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("mcp.json");
        std::fs::write(&file, "{}").unwrap();
        let file_text = file.display().to_string();
        let dir_text = dir.display().to_string();
        let run = |value: Option<&str>| {
            let mut problems = Vec::new();
            let get = |_: &str| value.map(str::to_owned);
            let found = parse_file(&get, "ADAM_EXTRA_MCP_FILE", &mut problems);
            (found, problems)
        };
        assert_eq!(run(None), (None, vec![]));
        assert_eq!(run(Some(&file_text)), (Some(file.clone()), vec![]));
        assert_eq!(run(Some(&format!("  {file_text} "))), (Some(file), vec![]));
        for bad in [dir_text.as_str(), "/definitely/not/here.json"] {
            let (found, problems) = run(Some(bad));
            assert_eq!(found, None);
            assert!(mentions(&problems, "ADAM_EXTRA_MCP_FILE"), "{problems:?}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
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
                worker_id: None,
                max_parallel_tool_calls: 10,
            })
        );
    }

    /// `NOTIFY_LOCAL_ONLY` is a startup variable of every role (every role runs
    /// the `notify` component), so a bad value is exit 78, not a late failure.
    #[test]
    fn notify_local_only_is_a_boolean_read_by_every_role() {
        let c = parse(&full()).expect("valid");
        assert!(!c.notify_local_only, "off by default");
        for (raw, want) in [
            ("true", true),
            ("TRUE", true),
            ("1", true),
            ("false", false),
            ("0", false),
        ] {
            let mut vars = full();
            vars.insert("NOTIFY_LOCAL_ONLY", raw);
            assert_eq!(
                parse(&vars).expect("valid").notify_local_only,
                want,
                "{raw:?}"
            );
        }
        // A control plane runs no workers, but it does run `notify`.
        let mut vars = with_role("control-plane");
        vars.insert("NOTIFY_LOCAL_ONLY", "true");
        assert!(parse(&vars).expect("valid").notify_local_only);
        let mut vars = full();
        vars.insert("NOTIFY_LOCAL_ONLY", "yes");
        assert!(mentions(&problems_of(&vars), "NOTIFY_LOCAL_ONLY"));
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
    fn max_parallel_tool_calls_is_10_by_default_and_1_to_32() {
        let workers = |value: Option<&'static str>| {
            let mut vars = full();
            if let Some(value) = value {
                vars.insert("MAX_PARALLEL_TOOL_CALLS", value);
            }
            parse(&vars).map(|c| c.worker.unwrap().max_parallel_tool_calls)
        };
        assert_eq!(workers(None).unwrap(), 10);
        assert_eq!(workers(Some("  ")).unwrap(), 10, "blank is unset");
        assert_eq!(workers(Some("1")).unwrap(), 1);
        assert_eq!(workers(Some(" 32 ")).unwrap(), 32);
        for bad in ["0", "33", "x", "-1", "2.5"] {
            let problems = workers(Some(bad)).unwrap_err().problems;
            assert!(
                mentions(&problems, "MAX_PARALLEL_TOOL_CALLS"),
                "{bad}: {problems:?}"
            );
        }
    }

    #[test]
    fn a_control_plane_does_not_validate_max_parallel_tool_calls() {
        let mut vars = with_role("control-plane");
        vars.insert("MAX_PARALLEL_TOOL_CALLS", "99");
        let config = parse(&vars).expect("a control plane does not read it");
        assert!(config.worker.is_none());
    }

    #[test]
    fn session_settings_have_defaults_and_bounds() {
        let sessions = |pairs: &[(&'static str, &'static str)]| {
            let mut vars = full();
            for (k, v) in pairs {
                vars.insert(*k, *v);
            }
            parse(&vars).map(|c| c.sessions)
        };
        let d = sessions(&[]).unwrap();
        assert_eq!(d, SessionSettings::default());
        assert_eq!(
            (
                d.limits.max_open(),
                d.limits.max_working(),
                d.limits.max_rounds()
            ),
            (16, 4, 20)
        );
        assert_eq!(d.idle_ttl, Duration::from_secs(7 * 24 * 3600));
        assert_eq!(
            sessions(&[("SESSION_MAX_OPEN", "  ")]).unwrap(),
            d,
            "blank is unset"
        );

        let set = sessions(&[
            ("SESSION_MAX_OPEN", "100"),
            ("SESSION_MAX_WORKING", " 32 "),
            ("SESSION_MAX_ROUNDS", "200"),
            ("SESSION_IDLE_DAYS", "365"),
        ])
        .unwrap();
        assert_eq!(
            (
                set.limits.max_open(),
                set.limits.max_working(),
                set.limits.max_rounds()
            ),
            (100, 32, 200)
        );
        assert_eq!(set.idle_ttl, Duration::from_secs(365 * 24 * 3600));
        let low = sessions(&[
            ("SESSION_MAX_OPEN", "1"),
            ("SESSION_MAX_WORKING", "1"),
            ("SESSION_MAX_ROUNDS", "2"),
            ("SESSION_IDLE_DAYS", "1"),
        ])
        .unwrap();
        assert_eq!(low.idle_ttl, Duration::from_secs(24 * 3600));

        for (name, bad) in [
            ("SESSION_MAX_OPEN", "0"),
            ("SESSION_MAX_OPEN", "101"),
            ("SESSION_MAX_OPEN", "x"),
            ("SESSION_MAX_WORKING", "0"),
            ("SESSION_MAX_WORKING", "33"),
            ("SESSION_MAX_ROUNDS", "1"),
            ("SESSION_MAX_ROUNDS", "201"),
            ("SESSION_MAX_ROUNDS", "-1"),
            ("SESSION_IDLE_DAYS", "0"),
            ("SESSION_IDLE_DAYS", "366"),
            ("SESSION_IDLE_DAYS", "1.5"),
        ] {
            let problems = sessions(&[(name, bad)]).unwrap_err().problems;
            assert!(mentions(&problems, name), "{name}={bad}: {problems:?}");
        }
        // More working than open is one problem, naming both.
        let problems = sessions(&[("SESSION_MAX_OPEN", "3"), ("SESSION_MAX_WORKING", "5")])
            .unwrap_err()
            .problems;
        assert!(
            mentions(&problems, "SESSION_MAX_WORKING") && problems[0].contains("SESSION_MAX_OPEN"),
            "{problems:?}"
        );
        // The default working is cut to open when only open is set: no variable is blamed that the
        // operator never set.
        for open in ["2", "3"] {
            let c = sessions(&[("SESSION_MAX_OPEN", open)]).unwrap();
            let open: u32 = open.parse().unwrap();
            assert_eq!(
                (c.limits.max_open(), c.limits.max_working()),
                (open, open.min(4)),
                "SESSION_MAX_OPEN={open}"
            );
        }
        assert_eq!(
            sessions(&[("SESSION_MAX_OPEN", "9")])
                .unwrap()
                .limits
                .max_working(),
            4,
            "the default stands when open is larger"
        );
        // A bad open value is one problem, and says nothing of working.
        for bad in ["0", "101"] {
            let problems = sessions(&[("SESSION_MAX_OPEN", bad)]).unwrap_err().problems;
            assert_eq!(problems.len(), 1, "{bad}: {problems:?}");
            assert!(mentions(&problems, "SESSION_MAX_OPEN"), "{problems:?}");
        }
    }

    #[test]
    fn a_control_plane_reads_the_session_settings() {
        // The front checks a person's start and reads the listing, so every role reads them.
        for role in Role::VALUES {
            let mut vars = with_role(role.as_str());
            vars.insert("SESSION_MAX_OPEN", "5");
            vars.insert("SESSION_MAX_WORKING", "2");
            vars.insert("SESSION_MAX_ROUNDS", "9");
            vars.insert("SESSION_IDLE_DAYS", "3");
            let c = parse(&vars).unwrap().sessions;
            assert_eq!(
                (
                    c.limits.max_open(),
                    c.limits.max_working(),
                    c.limits.max_rounds()
                ),
                (5, 2, 9),
                "{role}"
            );
            assert_eq!(c.idle_ttl, Duration::from_secs(3 * 24 * 3600), "{role}");
            let mut bad = with_role(role.as_str());
            bad.insert("SESSION_IDLE_DAYS", "0");
            assert!(
                mentions(&problems_of(&bad), "SESSION_IDLE_DAYS"),
                "{role} does not stop at a bad value"
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
    fn nothing_of_a2a_is_on_by_default() {
        let config = parse(&full()).unwrap();
        assert!(config.a2a.push.is_none() && config.a2a.card_signing.is_none());
        // A role that does not serve A2A reads none of it, and is not stopped by a bad value.
        let mut vars = with_role("worker");
        vars.insert("A2A_PUSH_ALLOWED_URLS", "not a url at all");
        vars.insert("A2A_CARD_SIGNING_KEY_FILE", "/nonexistent");
        let config = parse(&vars).unwrap();
        assert!(config.a2a.push.is_none() && config.a2a.card_signing.is_none());
    }

    #[test]
    fn the_docs_are_on_unless_a2a_docs_says_false() {
        assert!(parse(&full()).unwrap().a2a.docs, "on by default");
        for (value, on) in [
            ("false", false),
            ("0", false),
            ("FALSE", false),
            ("true", true),
            ("1", true),
            ("  ", true),
        ] {
            let mut vars = full();
            vars.insert("A2A_DOCS", value);
            assert_eq!(parse(&vars).unwrap().a2a.docs, on, "A2A_DOCS={value:?}");
        }
        let mut vars = full();
        vars.insert("A2A_DOCS", "maybe");
        assert!(mentions(&problems_of(&vars), "A2A_DOCS"));
        // A role that serves no A2A reads none of it.
        let mut vars = with_role("worker");
        vars.insert("A2A_DOCS", "maybe");
        assert!(parse(&vars).is_ok());
    }

    #[test]
    fn push_is_turned_on_by_an_allow_list_and_nothing_else() {
        let mut vars = full();
        vars.insert(
            "A2A_PUSH_ALLOWED_URLS",
            " https://hooks.example.com/a2a/ , *.partner.io,, ",
        );
        let push = parse(&vars).unwrap().a2a.push.expect("on");
        assert_eq!(
            push.allowed_urls,
            ["https://hooks.example.com/a2a/", "*.partner.io"]
        );
        assert!(!push.allow_private_addresses);
        assert_eq!(push.give_up_after.as_secs(), 3600);
        assert_eq!(push.request_timeout.as_secs(), 15);
        let policy = push.policy().unwrap();
        assert!(policy.is_enabled() && !policy.allows_private_addresses());
        assert!(policy.check("https://hooks.example.com/a2a/x").is_ok());
        assert!(policy.check("https://hooks.example.com/other").is_err());

        vars.insert("A2A_PUSH_ALLOW_PRIVATE", "true");
        vars.insert("A2A_PUSH_GIVE_UP_AFTER_SECS", "60");
        vars.insert("A2A_PUSH_REQUEST_TIMEOUT_SECS", "5");
        let push = parse(&vars).unwrap().a2a.push.unwrap();
        assert!(push.allow_private_addresses);
        assert_eq!(
            (push.give_up_after.as_secs(), push.request_timeout.as_secs()),
            (60, 5)
        );

        // Blank means off.
        let mut off = full();
        off.insert("A2A_PUSH_ALLOWED_URLS", "  ");
        assert!(parse(&off).unwrap().a2a.push.is_none());
    }

    #[test]
    fn a_bad_push_setting_names_its_variable() {
        for (name, value) in [
            ("A2A_PUSH_ALLOWED_URLS", "ftp://hooks.example.com/"),
            ("A2A_PUSH_ALLOWED_URLS", "hooks.example.com/path"),
            ("A2A_PUSH_GIVE_UP_AFTER_SECS", "0"),
            ("A2A_PUSH_GIVE_UP_AFTER_SECS", "999999999"),
            ("A2A_PUSH_GIVE_UP_AFTER_SECS", "soon"),
            ("A2A_PUSH_REQUEST_TIMEOUT_SECS", "121"),
            ("A2A_PUSH_ALLOW_PRIVATE", "maybe"),
        ] {
            let mut vars = full();
            vars.insert("A2A_PUSH_ALLOWED_URLS", "hooks.example.com");
            vars.insert(name, value);
            assert!(mentions(&problems_of(&vars), name), "{name}={value}");
        }
    }

    #[test]
    fn the_card_signing_key_is_read_checked_and_never_shown() {
        use std::io::Write as _;
        let dir = std::env::temp_dir().join(format!("adam-service-sign-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("key.pem");
        std::fs::File::create(&good)
            .unwrap()
            .write_all(adam_a2a::generate_signing_key_pem(false).as_bytes())
            .unwrap();
        let bad = dir.join("bad.pem");
        std::fs::write(&bad, "not a key").unwrap();
        let (good, bad) = (
            good.to_str().unwrap().to_owned(),
            bad.to_str().unwrap().to_owned(),
        );
        let (good, bad): (&'static str, &'static str) = (
            Box::leak(good.into_boxed_str()),
            Box::leak(bad.into_boxed_str()),
        );

        let mut vars = full();
        vars.insert("A2A_CARD_SIGNING_KEY_FILE", good);
        vars.insert("A2A_CARD_SIGNING_KEY_ID", "key-1");
        vars.insert(
            "A2A_CARD_SIGNING_JKU",
            "https://agent.example.com/.well-known/jwks.json",
        );
        let config = parse(&vars).unwrap();
        let signing = config.a2a.card_signing.as_ref().expect("signing is on");
        assert_eq!(signing.signer.kid(), "key-1");
        assert_eq!(
            signing.signer.jku(),
            Some("https://agent.example.com/.well-known/jwks.json")
        );
        let shown = format!("{config:?}");
        assert!(
            !shown.contains("PRIVATE") && !shown.contains("MIG"),
            "{shown}"
        );

        // Without a kid, the thumbprint; a key that is not one, a missing file, and a kid or a
        // jku without a key are problems that name their variable and never the key.
        vars.remove("A2A_CARD_SIGNING_KEY_ID");
        vars.remove("A2A_CARD_SIGNING_JKU");
        assert!(
            !parse(&vars)
                .unwrap()
                .a2a
                .card_signing
                .unwrap()
                .signer
                .kid()
                .is_empty()
        );
        vars.insert("A2A_CARD_SIGNING_KEY_FILE", bad);
        let problems = problems_of(&vars);
        assert!(
            mentions(&problems, "A2A_CARD_SIGNING_KEY_FILE"),
            "{problems:?}"
        );
        assert!(!problems.join(" ").contains("not a key"));
        vars.insert("A2A_CARD_SIGNING_KEY_FILE", "/does/not/exist.pem");
        assert!(mentions(&problems_of(&vars), "A2A_CARD_SIGNING_KEY_FILE"));
        vars.remove("A2A_CARD_SIGNING_KEY_FILE");
        vars.insert("A2A_CARD_SIGNING_KEY_ID", "orphan");
        assert!(mentions(&problems_of(&vars), "A2A_CARD_SIGNING_KEY_ID"));
        vars.remove("A2A_CARD_SIGNING_KEY_ID");
        vars.insert("A2A_CARD_SIGNING_JKU", "https://x.example/jwks");
        assert!(mentions(&problems_of(&vars), "A2A_CARD_SIGNING_JKU"));
        let _ = std::fs::remove_dir_all(dir);
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

    fn model_vars() -> Vars {
        HashMap::from([
            ("MODEL_BASE_URL", "https://gw.example/v1"),
            ("MODEL_API_KEY", "sk-secret"),
            ("MODEL", "large"),
        ])
    }

    #[test]
    fn the_extra_body_is_a_json_object_or_a_startup_problem() {
        let mut vars = model_vars();
        // Unset, blank and `{}` add nothing.
        for none in [None, Some(""), Some("  "), Some("{}")] {
            if let Some(v) = none {
                vars.insert("MODEL_EXTRA_BODY", v);
            }
            let (model, problems) = model_problems(&vars);
            assert!(problems.is_empty(), "{none:?}: {problems:?}");
            assert!(model.extra_body.is_none(), "{none:?}");
        }
        vars.insert(
            "MODEL_EXTRA_BODY",
            r#"{"reasoning_effort":"medium","chat_template_kwargs":{"enable_thinking":true}}"#,
        );
        let (model, problems) = model_problems(&vars);
        assert!(problems.is_empty(), "{problems:?}");
        let extra = model.extra_body.as_ref().expect("an extra body");
        assert_eq!(extra["reasoning_effort"], "medium");
        // Its members show in `Debug`, its values do not.
        let shown = format!("{model:?}");
        assert!(shown.contains("reasoning_effort"), "{shown}");
        assert!(!shown.contains("medium"), "{shown}");
        model.client().expect("a usable client");

        // Not JSON, not an object, or a member the runtime owns: a problem that names the variable
        // and does not repeat the value.
        for bad in [
            "{oops",
            "[1]",
            "\"a string\"",
            "null",
            r#"{"model":"x"}"#,
            r#"{"stream":false}"#,
            r#"{"messages":[]}"#,
        ] {
            vars.insert("MODEL_EXTRA_BODY", bad);
            let (model, problems) = model_problems(&vars);
            assert_eq!(problems.len(), 1, "{bad}: {problems:?}");
            assert!(mentions(&problems, "MODEL_EXTRA_BODY"), "{problems:?}");
            assert!(!problems[0].contains("a string"), "{}", problems[0]);
            assert!(model.extra_body.is_none(), "{bad}");
        }
    }

    #[test]
    fn model_prompt_cache_key_parses_off_and_conversation_only() {
        let mut vars = model_vars();
        let (model, problems) = model_problems(&vars);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(
            model.prompt_cache_key,
            PromptCacheKey::Off,
            "off by default"
        );
        for (value, want) in [
            ("off", PromptCacheKey::Off),
            (" conversation ", PromptCacheKey::Conversation),
            ("", PromptCacheKey::Off),
        ] {
            vars.insert("MODEL_PROMPT_CACHE_KEY", value);
            let (model, problems) = model_problems(&vars);
            assert!(problems.is_empty(), "{value:?}: {problems:?}");
            assert_eq!(model.prompt_cache_key, want, "{value:?}");
            model.client().expect("a usable client");
        }
        for bad in ["on", "true", "Conversation", "run", "conversation,off"] {
            vars.insert("MODEL_PROMPT_CACHE_KEY", bad);
            let (model, problems) = model_problems(&vars);
            assert_eq!(problems.len(), 1, "{bad}: {problems:?}");
            assert!(
                mentions(&problems, "MODEL_PROMPT_CACHE_KEY"),
                "{problems:?}"
            );
            assert_eq!(model.prompt_cache_key, PromptCacheKey::Off, "{bad}");
        }
    }

    #[test]
    fn reasoning_is_echoed_only_when_asked_and_under_a_name_that_is_checked() {
        let mut vars = model_vars();
        assert_eq!(model_problems(&vars).0.echo_reasoning, None);
        for (value, want) in [
            ("reasoning_content", Some(ReasoningField::ReasoningContent)),
            ("reasoning", Some(ReasoningField::Reasoning)),
            ("false", None),
            ("off", None),
        ] {
            vars.insert("MODEL_ECHO_REASONING", value);
            let (model, problems) = model_problems(&vars);
            assert!(problems.is_empty(), "{value}: {problems:?}");
            assert_eq!(model.echo_reasoning, want, "{value}");
        }
        vars.insert("MODEL_ECHO_REASONING", "yes please");
        let (_, problems) = model_problems(&vars);
        assert!(mentions(&problems, "MODEL_ECHO_REASONING"), "{problems:?}");
    }

    #[test]
    fn the_context_window_is_a_positive_count_for_the_models_alias() {
        use adam_model::ModelClient as _;
        let mut vars = model_vars();
        let (model, problems) = model_problems(&vars);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(model.context_window, None);
        let client = model.client().expect("a usable client");
        assert_eq!(client.context_window("large"), None);

        for (value, want) in [
            ("131072", 131_072),
            (" 8192 ", 8192),
            ("1", 1),
            ("9007199254740991", MAX_CONTEXT_WINDOW),
        ] {
            vars.insert("MODEL_CONTEXT_WINDOW", value);
            let (model, problems) = model_problems(&vars);
            assert!(problems.is_empty(), "{value}: {problems:?}");
            assert_eq!(model.context_window, Some(want), "{value}");
            assert!(format!("{model:?}").contains(&want.to_string()));
            // The client says it for the alias `MODEL` names, and for no other.
            let client = model.client().expect("a usable client");
            assert_eq!(client.context_window("large"), Some(want), "{value}");
            assert_eq!(client.context_window("small"), None);
            assert_eq!(client.provider(), Some("openai"));
        }
        // Blank is unset.
        vars.insert("MODEL_CONTEXT_WINDOW", "  ");
        assert!(model_problems(&vars).1.is_empty());

        for bad in [
            "0",
            "-1",
            "128k",
            "1.5",
            "131072.0",
            "9007199254740992",
            "99999999999999999999999",
        ] {
            vars.insert("MODEL_CONTEXT_WINDOW", bad);
            let (model, problems) = model_problems(&vars);
            assert_eq!(problems.len(), 1, "{bad}: {problems:?}");
            assert!(mentions(&problems, "MODEL_CONTEXT_WINDOW"), "{problems:?}");
            assert_eq!(model.context_window, None, "{bad}");
        }
    }

    #[test]
    fn input_modalities_parse_and_refuse() {
        use adam_model::ImageOrigin;
        let mut vars = model_vars();
        let (model, problems) = model_problems(&vars);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(model.images, None, "text only is the default");
        for text in ["text", " TEXT ", "  "] {
            vars.insert("MODEL_INPUT_MODALITIES", text);
            let (model, problems) = model_problems(&vars);
            assert!(problems.is_empty(), "{text}: {problems:?}");
            assert_eq!(model.images, None, "{text}");
        }
        for both in ["text,image", "image, text"] {
            vars.insert("MODEL_INPUT_MODALITIES", both);
            let (model, problems) = model_problems(&vars);
            assert!(problems.is_empty(), "{both}: {problems:?}");
            let limits = model.images.expect("images");
            assert_eq!(limits.max_bytes, 1_048_576);
            assert_eq!(limits.max_count, 8);
            assert_eq!(limits.max_side, 4096);
            assert_eq!(
                limits.sources,
                std::collections::BTreeSet::from([ImageOrigin::Person, ImageOrigin::Workspace])
            );
        }
        vars.insert("MODEL_INPUT_MODALITIES", "text,image");
        vars.insert("MODEL_IMAGE_MAX_BYTES", "4194304");
        vars.insert("MODEL_IMAGE_MAX_COUNT", "8");
        vars.insert("MODEL_IMAGE_MAX_SIDE", "64");
        vars.insert("MODEL_IMAGE_SOURCES", "mcp, person");
        let (model, problems) = model_problems(&vars);
        assert!(problems.is_empty(), "{problems:?}");
        let limits = model.images.expect("images");
        assert_eq!(
            (limits.max_bytes, limits.max_count, limits.max_side),
            (4_194_304, 8, 64)
        );
        assert!(limits.sources.contains(&ImageOrigin::Mcp));
        assert!(!limits.sources.contains(&ImageOrigin::Workspace));

        for (name, bad) in [
            ("MODEL_INPUT_MODALITIES", "image"),
            ("MODEL_INPUT_MODALITIES", "text,audio"),
            ("MODEL_INPUT_MODALITIES", "text,,image"),
            ("MODEL_IMAGE_MAX_BYTES", "0"),
            ("MODEL_IMAGE_MAX_BYTES", "4194305"),
            ("MODEL_IMAGE_MAX_COUNT", "0"),
            ("MODEL_IMAGE_MAX_COUNT", "33"),
            ("MODEL_IMAGE_MAX_SIDE", "63"),
            ("MODEL_IMAGE_MAX_SIDE", "16385"),
            ("MODEL_IMAGE_MAX_SIDE", "big"),
            ("MODEL_IMAGE_SOURCES", "person,web"),
            ("MODEL_IMAGE_SOURCES", ""),
        ] {
            let mut vars = model_vars();
            vars.insert("MODEL_INPUT_MODALITIES", "text,image");
            vars.insert(name, bad);
            let (_, problems) = model_problems(&vars);
            // A blank value is unset.
            if bad.is_empty() {
                assert!(problems.is_empty(), "{name}={bad:?}: {problems:?}");
            } else {
                assert!(mentions(&problems, name), "{name}={bad:?}: {problems:?}");
            }
        }
    }

    #[test]
    fn a_request_may_carry_at_most_32_mib_of_images() {
        let with = |bytes: &'static str, count: &'static str| {
            let mut vars = model_vars();
            vars.insert("MODEL_INPUT_MODALITIES", "text,image");
            vars.insert("MODEL_IMAGE_MAX_BYTES", bytes);
            vars.insert("MODEL_IMAGE_MAX_COUNT", count);
            model_problems(&vars).1
        };
        // The defaults and the edge pass; one image more than fits is exit 78 naming both variables.
        assert!(with("1048576", "8").is_empty());
        assert!(with("4194304", "8").is_empty());
        assert!(with("1048576", "32").is_empty());
        for (bytes, count) in [("4194304", "9"), ("1048577", "32"), ("4194304", "32")] {
            let problems = with(bytes, count);
            assert_eq!(problems.len(), 1, "{problems:?}");
            assert!(
                problems[0].contains("MODEL_IMAGE_MAX_BYTES"),
                "{problems:?}"
            );
            assert!(
                problems[0].contains("MODEL_IMAGE_MAX_COUNT"),
                "{problems:?}"
            );
        }
    }

    #[test]
    fn image_settings_without_image_are_a_problem() {
        for name in [
            "MODEL_IMAGE_MAX_BYTES",
            "MODEL_IMAGE_MAX_COUNT",
            "MODEL_IMAGE_MAX_SIDE",
            "MODEL_IMAGE_SOURCES",
        ] {
            for modalities in [None, Some("text")] {
                let mut vars = model_vars();
                if let Some(m) = modalities {
                    vars.insert("MODEL_INPUT_MODALITIES", m);
                }
                vars.insert(name, "person");
                let (model, problems) = model_problems(&vars);
                assert_eq!(problems.len(), 1, "{name}: {problems:?}");
                assert!(mentions(&problems, name), "{problems:?}");
                assert_eq!(model.images, None);
            }
        }
    }

    #[test]
    fn output_image_is_refused_until_built() {
        let mut vars = model_vars();
        vars.insert("MODEL_OUTPUT_MODALITIES", "text");
        assert!(model_problems(&vars).1.is_empty());
        for bad in ["text,image", "image,text", "image"] {
            vars.insert("MODEL_OUTPUT_MODALITIES", bad);
            let (_, problems) = model_problems(&vars);
            assert!(
                mentions(&problems, "MODEL_OUTPUT_MODALITIES"),
                "{bad}: {problems:?}"
            );
        }
    }

    #[test]
    fn an_image_reaches_the_wire_only_for_alias_model() {
        use adam_model::ModelClient as _;
        let mut vars = model_vars();
        let client = model_problems(&vars).0.client().expect("client");
        assert!(client.input_modalities("large").images.is_none());
        vars.insert("MODEL_INPUT_MODALITIES", "text,image");
        let client = model_problems(&vars).0.client().expect("client");
        assert!(client.input_modalities("large").images.is_some());
        // A subagent on another alias is text only, whatever the deployment said of `MODEL`.
        assert!(client.input_modalities("small").images.is_none());
    }

    #[test]
    fn model_auth_device_signs_in_with_the_issuer_instead_of_the_key() {
        let mut vars = model_vars();
        vars.remove("MODEL_API_KEY");
        vars.insert("MODEL_AUTH", "device");
        vars.insert("MODEL_OAUTH_ISSUER", "https://id.example.test");
        vars.insert("MODEL_OAUTH_CLIENT_ID", "tools");
        let (model, problems) = model_problems(&vars);
        assert!(problems.is_empty(), "{problems:?}");
        assert!(model.device_grant.is_some());
        assert!(format!("{model:?}").contains("https://id.example.test"));
        // An empty key is accepted beside it; a key is not.
        vars.insert("MODEL_API_KEY", "");
        assert!(model_problems(&vars).1.is_empty());
        vars.insert("MODEL_API_KEY", "sk-secret");
        let (_, problems) = model_problems(&vars);
        assert!(
            mentions(&problems, "MODEL_API_KEY and MODEL_AUTH=device"),
            "{problems:?}"
        );
        assert!(
            problems.iter().all(|p| !p.contains("sk-secret")),
            "{problems:?}"
        );
    }

    #[test]
    fn model_auth_device_needs_an_issuer_and_a_client() {
        let mut vars = model_vars();
        vars.insert("MODEL_AUTH", "device");
        vars.remove("MODEL_API_KEY");
        let (model, problems) = model_problems(&vars);
        assert!(mentions(&problems, "MODEL_OAUTH_ISSUER"), "{problems:?}");
        assert!(mentions(&problems, "MODEL_OAUTH_CLIENT_ID"), "{problems:?}");
        assert!(
            !mentions(&problems, "MODEL_API_KEY"),
            "no key is asked for: {problems:?}"
        );
        assert!(model.device_grant.is_none());
        vars.insert("MODEL_OAUTH_CLIENT_ID", "tools");
        for bad in [
            "http://id.example.test",
            "id.example.test",
            "ftp://id.example.test",
        ] {
            vars.insert("MODEL_OAUTH_ISSUER", bad);
            let (_, problems) = model_problems(&vars);
            assert!(
                mentions(&problems, "MODEL_OAUTH_ISSUER"),
                "{bad}: {problems:?}"
            );
        }
        vars.insert("MODEL_OAUTH_ISSUER", "http://127.0.0.1:9");
        assert!(
            model_problems(&vars).1.is_empty(),
            "loopback http is allowed"
        );
    }

    #[test]
    fn model_auth_is_key_or_device() {
        let mut vars = model_vars();
        for ok in ["key", "KEY", "", " "] {
            vars.insert("MODEL_AUTH", ok);
            let (model, problems) = model_problems(&vars);
            assert!(problems.is_empty(), "{ok:?}: {problems:?}");
            assert!(model.device_grant.is_none());
        }
        vars.insert("MODEL_AUTH", "oauth");
        assert!(mentions(&model_problems(&vars).1, "MODEL_AUTH"));
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
        // The gateway's address is kept in a secret by deployments: only its scheme and host print.
        assert!(format!("{model:?}").contains("https://gw.example"));
        assert!(!format!("{model:?}").contains("/v1"));
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
        fn the_cap_on_what_the_files_may_ask_is_fifteen_minutes_and_can_be_set() {
            let (s, problems) = settings(&HashMap::new());
            assert!(problems.is_empty());
            assert_eq!(s.max_call_secs, 900);
            assert_eq!(
                s.policy().max_call_value(),
                std::time::Duration::from_secs(900)
            );
            for (value, secs) in [("", 900), (" 120 ", 120), ("1", 1), ("86400", 86_400)] {
                let (s, problems) = settings(&HashMap::from([("MCP_MAX_CALL_SECS", value)]));
                assert!(problems.is_empty(), "{value}: {problems:?}");
                assert_eq!(s.max_call_secs, secs, "{value}");
                assert_eq!(
                    s.policy().max_call_value(),
                    std::time::Duration::from_secs(secs)
                );
            }
            for bad in ["0", "86401", "-5", "a quarter hour", "1.5"] {
                let (s, problems) = settings(&HashMap::from([("MCP_MAX_CALL_SECS", bad)]));
                assert!(
                    problems.iter().any(|p| p.starts_with("MCP_MAX_CALL_SECS")),
                    "{bad}: {problems:?}"
                );
                assert_eq!(s.max_call_secs, 900, "{bad}");
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
