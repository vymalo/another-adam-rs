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
//! | `ACTIVITY_LISTEN_ADDR` | turns on the **activity listener**: a bind address that serves `GET /internal/activity` and nothing else, behind `ACTIVITY_BEARER_TOKENS`; the A2A port then stops serving the route ([`ActivitySettings`]). Read by every role; it must differ from `LISTEN_ADDR` | unset: the route stays on `LISTEN_ADDR` behind `A2A_BEARER_TOKENS` (**deprecated**) |
//! | `ACTIVITY_BEARER_TOKENS` | comma-separated tokens the activity listener accepts, none of them an A2A token; required with `ACTIVITY_LISTEN_ADDR` (fail closed: exit 78), refused without it | unset |
//! | `WORKERS` | runs advanced concurrently by this process | `4` |
//! | `MAX_PARALLEL_TOOL_CALLS` | the most tool calls of one model turn that run at the same time, 1 to 32 (`1` is one at a time); an agent file's `limits.max_parallel` may lower it for that agent; read by the roles that run workers ([`WorkerSettings::max_parallel_tool_calls`]), which refuse a bad value; **every role also reads it, leniently** (a bad value is the default and no problem), because a control plane holds a person's `start.config` to the same bound ([`SessionSettings::max_parallel_tool_calls`]) | `10` |
//! | `MAX_TOOL_CALLS_IN_FLIGHT` | the most tool calls of the whole process that run at the same time, 1 to 256: the process's pool, inside which `MAX_PARALLEL_TOOL_CALLS` still bounds one turn (ADR 0065) | `4` |
//! | `MAX_RUN_CONTINUATIONS` | the most times `run.ending` hooks may keep one run going, 0 to 20 (ADR 0050); anything else is exit 78 | `3` |
//! | `ADAM_HOOKS_FILE` | the deployment's hook programs (ADR 0050, slice P2a): a JSON file with `programs`, each `{name, command, args?, env?, agents?, points?, deadlineMs?}`; refused unless `HOOKS_ALLOW_PROGRAMS=true`, and a file under a directory the agent works on is refused ([`HooksSettings`](crate::HooksSettings)) | unset (no program) |
//! | `HOOKS_ALLOW_PROGRAMS` | `true` lets this process run hook programs at all; a file that names one without it is exit 78 | `false` |
//! | `MAX_MODEL_CALLS` | the most model calls in flight in the process, 1 to 64 (ADR 0065) | `2` |
//! | `MAX_PROCESSES` | the most commands (`run`, `run_command`, checks, background commands, language servers) of the whole process at once, 1 to 256; a command waits for a seat, then the tool says so and the run retries (ADR 0065) | `4` |
//! | `MAX_GIT_PROCESSES` | the most git processes at once, 1 to 64 (ADR 0065) | `2` |
//! | `COMMAND_MEMORY_MAX` | the memory one command may use: bytes, or `512MiB`, `2GB` (binary units), at least 64 MiB; over it the OS stops the command and the tool says "stopped: memory limit" | a quarter of the RAM, at most 8 GB |
//! | `COMMAND_CPU_MAX` | the CPU one command may use: cores (`2`, `1.5`) or millicores (`500m`), at least `100m`; needs a systemd user manager that delegates the `cpu` controller | half the cores, at least 1 |
//! | `MODEL_TOKENS_PER_MINUTE` | the model's tokens per minute, charged to a bucket the whole process shares (1 to 100000000); unset meters none | unset |
//! | `SESSION_MAX_OPEN` | the most sessions (a subagent that runs in the background) of one conversation open at once, 1 to 100; read by **every role**; the root agent's `limits.max_open_sessions` may lower it ([`SessionSettings::limits`]) | `16` |
//! | `SESSION_MAX_WORKING` | the most rounds of one conversation working at once, 1 to 32 and at most `SESSION_MAX_OPEN`; `limits.max_working_sessions` may lower it | `4` |
//! | `SESSION_MAX_ROUNDS` | the most rounds one session may have, 2 to 200; `limits.max_session_rounds` may lower it | `20` |
//! | `SESSION_IDLE_DAYS` | days a kept-open session may sit idle before it reads closed, 1 to 365; the same on every process that serves one store ([`SessionSettings::idle_ttl`]) | `7` |
//! | `WORKER_ID` | stable identity of this worker (the lease identity; the run owner for a binary that pins runs); letters, digits, `.`, `_`, `-` | random per process |
//! | `MODEL_BASE_URL` | OpenAI-compatible gateway, with its `/v1` prefix ([`ModelConfig`]) | required for `all` and `worker` |
//! | `MODEL_API_KEY` | bearer token for it (may be empty for local servers) | required for `all` and `worker`, unless `MODEL_AUTH=device` or `injector` |
//! | `MODEL_AUTH` | `key` (the token of `MODEL_API_KEY`), `device`: the process signs in to an OAuth 2.0 issuer with the device grant and renews its token (ADR 0058, [`ModelConfig::device_grant`]); the code and the link are a `WARN` log line at startup; or `injector`: no key, every model call carries `X-Run-Id: <root run id>` and no `Authorization`, for an injector in front of the model that adds the run's credential (ADR 0067, [`ModelConfig::injected_run`]). With `device` or `injector`, `MODEL_API_KEY` must be unset or empty | `key` |
//! | `MODEL_PARK_LIMIT` | how long parks on a timer in a row may last, from the first, before the run fails with the last reason: a model call the gateway refused (or could not be reached, with `MODEL_AUTH=injector`) parks the run and is made again (ADR 0067, [`ModelConfig::park_limit`]). Whole seconds, or a number with `s`, `m`, `h` or `d` (`24h`), from 60 s to 30 d; anything else is a startup error | `24h` |
//! | `MODEL_CA_FILE` | a PEM file of certificates trusted for the model endpoint, beside the default roots (ADR 0067, [`ModelConfig::ca_roots`]); not for the device grant's issuer. A file that cannot be read or holds no certificate is a startup error | unset (the default roots) |
//! | `RUN_CERT_ISSUER`, `RUN_CERT_ISSUER_URL`, `RUN_CERT_TOKEN_FILE`, `RUN_CERT_CA_FILE`, `RUN_CERT_RENEW_FRACTION` | a run's certificate from the broker ([`RunCertSettings`], ADR 0086): off when all are unset; a half-set configuration is a startup error naming the variable; `broker` is built, `openbao` is refused until A6; the token file is re-read on every call and never logged | off |
//! | `TURN_SIGNING_KEY_FILE` | a PKCS#8 PEM Ed25519 private key: every model call carries `Turn-Signature`, adam's signature of that request, and `X-Run-Id` (ADR 0068 amended 2026-11-25, [`ModelConfig::turn_keys`]). A file that cannot be read or is not an Ed25519 private key is a startup error | unset (no model call is signed) |
//! | `TURN_SIGNING_KEY_ID` | the `kid` of those signatures (printable ASCII, at most 256 bytes); needs the key file | the key's RFC 7638 thumbprint |
//! | `TURN_VERIFY_KEYS_FILE` | a JWKS of earlier public keys and `revoked` ids, read and checked with the key; needs the key file | the key's own public key |
//! | `MODEL_OAUTH_ISSUER` | with `MODEL_AUTH=device`: the issuer, whose `/.well-known/openid-configuration` names the device and token endpoints (on its own origin); `https`, or `http` on a loopback address | required with `device` |
//! | `MODEL_OAUTH_CLIENT_ID` | with `MODEL_AUTH=device`: the public client the issuer knows the process as | required with `device` |
//! | `MODEL_OAUTH_SCOPE` | with `MODEL_AUTH=device`: the scope asked for | `offline_access` |
//! | `MODEL` | model alias of the agent | required for `all` and `worker` |
//! | `MODEL_ALIASES_FILE` | the per-model profiles (ADR 0063, ADR 0064): **JSON only for now**, one object with `profiles` and `presets`; a file that cannot be read or does not parse is a startup error naming the file and the key ([`ModelConfig::profiles`]) | unset (no profile) |
//! | `MODEL_EXTRA_BODY` | a JSON object merged into every chat-completions request, to make a gateway or a model emit its reasoning (`{"reasoning_effort":"medium"}`); **not secret**; an invalid value is a startup error ([`ModelConfig::extra_body`]) | unset (nothing added) |
//! | `MODEL_ECHO_REASONING` | send the reasoning of earlier turns back, under this member name (`reasoning_content` or `reasoning`), for a provider that requires it (DeepSeek's thinking mode with tools); `false` or unset sends none ([`ModelConfig::echo_reasoning`]) | unset (never sent) |
//! | `DEFAULT_MODEL_CONTEXT_WINDOW` | the context window of the model `MODEL` names, in tokens (1 to 9007199254740991), used only when that model's profile names no `contextWindow` (ADR 0088): the window its history budget, its output cap and each `usage/v1` call report on that alias take; an invalid value is a startup error ([`ModelConfig::context_window`]) | unset (the profile's, else none) |
//! | `MODEL_CONTEXT_WINDOW` | **deprecated** alias of `DEFAULT_MODEL_CONTEXT_WINDOW`, read only when that is unset, with one warning at startup naming it; both set to different values is a startup error (two answers to one question), to equal values the value and the warning (ADR 0088 decision 2) | unset |
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
use adam_core::turn::{DynRunCertIssuer, RenewFraction, RunKeyring, RunSubject};
use adam_error::{Classify, ErrorClass};
use adam_host::Role;
use adam_model::{
    DEFAULT_MAX_OUTPUT_TOKENS, DynModel, ImageLimits, ImageOrigin, InputModalities, ModelProfiles,
};
use adam_model_openai::{
    ExtraRoots, OpenAiCompatible, OpenAiConfig, OpenAiConfigError, ReasoningField,
    endpoint_for_logs,
};
use adam_run_cert_broker::{BrokerError, BrokerIssuer};
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
/// Read a variable that names an **existing directory**, or report a problem and answer `None`.
///
/// The counterpart of [`parse_file`] for a setting that is a folder rather than a file (the L1
/// memory's `SI_MEMORY_DIR`, ADR 0081 decision 2). Unset or blank is `None`: a setting with no
/// default is absent, not an error.
pub fn parse_dir(
    get: &impl Fn(&str) -> Option<String>,
    name: &str,
    problems: &mut Vec<String>,
) -> Option<std::path::PathBuf> {
    let raw = get(name)?;
    let path = std::path::PathBuf::from(raw.trim());
    if path.is_dir() {
        Some(path)
    } else {
        problems.push(format!(
            "{name} {:?} is not a directory (name an existing directory, or unset it)",
            path.display().to_string()
        ));
        None
    }
}

/// Read a variable that names an **existing file**, or report a problem and answer `None`.
///
/// Unset or blank is `None`: a setting with no default is absent, not an error.
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
    /// `ACTIVITY_LISTEN_ADDR` and `ACTIVITY_BEARER_TOKENS`: the listener of its own that serves
    /// `GET /internal/activity`. `None`: the route is on [`listen_addr`](Self::listen_addr) behind
    /// the A2A tokens, as before (deprecated). Read by every role.
    pub activity: Option<ActivitySettings>,
    /// `WORKERS` and `WORKER_ID`. `Some` exactly when [`Role::runs_workers`].
    pub worker: Option<WorkerSettings>,
    /// The optional A2A features (push notifications, the signature of the card): read only by a
    /// role that serves A2A, and nothing is on unless the environment turns it on.
    pub a2a: A2aSettings,
    /// `SESSION_*`: the limits on the sessions of one conversation and how long an idle one stays
    /// open. Read by every role.
    pub sessions: SessionSettings,
    /// `RUN_RETENTION_DAYS`, `RUN_RETENTION_SWEEP_SECS`: how long finished runs are kept (90 days
    /// by default; `off` keeps all). Read by every role; swept by the roles that run workers.
    pub retention: crate::retention::RetentionSettings,
    /// `DEFAULT_LOCALE`: the agent's default language, a BCP 47 tag; the card lists `locale/v1`
    /// with it when set (ADR 0069). `None`: the card lists nothing.
    pub default_locale: Option<adam_a2a::Locale>,
    /// `UPLOADS_INLINE_MAX_BYTES`: the largest file the agent sends inline, 1 byte to 256 MiB; a larger
    /// one asks for an upload slot (`uploads/v1`; the card carries no params). `None`: the card says
    /// [`DEFAULT_UPLOADS_INLINE_MAX_BYTES`](adam_a2a::DEFAULT_UPLOADS_INLINE_MAX_BYTES). The agent folder's
    /// `limits.max_inline_upload_bytes` wins over it ([`Agents::uploads_inline_max_bytes`](crate::Agents)).
    pub uploads_inline_max_bytes: Option<u64>,
    /// `ADAM_HOOKS_FILE` and `HOOKS_ALLOW_PROGRAMS`: the deployment's hook programs (ADR 0050, slice
    /// P2a). Empty unless the file is named and the switch is on. Read by every role; a program is
    /// started per worker, not here.
    pub hooks: crate::hooks::HooksSettings,
    /// `MODEL_AUTH=injector` or `TURN_SIGNING_KEY_FILE` set, read by every role (the word only): the
    /// process's model calls carry a message's prompt statement and the person's envelope, so the
    /// card lists `prompt-statement/v1` (ADR 0077; with a turn key the calls are signed and the
    /// signature binds the values, ADR 0068 amended 2026-11-25). `false` with a key or a device
    /// grant and no turn key: such a card does not list it, and no message is read for it.
    pub prompt_statement: bool,
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
            .field("activity", &self.activity)
            .field("worker", &self.worker)
            .field("a2a", &self.a2a)
            .field("sessions", &self.sessions)
            .field("prompt_statement", &self.prompt_statement)
            .field("hooks", &self.hooks.programs().len())
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

        let activity = ActivitySettings::parse(lookup, listen_addr, &a2a_bearer_tokens, problems);

        let notify_local_only = parse_flag(&get, "NOTIFY_LOCAL_ONLY", problems);

        let worker = role
            .runs_workers()
            .then(|| WorkerSettings::parse(lookup, problems));
        let a2a = if front {
            A2aSettings::parse(lookup, problems)
        } else {
            A2aSettings::default()
        };

        let default_locale = get("DEFAULT_LOCALE").and_then(|raw| {
            adam_a2a::Locale::parse(raw.trim())
                .map_err(|e| problems.push(format!("DEFAULT_LOCALE is invalid: {e}")))
                .ok()
        });
        let uploads_inline_max_bytes = get("UPLOADS_INLINE_MAX_BYTES").and_then(|raw| {
            match raw.trim().parse::<u64>() {
                Ok(n) if (1..=adam_a2a::UPLOADS_INLINE_MAX_BYTES_CEILING).contains(&n) => Some(n),
                _ => {
                    problems.push(format!(
                        "UPLOADS_INLINE_MAX_BYTES is invalid: a whole number of bytes from 1 to {} (256 MiB), got {raw:?}",
                        adam_a2a::UPLOADS_INLINE_MAX_BYTES_CEILING
                    ));
                    None
                }
            }
        });
        Self {
            role,
            database_url: SecretString::from(database_url),
            notify_local_only,
            a2a_bearer_tokens,
            public_url,
            listen_addr,
            activity,
            worker,
            a2a,
            sessions: SessionSettings::parse(lookup, problems),
            retention: crate::retention::RetentionSettings::parse(lookup, problems),
            default_locale,
            uploads_inline_max_bytes,
            hooks: crate::hooks::HooksSettings::parse(&get, problems),
            prompt_statement: is_injector(lookup)
                || not_blank(lookup, "TURN_SIGNING_KEY_FILE").is_some(),
        }
    }
}

/// `ACTIVITY_LISTEN_ADDR` and `ACTIVITY_BEARER_TOKENS`: `GET /internal/activity` on a listener of
/// its own, with tokens of its own (ADR 0083).
///
/// The caller of the route (an operator that scales the service) then holds no A2A token and needs
/// no path to the A2A port: a token of this list is refused on the A2A listener, and an A2A token
/// is refused here. The listener serves that one route; every other path is a 404.
#[derive(Clone)]
pub struct ActivitySettings {
    /// `ACTIVITY_LISTEN_ADDR`: where the route is served. Never [`ServiceConfig::listen_addr`].
    pub listen_addr: SocketAddr,
    /// `ACTIVITY_BEARER_TOKENS`: the tokens it accepts. Never empty, and none of them is one of
    /// [`ServiceConfig::a2a_bearer_tokens`].
    pub bearer_tokens: Vec<SecretString>,
}

impl std::fmt::Debug for ActivitySettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActivitySettings")
            .field("listen_addr", &self.listen_addr)
            .field("bearer_tokens", &self.bearer_tokens.len())
            .finish()
    }
}

impl ActivitySettings {
    /// Read the two variables. `None` when `ACTIVITY_LISTEN_ADDR` is unset or blank (the route
    /// stays on `listen_addr`). Each of these adds a problem, so the process exits 78 before
    /// anything binds: an address with no usable token (fail closed), tokens with no address, an
    /// address that is not one or that `listen_addr` already takes, and a token that is also in
    /// `a2a_tokens`. No message carries a token.
    pub fn parse(
        lookup: &impl Fn(&str) -> Option<String>,
        listen_addr: SocketAddr,
        a2a_tokens: &[SecretString],
        problems: &mut Vec<String>,
    ) -> Option<Self> {
        use secrecy::ExposeSecret as _;
        let get = |name: &str| not_blank(lookup, name);
        let tokens_raw = get("ACTIVITY_BEARER_TOKENS");
        let Some(addr_raw) = get("ACTIVITY_LISTEN_ADDR") else {
            if tokens_raw.is_some() {
                problems.push(
                    "ACTIVITY_BEARER_TOKENS is set and ACTIVITY_LISTEN_ADDR is not: the tokens are \
                     read only by the activity listener. Set ACTIVITY_LISTEN_ADDR, or unset the tokens"
                        .into(),
                );
            }
            return None;
        };
        let addr = match addr_raw.trim().parse::<SocketAddr>() {
            Ok(addr) => Some(addr),
            Err(e) => {
                problems.push(format!(
                    "ACTIVITY_LISTEN_ADDR is invalid ({addr_raw:?}): {e}"
                ));
                None
            }
        };
        let tokens: Vec<SecretString> = tokens_raw
            .as_deref()
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(|t| SecretString::from(t.to_owned()))
            .collect();
        match &tokens_raw {
            None => problems.push(
                "ACTIVITY_BEARER_TOKENS is required when ACTIVITY_LISTEN_ADDR is set: the activity \
                 listener has tokens of its own and accepts no other (fail closed)"
                    .into(),
            ),
            Some(_) if tokens.is_empty() => {
                problems.push("ACTIVITY_BEARER_TOKENS has no usable token".into());
            }
            Some(_) => {}
        }
        let shared = tokens.iter().any(|token| {
            a2a_tokens
                .iter()
                .any(|a2a| a2a.expose_secret() == token.expose_secret())
        });
        if shared {
            problems.push(
                "ACTIVITY_BEARER_TOKENS and A2A_BEARER_TOKENS share a token: each listener needs \
                 tokens of its own, so that a caller of the activity route cannot reach A2A"
                    .into(),
            );
        }
        let addr = addr?;
        if same_port(addr, listen_addr) {
            problems.push(format!(
                "ACTIVITY_LISTEN_ADDR ({addr}) takes the port of LISTEN_ADDR ({listen_addr}): the \
                 activity route has a listener of its own, on another port"
            ));
        }
        Some(Self {
            listen_addr: addr,
            bearer_tokens: tokens,
        })
    }
}

/// Whether two bind addresses would take the same port: the same non-zero port, on the same
/// address or with either one unspecified (`0.0.0.0`, `::`). Port 0 picks a free port each time.
fn same_port(a: SocketAddr, b: SocketAddr) -> bool {
    a.port() != 0
        && a.port() == b.port()
        && (a.ip() == b.ip() || a.ip().is_unspecified() || b.ip().is_unspecified())
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
    /// `MAX_PARALLEL_TOOL_CALLS`, for the bounds of a person's `start.config` on **every role**
    /// (ADR 0054 decision 2): a control plane binds no agent, and holds a `config` to the same
    /// numbers the workers' session tools do. 1 to [`MAX_PARALLEL_TOOL_CALLS_LIMIT`], else
    /// [`DEFAULT_MAX_PARALLEL_TOOL_CALLS`]. Read **leniently**: a value out of range is the
    /// default here and no problem (the roles that run workers refuse it, with
    /// [`WorkerSettings::max_parallel_tool_calls`], and no worker runs on it), so a control plane
    /// still starts as it did.
    pub max_parallel_tool_calls: usize,
}

impl Default for SessionSettings {
    fn default() -> Self {
        Self {
            max_parallel_tool_calls: DEFAULT_MAX_PARALLEL_TOOL_CALLS,
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
        // The roles that run workers report a bad value (`WorkerSettings`); here it is the
        // default, never a second problem and never a reason for a control plane not to start.
        let max_parallel_tool_calls = parse_max_parallel(&get, &mut Vec::new());
        Self {
            max_parallel_tool_calls,
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

/// The default of `MAX_TOOL_CALLS_IN_FLIGHT`: the desktop's four (ADR 0065 decision 11).
pub const DEFAULT_MAX_TOOL_CALLS_IN_FLIGHT: usize = 4;

/// The most `MAX_TOOL_CALLS_IN_FLIGHT` may say.
pub const MAX_TOOL_CALLS_IN_FLIGHT_LIMIT: usize = 256;

/// The default of `MAX_MODEL_CALLS`: the desktop's two.
pub const DEFAULT_MAX_MODEL_CALLS: usize = 2;

/// The most `MAX_MODEL_CALLS` may say.
pub const MAX_MODEL_CALLS_LIMIT: usize = 64;

/// The default of `MAX_PROCESSES`: the desktop's four (ADR 0065 decision 11).
pub const DEFAULT_MAX_PROCESSES: usize = 4;

/// The most `MAX_PROCESSES` may say.
pub const MAX_PROCESSES_LIMIT: usize = 256;

/// The default of `MAX_GIT_PROCESSES`.
pub const DEFAULT_MAX_GIT_PROCESSES: usize = 2;

/// The most `MAX_GIT_PROCESSES` may say.
pub const MAX_GIT_PROCESSES_LIMIT: usize = 64;

/// The least `COMMAND_MEMORY_MAX` may say: 64 MiB, under which no shell starts.
pub const COMMAND_MEMORY_MIN: u64 = 64 * 1024 * 1024;

/// The least `COMMAND_CPU_MAX` may say, in millicores: a tenth of a core.
pub const COMMAND_CPU_MIN: u32 = 100;

/// The most `MODEL_TOKENS_PER_MINUTE` may say.
pub const MODEL_TOKENS_PER_MINUTE_LIMIT: u32 = 100_000_000;

/// A count variable in `low..=high`, the default when unset or blank, a problem and the default
/// when it is not.
fn bounded_count(
    get: &impl Fn(&str) -> Option<String>,
    name: &str,
    default: usize,
    high: usize,
    problems: &mut Vec<String>,
) -> usize {
    let n = parse_or(get, name, default, problems);
    if (1..=high).contains(&n) {
        return n;
    }
    problems.push(format!("{name} must be between 1 and {high}, got {n}"));
    default
}

/// `MODEL_TOKENS_PER_MINUTE`: unset or blank meters no tokens; otherwise 1 to
/// [`MODEL_TOKENS_PER_MINUTE_LIMIT`], and anything else is a problem and no rate.
fn parse_tokens_per_minute(
    get: &impl Fn(&str) -> Option<String>,
    problems: &mut Vec<String>,
) -> Option<u32> {
    const NAME: &str = "MODEL_TOKENS_PER_MINUTE";
    let raw = get(NAME)?;
    let Ok(n) = raw.trim().parse::<u32>() else {
        problems.push(format!("{NAME} is invalid ({raw:?}): not a whole number"));
        return None;
    };
    if (1..=MODEL_TOKENS_PER_MINUTE_LIMIT).contains(&n) {
        return Some(n);
    }
    problems.push(format!(
        "{NAME} must be between 1 and {MODEL_TOKENS_PER_MINUTE_LIMIT}, got {n}"
    ));
    None
}

/// The RAM of this machine, in bytes, where the system says (`MemTotal` of `/proc/meminfo`).
fn total_ram() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = meminfo.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    kib.checked_mul(1024)
}

/// `COMMAND_MEMORY_MAX`: a size (`2GB`, `512MiB`, bytes) of at least [`COMMAND_MEMORY_MIN`]; unset or
/// blank is a quarter of the RAM, at most 8 GB. Anything else is a problem and the default.
fn parse_command_memory(get: &impl Fn(&str) -> Option<String>, problems: &mut Vec<String>) -> u64 {
    const NAME: &str = "COMMAND_MEMORY_MAX";
    let default = adam_budget::default_command_memory(total_ram());
    let Some(raw) = get(NAME) else {
        return default;
    };
    match adam_budget::parse_bytes(&raw) {
        Ok(bytes) if bytes >= COMMAND_MEMORY_MIN => bytes,
        Ok(bytes) => {
            problems.push(format!(
                "{NAME} must be at least {COMMAND_MEMORY_MIN} bytes (64 MiB), got {bytes}"
            ));
            default
        }
        Err(e) => {
            problems.push(format!("{NAME} is invalid ({raw:?}): {e}"));
            default
        }
    }
}

/// `COMMAND_CPU_MAX`: cores (`2`, `1.5`) or millicores (`500m`) of at least [`COMMAND_CPU_MIN`];
/// unset or blank is half the cores, at least one. Anything else is a problem and the default.
fn parse_command_cpu(get: &impl Fn(&str) -> Option<String>, problems: &mut Vec<String>) -> u32 {
    const NAME: &str = "COMMAND_CPU_MAX";
    let cores =
        std::thread::available_parallelism().map_or(1, |n| u32::try_from(n.get()).unwrap_or(1));
    let default = adam_budget::default_command_cpu(cores);
    let Some(raw) = get(NAME) else {
        return default;
    };
    match adam_budget::parse_millicores(&raw) {
        Ok(milli) if milli >= COMMAND_CPU_MIN => milli,
        Ok(milli) => {
            problems.push(format!(
                "{NAME} must be at least {COMMAND_CPU_MIN} millicores (0.1 core), got {milli}"
            ));
            default
        }
        Err(e) => {
            problems.push(format!("{NAME} is invalid ({raw:?}): {e}"));
            default
        }
    }
}

/// `MAX_RUN_CONTINUATIONS`: the most times `run.ending` hooks may keep one run going (ADR 0050,
/// decision 10), 0 to this.
pub const MAX_RUN_CONTINUATIONS_LIMIT: u32 = 20;
/// The default of `MAX_RUN_CONTINUATIONS`.
pub const DEFAULT_MAX_RUN_CONTINUATIONS: u32 = 3;

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
    /// `MAX_TOOL_CALLS_IN_FLIGHT`: the most tool calls of the whole process that run at once, 1 to
    /// [`MAX_TOOL_CALLS_IN_FLIGHT_LIMIT`]. The per-turn bound sits inside it (ADR 0065).
    pub max_tool_calls_in_flight: usize,
    /// `MAX_MODEL_CALLS`: the most model calls in flight in the process, 1 to
    /// [`MAX_MODEL_CALLS_LIMIT`].
    pub max_model_calls: usize,
    /// `MODEL_TOKENS_PER_MINUTE`: the model's tokens per minute, or `None` to meter none.
    pub model_tokens_per_minute: Option<u32>,
    /// `MAX_PROCESSES`: the most commands (`run`, `run_command`, checks, background commands and
    /// language servers) at once, 1 to [`MAX_PROCESSES_LIMIT`] (ADR 0065).
    pub max_processes: usize,
    /// `MAX_GIT_PROCESSES`: the most git processes at once, 1 to [`MAX_GIT_PROCESSES_LIMIT`].
    pub max_git_processes: usize,
    /// `COMMAND_MEMORY_MAX`: the memory one command may use, in bytes.
    pub command_memory_max: u64,
    /// `COMMAND_CPU_MAX`: the CPU one command may use, in millicores.
    pub command_cpu_max: u32,
    /// `MAX_RUN_CONTINUATIONS`: the most times `run.ending` hooks may keep one run going, 0 to
    /// `MAX_RUN_CONTINUATIONS_LIMIT`; hand it to `LlmAgentBuilder::max_run_continuations`.
    pub max_run_continuations: u32,
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
        let max_tool_calls_in_flight = bounded_count(
            &get,
            "MAX_TOOL_CALLS_IN_FLIGHT",
            DEFAULT_MAX_TOOL_CALLS_IN_FLIGHT,
            MAX_TOOL_CALLS_IN_FLIGHT_LIMIT,
            problems,
        );
        let max_model_calls = bounded_count(
            &get,
            "MAX_MODEL_CALLS",
            DEFAULT_MAX_MODEL_CALLS,
            MAX_MODEL_CALLS_LIMIT,
            problems,
        );
        let model_tokens_per_minute = parse_tokens_per_minute(&get, problems);
        let max_processes = bounded_count(
            &get,
            "MAX_PROCESSES",
            DEFAULT_MAX_PROCESSES,
            MAX_PROCESSES_LIMIT,
            problems,
        );
        let max_git_processes = bounded_count(
            &get,
            "MAX_GIT_PROCESSES",
            DEFAULT_MAX_GIT_PROCESSES,
            MAX_GIT_PROCESSES_LIMIT,
            problems,
        );
        let command_memory_max = parse_command_memory(&get, problems);
        let command_cpu_max = parse_command_cpu(&get, problems);
        let max_run_continuations = {
            let n = parse_or(
                &get,
                "MAX_RUN_CONTINUATIONS",
                DEFAULT_MAX_RUN_CONTINUATIONS,
                problems,
            );
            if n > MAX_RUN_CONTINUATIONS_LIMIT {
                problems.push(format!(
                    "MAX_RUN_CONTINUATIONS must be between 0 and {MAX_RUN_CONTINUATIONS_LIMIT}, got {n}"
                ));
                DEFAULT_MAX_RUN_CONTINUATIONS
            } else {
                n
            }
        };
        Self {
            workers,
            worker_id,
            max_parallel_tool_calls,
            max_tool_calls_in_flight,
            max_model_calls,
            model_tokens_per_minute,
            max_processes,
            max_git_processes,
            command_memory_max,
            command_cpu_max,
            max_run_continuations,
        }
    }

    /// The process's budget: this many runs ([`WORKERS`](Self::workers)), the two pools of
    /// [ADR 0065](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0065-resource-governance-admission-pools-and-os-limits.md)
    /// slice 2, the process pools and the memory and CPU of one command (slice 3) and, when the
    /// deployment meters them, the tokens per minute. Every other ceiling is `Budget::UNLIMITED`:
    /// the slices that build them lower it.
    #[must_use]
    pub fn budget(&self) -> adam_budget::Budget {
        let budget = adam_budget::Budget::builder()
            .runs(u32::try_from(self.workers).unwrap_or(u32::MAX))
            .tool_calls(u32::try_from(self.max_tool_calls_in_flight).unwrap_or(u32::MAX))
            .model_calls(u32::try_from(self.max_model_calls).unwrap_or(u32::MAX))
            .processes(u32::try_from(self.max_processes).unwrap_or(u32::MAX))
            .git(u32::try_from(self.max_git_processes).unwrap_or(u32::MAX))
            .command_memory(self.command_memory_max)
            .command_cpu(self.command_cpu_max)
            // The share ADR 0065 decision 11 gives one conversation: half of each pool.
            .per_conversation_share(adam_budget::Ratio::HALF);
        match self.model_tokens_per_minute {
            Some(tokens) => budget.tokens_per_minute(tokens).build(),
            None => budget.build(),
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

/// `RUN_CERT_ISSUER`: which adapter of `RunCertIssuer` a run's certificate comes from. A closed
/// set: `broker` is built; `openbao` is reserved for A6 and refused until then; anything else is a
/// startup problem naming the variable (exit 78).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunCertIssuerKind {
    /// The credential broker's internal route, contract v1 (`adam-run-cert-broker`).
    Broker,
    /// The OpenBao issuer (A6): parsed, not built.
    OpenBao,
}

impl RunCertIssuerKind {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "broker" => Some(Self::Broker),
            "openbao" => Some(Self::OpenBao),
            _ => None,
        }
    }

    /// The value `RUN_CERT_ISSUER` holds.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Broker => "broker",
            Self::OpenBao => "openbao",
        }
    }
}

/// `RUN_CERT_*`: a run's certificate comes from `RUN_CERT_ISSUER_URL`, presenting the projected
/// token in `RUN_CERT_TOKEN_FILE`, trusting the anchors of `RUN_CERT_CA_FILE` beside the system
/// roots, and renewing at `RUN_CERT_RENEW_FRACTION` of the leaf's life (ADR 0086, A3).
///
/// **Nothing is on unless every required variable is set.** All of them unset is off; any of them
/// set without `RUN_CERT_ISSUER`, `RUN_CERT_ISSUER_URL` and `RUN_CERT_TOKEN_FILE` is a startup
/// problem naming the variable (exit 78), because a half-set run certificate would silently sign
/// with the process turn key instead. `RUN_CERT_CA_FILE` and `RUN_CERT_RENEW_FRACTION` are
/// optional, but they too are refused without the three required ones.
///
/// The token itself is never read here: only its file's path. [`issuer`](Self::issuer) builds one
/// issuer per run, and [`keyring`](Self::keyring) the run's keyring over it.
#[derive(Clone)]
pub struct RunCertSettings {
    /// `RUN_CERT_ISSUER`: the adapter.
    pub issuer: RunCertIssuerKind,
    /// `RUN_CERT_ISSUER_URL`: the broker's base URL, without the route.
    pub url: String,
    /// `RUN_CERT_TOKEN_FILE`: the projected token, re-read on every call.
    pub token_file: std::path::PathBuf,
    /// `RUN_CERT_CA_FILE`: PEM anchors trusted beside the system roots, read at startup.
    pub ca_file: Option<std::path::PathBuf>,
    /// `RUN_CERT_RENEW_FRACTION`: when the run key is replaced, two thirds by default.
    pub renew_fraction: RenewFraction,
    http: adam_run_cert_broker::HttpClient,
}

impl RunCertSettings {
    /// Read the `RUN_CERT_*` variables. `None` when none is set (run certificates off); a problem
    /// naming the variable for every missing or malformed one, and `None` too.
    pub fn parse(
        lookup: &impl Fn(&str) -> Option<String>,
        problems: &mut Vec<String>,
    ) -> Option<Self> {
        let get = |name: &str| not_blank(lookup, name);
        let issuer_raw = get("RUN_CERT_ISSUER");
        let url_raw = get("RUN_CERT_ISSUER_URL");
        let token_raw = get("RUN_CERT_TOKEN_FILE");
        let ca_raw = get("RUN_CERT_CA_FILE");
        let fraction_raw = get("RUN_CERT_RENEW_FRACTION");
        if [&issuer_raw, &url_raw, &token_raw, &ca_raw, &fraction_raw]
            .iter()
            .all(|value| value.is_none())
        {
            return None;
        }
        let off = "unset every RUN_CERT_* variable to turn run certificates off";
        let mut complete = true;
        for (name, value) in [
            ("RUN_CERT_ISSUER", &issuer_raw),
            ("RUN_CERT_ISSUER_URL", &url_raw),
            ("RUN_CERT_TOKEN_FILE", &token_raw),
        ] {
            if value.is_none() {
                problems.push(format!(
                    "{name} is required when any RUN_CERT_* variable is set ({off})"
                ));
                complete = false;
            }
        }

        let issuer = issuer_raw
            .as_deref()
            .and_then(|raw| match RunCertIssuerKind::parse(raw) {
                Some(RunCertIssuerKind::Broker) => Some(RunCertIssuerKind::Broker),
                Some(RunCertIssuerKind::OpenBao) => {
                    problems
                        .push("RUN_CERT_ISSUER=openbao is not built yet (A6): set broker".into());
                    None
                }
                None => {
                    problems.push(format!(
                        "RUN_CERT_ISSUER is invalid: {raw:?} (broker, or openbao once built)"
                    ));
                    None
                }
            });
        let url = url_raw.as_deref().and_then(|raw| {
            match Url::parse(raw.trim()) {
                Ok(url)
                    if matches!(url.scheme(), "http" | "https")
                        && url.host_str().is_some()
                        && url.query().is_none()
                        && url.fragment().is_none() =>
                {
                    Some(raw.trim().to_owned())
                }
                _ => {
                    problems.push(
                        "RUN_CERT_ISSUER_URL must be an absolute http(s) URL with a host, no query and no fragment"
                            .into(),
                    );
                    None
                }
            }
        });
        let token_file = parse_file(&get, "RUN_CERT_TOKEN_FILE", problems);
        let ca_file = parse_file(&get, "RUN_CERT_CA_FILE", problems);
        let mut fraction_ok = true;
        let renew_fraction = fraction_raw.as_deref().map_or(
            RenewFraction::TWO_THIRDS,
            |raw| match parse_renew_fraction(raw.trim()) {
                Some(fraction) => fraction,
                None => {
                    problems.push(format!(
                        "RUN_CERT_RENEW_FRACTION is invalid: {raw:?} (n/d with 0 < n/d < 1, e.g. 2/3)"
                    ));
                    fraction_ok = false;
                    RenewFraction::TWO_THIRDS
                }
            },
        );

        let (Some(issuer), Some(url), Some(token_file)) = (issuer, url, token_file) else {
            return None;
        };
        if !complete || !fraction_ok || (ca_raw.is_some() && ca_file.is_none()) {
            return None;
        }
        let ca_pem = match ca_file.as_deref() {
            Some(path) => match read_ca_file(path) {
                Ok(pem) => Some(pem),
                Err(why) => {
                    problems.push(format!("RUN_CERT_CA_FILE is unusable: {why}"));
                    return None;
                }
            },
            None => None,
        };
        let http = match adam_run_cert_broker::http_client(ca_pem.as_deref()) {
            Ok(http) => http,
            Err(why) => {
                problems.push(format!("RUN_CERT_CA_FILE is unusable: {why}"));
                return None;
            }
        };
        Some(Self {
            issuer,
            url,
            token_file,
            ca_file,
            renew_fraction,
            http,
        })
    }

    /// The issuer of `run`: one broker issuer, at the URL and with the token file this
    /// configuration names.
    ///
    /// # Errors
    ///
    /// [`BrokerError`] when the URL the configuration accepted cannot make a route.
    pub fn issuer(&self, run: adam_core::RunId) -> Result<DynRunCertIssuer, BrokerError> {
        let issuer =
            BrokerIssuer::with_client(&self.url, &self.token_file, run, self.http.clone())?;
        Ok(std::sync::Arc::new(issuer))
    }

    /// The keyring of `subject`'s run: an issuer per run, and the configured renewal fraction.
    ///
    /// # Errors
    ///
    /// [`BrokerError`] when the URL the configuration accepted cannot make a route.
    pub fn keyring(&self, subject: RunSubject) -> Result<RunKeyring, BrokerError> {
        Ok(RunKeyring::new(self.issuer(subject.run())?, subject)
            .with_renew_fraction(self.renew_fraction))
    }
}

impl std::fmt::Debug for RunCertSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunCertSettings")
            .field("issuer", &self.issuer)
            // The broker's address is a deployment secret: scheme and host only, as the model's.
            .field("url", &endpoint_for_logs(&self.url))
            .field("token_file", &self.token_file)
            .field("ca_file", &self.ca_file)
            .field("renew_fraction", &self.renew_fraction)
            .finish_non_exhaustive()
    }
}

/// `n/d`, both integers, strictly between 0 and 1.
fn parse_renew_fraction(raw: &str) -> Option<RenewFraction> {
    let (numerator, denominator) = raw.split_once('/')?;
    let numerator: u32 = numerator.trim().parse().ok()?;
    let denominator: u32 = denominator.trim().parse().ok()?;
    RenewFraction::new(numerator, denominator).ok()
}

/// The PEM of `path`, capped like the broker's client caps it.
fn read_ca_file(path: &std::path::Path) -> Result<Vec<u8>, String> {
    let size = std::fs::metadata(path)
        .map_err(|e| format!("it cannot be read: {e}"))?
        .len();
    if size > adam_run_cert_broker::MAX_CA_PEM_BYTES as u64 {
        return Err(format!(
            "it is longer than {} bytes",
            adam_run_cert_broker::MAX_CA_PEM_BYTES
        ));
    }
    std::fs::read(path).map_err(|e| format!("it cannot be read: {e}"))
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
    /// `MODEL_AUTH=injector`: no key; every call names its root run in `X-Run-Id` and carries no
    /// `Authorization` ([`adam_model::InjectedRun`], ADR 0067). `api_key` is empty then.
    pub injected_run: bool,
    /// `MODEL_CA_FILE`: certificates trusted for the model endpoint beside the default roots, read
    /// and checked at startup. `None` when unset.
    pub ca_roots: Option<ExtraRoots>,
    /// `RUN_CERT_*`: the run's certificate comes from a broker ([`RunCertSettings`], ADR 0086,
    /// A3). `None` when none of the variables is set: no run certificate, and the process turn
    /// keys sign every request. A half-set configuration is a startup problem (exit 78).
    pub run_cert: Option<RunCertSettings>,
    /// `TURN_SIGNING_KEY_FILE` (with `TURN_SIGNING_KEY_ID` and `TURN_VERIFY_KEYS_FILE`): the keys
    /// every model call is signed with ([`adam_model::SignedRequests`], ADR 0068 amended
    /// 2026-11-25), read and checked at startup. `None` when unset: no call is signed.
    pub turn_keys: Option<adam_core::turn::DynTurnKeys>,
    /// `MODEL_PARK_LIMIT`: how long parks on a timer in a row may last, from the first, before the
    /// run fails with the last reason (ADR 0067; `adam_llm_agent::ModelParking::limit`). 24 hours
    /// by default. A binary gives it to its agents with whether an unreachable endpoint parks,
    /// which is [`injected_run`](Self::injected_run).
    pub park_limit: Duration,
    /// `MODEL`: the model alias the gateway knows.
    pub alias: String,
    /// `MODEL_ALIASES_FILE`: the file of per-model profiles (ADR 0063, ADR 0064), **JSON only for
    /// now**. `None` when unset: no model has a profile, and nothing beyond the variables below
    /// changes. Read and checked at startup; a file that cannot be read or does not parse is a
    /// problem naming the file and the key.
    pub model_aliases_file: Option<std::path::PathBuf>,
    /// The profiles read from [`model_aliases_file`](Self::model_aliases_file), by model id; empty
    /// when the file is unset. [`client`](Self::client) gives them to the client, which applies a
    /// model's `reasoningWire` to its requests (ADR 0063 decision 6); the binaries also pass them
    /// to the assembly, which resolves each agent's default reasoning effort and output cap and
    /// refuses a level the model does not take (ADR 0063, ADR 0064).
    pub profiles: ModelProfiles,
    /// `MODEL_EXTRA_BODY`: members merged into every request body (a flag that makes the model
    /// emit its reasoning). Not secret, and never a member the runtime owns (`model`, `messages`,
    /// `tools`, `tool_choice`, `stream`). `None` when unset or `{}`.
    pub extra_body: Option<serde_json::Map<String, serde_json::Value>>,
    /// `MODEL_ECHO_REASONING`: the member name under which the reasoning of earlier turns is sent
    /// back; `None` (the default) sends none.
    pub echo_reasoning: Option<ReasoningField>,
    /// `DEFAULT_MODEL_CONTEXT_WINDOW` (or its deprecated alias `MODEL_CONTEXT_WINDOW`): the
    /// context window, in tokens, of the model [`alias`](Self::alias) names. The client says it for that alias only ([`OpenAiCompatible::with_context_window`]),
    /// and only when the alias's profile names no `contextWindow`, which wins (ADR 0088): an agent
    /// whose files name another alias has its own profile's window, or none. `None` when unset.
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
            .field("injected_run", &self.injected_run)
            .field("ca_roots", &self.ca_roots)
            .field("turn_keys", &self.turn_keys)
            .field("run_cert", &self.run_cert)
            .field("park_limit", &self.park_limit)
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
            // The path and the models the file declares, never a profile's values.
            .field("model_aliases_file", &self.model_aliases_file)
            .field(
                "profiles",
                &self.profiles.profiles.keys().collect::<Vec<_>>(),
            )
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
        let injected_run = is_injector(lookup);
        if injected_run {
            // The injector signs nothing in: a device-grant setting beside it is a contradiction.
            for name in [
                "MODEL_OAUTH_ISSUER",
                "MODEL_OAUTH_CLIENT_ID",
                "MODEL_OAUTH_SCOPE",
            ] {
                if not_blank(lookup, name).is_some() {
                    problems.push(format!(
                        "{name} is for MODEL_AUTH=device: unset it with MODEL_AUTH=injector"
                    ));
                }
            }
        }
        let keyless = device || injected_run;
        let api_key = match (lookup("MODEL_API_KEY"), keyless) {
            (Some(v), false) => v,
            (Some(v), true) if !v.is_empty() => {
                let auth = if injected_run { "injector" } else { "device" };
                problems.push(format!(
                    "MODEL_API_KEY and MODEL_AUTH={auth} are exclusive: unset MODEL_API_KEY or leave it empty"
                ));
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
        let model_aliases_file = parse_file(&get, "MODEL_ALIASES_FILE", problems);
        let profiles = parse_profiles(model_aliases_file.as_deref(), problems);
        let context_window = parse_default_context_window(&get, problems);
        // The window the client will answer for `MODEL` (ADR 0088): the profile's, else the variable's.
        let window = adam_model::resolve_context_window(profiles.profile(&alias), context_window);
        if warns_no_output_limit(model_aliases_file.as_deref(), &profiles, &alias) {
            tracing::warn!(
                model = %alias,
                fallback = DEFAULT_MAX_OUTPUT_TOKENS,
                window,
                "MODEL_ALIASES_FILE gives no maxOutputTokensLimit for this alias: its output cap \
                 is the 128,000 fallback, clamped to the context window when one is known \
                 (ADR 0064)"
            );
        } else if model_aliases_file.is_none() && !alias.is_empty() {
            tracing::info!(
                model = %alias,
                fallback = DEFAULT_MAX_OUTPUT_TOKENS,
                window,
                "no MODEL_ALIASES_FILE: every model's output cap is the 128,000 fallback, \
                 clamped to the context window when one is known (ADR 0064)"
            );
        }
        Self {
            base_url,
            api_key: SecretString::from(api_key),
            device_grant,
            injected_run,
            ca_roots: parse_ca_file(&get, problems),
            turn_keys: parse_turn_keys(&get, problems),
            run_cert: RunCertSettings::parse(lookup, problems),
            park_limit: parse_park_limit(&get("MODEL_PARK_LIMIT"), problems),
            alias,
            model_aliases_file,
            profiles,
            extra_body: parse_extra_body(&get("MODEL_EXTRA_BODY"), problems),
            echo_reasoning: parse_echo_reasoning(&get("MODEL_ECHO_REASONING"), problems),
            context_window,
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
        let mut credentials: Option<adam_model::DynCredentials> = None;
        if let Some(grant) = &self.device_grant {
            // The code is asked for now, so the person can sign in before the first run.
            grant.start();
            credentials = Some(std::sync::Arc::new(grant.clone()));
        }
        if self.injected_run {
            credentials = Some(std::sync::Arc::new(adam_model::InjectedRun::new()));
        }
        if let Some(keys) = &self.turn_keys {
            let inner = credentials.unwrap_or_else(|| {
                std::sync::Arc::new(adam_model::StaticBearer::new(self.api_key.clone()))
            });
            credentials = Some(std::sync::Arc::new(adam_model::SignedRequests::new(
                inner,
                keys.clone(),
            )));
        }
        if let Some(credentials) = credentials {
            client = client.with_credentials(credentials);
        }
        if let Some(roots) = &self.ca_roots {
            client = client.with_extra_roots(roots)?;
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
        client = client.with_profiles(self.profiles.clone());
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
        None | Some("key" | "injector") => return (false, None),
        Some("device") => {}
        Some(_) => {
            problems.push("MODEL_AUTH must be `key`, `device` or `injector`".into());
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

/// Whether `MODEL_AUTH` is `injector` (ADR 0067).
fn is_injector(lookup: &impl Fn(&str) -> Option<String>) -> bool {
    not_blank(lookup, "MODEL_AUTH").is_some_and(|auth| auth.eq_ignore_ascii_case("injector"))
}

/// `TURN_SIGNING_KEY_FILE`, `TURN_SIGNING_KEY_ID`, `TURN_VERIFY_KEYS_FILE`: the turn keys, `None`
/// when the key file is unset. A key file that cannot be read or is not an Ed25519 private key, a
/// bad id, a bad verify set, or an id or a set without a key file, is a problem naming the
/// variable, never a key.
fn parse_turn_keys(
    get: &impl Fn(&str) -> Option<String>,
    problems: &mut Vec<String>,
) -> Option<adam_core::turn::DynTurnKeys> {
    use adam_core::turn::{Ed25519TurnKeys, KeyId};
    let kid = get("TURN_SIGNING_KEY_ID");
    let verify = get("TURN_VERIFY_KEYS_FILE");
    let Some(key) = get("TURN_SIGNING_KEY_FILE") else {
        for (name, value) in [
            ("TURN_SIGNING_KEY_ID", &kid),
            ("TURN_VERIFY_KEYS_FILE", &verify),
        ] {
            if value.is_some() {
                problems.push(format!("{name} needs TURN_SIGNING_KEY_FILE"));
            }
        }
        return None;
    };
    let kid = match kid.map(|kid| KeyId::new(kid.trim())) {
        None => None,
        Some(Ok(kid)) => Some(kid),
        Some(Err(e)) => {
            problems.push(format!("TURN_SIGNING_KEY_ID: {e}"));
            return None;
        }
    };
    let verify = verify.map(|path| std::path::PathBuf::from(path.trim()));
    match Ed25519TurnKeys::from_files(std::path::Path::new(key.trim()), kid, verify.as_deref()) {
        Ok(keys) => Some(std::sync::Arc::new(keys)),
        Err(e) => {
            problems.push(format!("TURN_SIGNING_KEY_FILE: {e}"));
            None
        }
    }
}

/// `MODEL_CA_FILE`: the certificates of the PEM file it names, `None` when unset. A file that
/// cannot be read, is not PEM or holds no certificate the TLS backend accepts is a problem.
fn parse_ca_file(
    get: &impl Fn(&str) -> Option<String>,
    problems: &mut Vec<String>,
) -> Option<ExtraRoots> {
    let raw = get("MODEL_CA_FILE")?;
    let path = std::path::PathBuf::from(raw.trim());
    match ExtraRoots::read(&path) {
        Ok(roots) => Some(roots),
        Err(e) => {
            problems.push(format!(
                "MODEL_CA_FILE {:?}: {e}",
                path.display().to_string()
            ));
            None
        }
    }
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

/// `MODEL_ALIASES_FILE`: the per-model profiles (ADR 0063, ADR 0064), JSON only for now. Each
/// problem names the file and the profile or preset and key it is about; an unreadable file is one
/// problem, and an empty set is returned either way, so the rest of the configuration is still
/// reported.
fn parse_profiles(path: Option<&std::path::Path>, problems: &mut Vec<String>) -> ModelProfiles {
    const NAME: &str = "MODEL_ALIASES_FILE";
    let Some(path) = path else {
        return ModelProfiles::default();
    };
    let shown = path.display().to_string();
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => {
            problems.push(format!("{NAME} {shown:?} cannot be read: {}", e.kind()));
            return ModelProfiles::default();
        }
    };
    match ModelProfiles::parse_json(&text) {
        Ok(profiles) => profiles,
        Err(errors) => {
            for error in errors {
                problems.push(format!("{NAME} {shown:?}: {error}"));
            }
            ModelProfiles::default()
        }
    }
}

/// Whether the startup log warns that the alias has no `maxOutputTokensLimit` (ADR 0064): a
/// profiles file is set and the profile gives none (or does not exist). With no file the 128,000
/// fallback is logged at `info`, not as a warning.
fn warns_no_output_limit(
    model_aliases_file: Option<&std::path::Path>,
    profiles: &ModelProfiles,
    alias: &str,
) -> bool {
    model_aliases_file.is_some()
        && !alias.is_empty()
        && profiles
            .profile(alias)
            .and_then(|p| p.max_output_tokens_limit)
            .is_none()
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

/// The largest `DEFAULT_MODEL_CONTEXT_WINDOW`: the largest count a `usage/v1` report may carry
/// (2^53 - 1, the largest integer a JSON number keeps exactly), the same bound as a profile's
/// `contextWindow`.
pub use adam_model::MAX_CONTEXT_WINDOW;

/// The variable of the window of the model `MODEL` names when its profile names none (ADR 0088).
const DEFAULT_CONTEXT_WINDOW_VAR: &str = "DEFAULT_MODEL_CONTEXT_WINDOW";

/// The deprecated alias of [`DEFAULT_CONTEXT_WINDOW_VAR`], kept for one minor release (ADR 0088
/// decision 2).
const DEPRECATED_CONTEXT_WINDOW_VAR: &str = "MODEL_CONTEXT_WINDOW";

/// `DEFAULT_MODEL_CONTEXT_WINDOW`, else its deprecated alias `MODEL_CONTEXT_WINDOW` (ADR 0088
/// decision 2). The alias is read only when the new variable is unset, and warns once (this runs
/// once, at startup). Both set to different values is a problem naming both: two answers to one
/// question fail closed. Both set to the same value is that value, with the warning.
fn parse_default_context_window(
    get: &impl Fn(&str) -> Option<String>,
    problems: &mut Vec<String>,
) -> Option<u64> {
    let current = get(DEFAULT_CONTEXT_WINDOW_VAR);
    let Some(deprecated) = get(DEPRECATED_CONTEXT_WINDOW_VAR) else {
        return current
            .and_then(|raw| parse_context_window(DEFAULT_CONTEXT_WINDOW_VAR, &raw, problems));
    };
    tracing::warn!(
        "{DEPRECATED_CONTEXT_WINDOW_VAR} is deprecated: rename it {DEFAULT_CONTEXT_WINDOW_VAR} \
         (ADR 0088)"
    );
    let alias = parse_context_window(DEPRECATED_CONTEXT_WINDOW_VAR, &deprecated, problems);
    let Some(current) = current else {
        return alias;
    };
    let window = parse_context_window(DEFAULT_CONTEXT_WINDOW_VAR, &current, problems);
    if let (Some(window), Some(alias)) = (window, alias)
        && window != alias
    {
        problems.push(format!(
            "{DEFAULT_CONTEXT_WINDOW_VAR} and {DEPRECATED_CONTEXT_WINDOW_VAR} (its deprecated \
             alias) give different windows: set {DEFAULT_CONTEXT_WINDOW_VAR} alone"
        ));
        return None;
    }
    window
}

/// The variable `name` (`DEFAULT_MODEL_CONTEXT_WINDOW` or its alias): a whole number of tokens
/// from 1 to [`MAX_CONTEXT_WINDOW`], or a problem.
fn parse_context_window(name: &str, raw: &str, problems: &mut Vec<String>) -> Option<u64> {
    match raw.trim().parse::<u64>() {
        Ok(window) if (1..=MAX_CONTEXT_WINDOW).contains(&window) => Some(window),
        _ => {
            problems.push(format!(
                "{name} must be a whole number of tokens from 1 to {MAX_CONTEXT_WINDOW}, like 131072"
            ));
            None
        }
    }
}

/// The default of `MODEL_PARK_LIMIT`: 24 hours.
const MODEL_PARK_LIMIT_DEFAULT: Duration = Duration::from_secs(24 * 60 * 60);

/// The range of `MODEL_PARK_LIMIT`: a minute to thirty days.
const MODEL_PARK_LIMIT_RANGE: std::ops::RangeInclusive<Duration> =
    Duration::from_secs(60)..=Duration::from_secs(30 * 24 * 60 * 60);

/// `MODEL_PARK_LIMIT`: whole seconds, or a whole number with `s`, `m`, `h` or `d`, within
/// [`MODEL_PARK_LIMIT_RANGE`].
fn parse_park_limit(raw: &Option<String>, problems: &mut Vec<String>) -> Duration {
    let Some(raw) = raw.as_deref().map(str::trim) else {
        return MODEL_PARK_LIMIT_DEFAULT;
    };
    let (digits, unit) = match raw.find(|c: char| !c.is_ascii_digit()) {
        Some(at) => raw.split_at(at),
        None => (raw, "s"),
    };
    let scale = match unit {
        "s" => Some(1),
        "m" => Some(60),
        "h" => Some(3600),
        "d" => Some(86_400),
        _ => None,
    };
    let limit = scale
        .zip(digits.parse::<u64>().ok())
        .and_then(|(scale, n)| n.checked_mul(scale))
        .map(Duration::from_secs)
        .filter(|limit| MODEL_PARK_LIMIT_RANGE.contains(limit));
    limit.unwrap_or_else(|| {
        problems.push(
            "MODEL_PARK_LIMIT must be whole seconds or a whole number with `s`, `m`, `h` or `d`, from 60s to 30d, like 24h".into(),
        );
        MODEL_PARK_LIMIT_DEFAULT
    })
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
                max_tool_calls_in_flight: 4,
                max_model_calls: 2,
                model_tokens_per_minute: None,
                max_processes: 4,
                max_git_processes: 2,
                command_memory_max: adam_budget::default_command_memory(total_ram()),
                command_cpu_max: parse_command_cpu(&|_: &str| None, &mut Vec::new()),
                max_run_continuations: 3,
            })
        );
    }

    /// The process pool and a command's limits (ADR 0065 slice 3): read like the other pools, and a
    /// bad value is a problem naming the variable (exit 78), never a silent default.
    #[test]
    fn the_process_pool_and_command_limits_are_read_and_bounded() {
        let mut vars = full();
        vars.insert("MAX_PROCESSES", "8");
        vars.insert("MAX_GIT_PROCESSES", "3");
        vars.insert("COMMAND_MEMORY_MAX", "2GB");
        vars.insert("COMMAND_CPU_MAX", "1.5");
        let worker = parse(&vars).expect("valid").worker.expect("workers");
        assert_eq!(worker.max_processes, 8);
        assert_eq!(worker.max_git_processes, 3);
        assert_eq!(worker.command_memory_max, 2 * 1024 * 1024 * 1024);
        assert_eq!(worker.command_cpu_max, 1500);
        let budget = worker.budget();
        assert_eq!(budget.processes, 8);
        assert_eq!(budget.git, 3);
        assert_eq!(budget.command_memory, 2 * 1024 * 1024 * 1024);
        assert_eq!(budget.command_cpu, 1500);

        for (name, bad) in [
            ("MAX_PROCESSES", "0"),
            ("MAX_PROCESSES", "257"),
            ("MAX_PROCESSES", "many"),
            ("MAX_GIT_PROCESSES", "0"),
            ("MAX_GIT_PROCESSES", "65"),
            ("COMMAND_MEMORY_MAX", "1MB"),
            ("COMMAND_MEMORY_MAX", "lots"),
            ("COMMAND_CPU_MAX", "0"),
            ("COMMAND_CPU_MAX", "50m"),
            ("COMMAND_CPU_MAX", "fast"),
        ] {
            let mut vars = full();
            vars.insert(name, bad);
            assert!(mentions(&problems_of(&vars), name), "{name}={bad}");
        }
    }

    /// `DEFAULT_LOCALE` names the agent's default language: unset is none, a well-formed tag is
    /// kept, a malformed one is a startup problem (exit 78), never a guessed language.
    /// The pools of ADR 0065 slice 2: read from their variables, with a problem naming each one
    /// that is out of range, never silently clamped.
    #[test]
    fn the_pool_variables_are_read_and_bounded() {
        let c = parse(&full()).expect("valid");
        let worker = c.worker.expect("the role runs workers");
        assert_eq!(worker.max_tool_calls_in_flight, 4);
        assert_eq!(worker.max_model_calls, 2);
        assert_eq!(worker.model_tokens_per_minute, None, "unset meters none");
        let budget = worker.budget();
        assert_eq!(budget.runs, 4, "WORKERS is the run ceiling");
        assert_eq!(budget.tool_calls, 4);
        assert_eq!(budget.model_calls, 2);
        assert_eq!(budget.tokens_per_minute, None);

        let mut vars = full();
        vars.insert("MAX_TOOL_CALLS_IN_FLIGHT", "16");
        vars.insert("MAX_MODEL_CALLS", "8");
        vars.insert("MODEL_TOKENS_PER_MINUTE", "20000");
        let worker = parse(&vars).expect("valid").worker.expect("workers");
        assert_eq!(worker.max_tool_calls_in_flight, 16);
        assert_eq!(worker.max_model_calls, 8);
        assert_eq!(worker.model_tokens_per_minute, Some(20_000));
        assert_eq!(worker.budget().tokens_per_minute, Some(20_000));

        for (name, bad) in [
            ("MAX_TOOL_CALLS_IN_FLIGHT", "0"),
            ("MAX_TOOL_CALLS_IN_FLIGHT", "257"),
            ("MAX_TOOL_CALLS_IN_FLIGHT", "many"),
            ("MAX_MODEL_CALLS", "0"),
            ("MAX_MODEL_CALLS", "65"),
            ("MAX_RUN_CONTINUATIONS", "21"),
            ("MAX_RUN_CONTINUATIONS", "-1"),
            ("MODEL_TOKENS_PER_MINUTE", "0"),
            ("MODEL_TOKENS_PER_MINUTE", "100000001"),
            ("MODEL_TOKENS_PER_MINUTE", "lots"),
        ] {
            let mut vars = full();
            vars.insert(name, bad);
            assert!(mentions(&problems_of(&vars), name), "{name}={bad}");
        }
        let mut vars = full();
        vars.insert("MAX_RUN_CONTINUATIONS", "0");
        let worker = parse(&vars).expect("valid").worker.expect("workers");
        assert_eq!(
            worker.max_run_continuations, 0,
            "0 never lets a hook keep a run going"
        );
        // A blank rate meters none, as an unset one does.
        let mut vars = full();
        vars.insert("MODEL_TOKENS_PER_MINUTE", "  ");
        assert_eq!(
            parse(&vars)
                .expect("valid")
                .worker
                .expect("workers")
                .model_tokens_per_minute,
            None
        );
    }

    #[test]
    fn uploads_inline_max_bytes_is_a_size_in_range_or_a_startup_problem() {
        assert!(
            parse(&full())
                .expect("valid")
                .uploads_inline_max_bytes
                .is_none()
        );
        let mut vars = full();
        vars.insert("UPLOADS_INLINE_MAX_BYTES", "1048576");
        assert_eq!(
            parse(&vars).expect("valid").uploads_inline_max_bytes,
            Some(1_048_576)
        );
        // The ceiling itself is a threshold; one byte past it is not.
        vars.insert("UPLOADS_INLINE_MAX_BYTES", "268435456");
        assert_eq!(
            parse(&vars).expect("valid").uploads_inline_max_bytes,
            Some(268_435_456)
        );
        for bad in ["0", "-1", "ten", "1.5", "268435457"] {
            let mut vars = full();
            vars.insert("UPLOADS_INLINE_MAX_BYTES", bad);
            assert!(
                mentions(&problems_of(&vars), "UPLOADS_INLINE_MAX_BYTES"),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn default_locale_is_a_well_formed_tag_or_a_startup_problem() {
        assert!(parse(&full()).expect("valid").default_locale.is_none());
        let mut vars = full();
        vars.insert("DEFAULT_LOCALE", "fr-FR");
        let c = parse(&vars).expect("valid");
        assert_eq!(c.default_locale.expect("a tag").as_str(), "fr-FR");
        for bad in ["fr_CM", "not a tag", "x-private", "toolongsubtagxx"] {
            let mut vars = full();
            vars.insert("DEFAULT_LOCALE", bad);
            assert!(mentions(&problems_of(&vars), "DEFAULT_LOCALE"), "{bad:?}");
        }
    }

    /// `prompt-statement/v1` is listed exactly when the model calls send the headers: through the
    /// injector (ADR 0077), or signed with a turn key (ADR 0068 amended 2026-11-25).
    #[test]
    fn the_prompt_statement_follows_model_auth_injector() {
        assert!(!parse(&full()).expect("valid").prompt_statement);
        for (auth, listed) in [("injector", true), ("Injector", true), ("key", false)] {
            let mut vars = full();
            vars.insert("MODEL_AUTH", auth);
            assert_eq!(
                parse(&vars).expect("valid").prompt_statement,
                listed,
                "{auth}"
            );
        }
        let dir =
            std::env::temp_dir().join(format!("adam-service-card-key-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = dir.join("turn.pem");
        std::fs::write(
            &key,
            "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIJ1hsZ3v/VpguoRK9JLsLMREScVpezJpGXA7rAMcrn9g\n-----END PRIVATE KEY-----\n",
        )
        .unwrap();
        let key: &'static str = Box::leak(key.to_str().unwrap().to_owned().into_boxed_str());
        let mut vars = full();
        vars.insert("TURN_SIGNING_KEY_FILE", key);
        assert!(parse(&vars).expect("valid").prompt_statement, "a turn key");
        std::fs::remove_dir_all(&dir).ok();
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

    /// Every role reads `MAX_PARALLEL_TOOL_CALLS` for the bounds of a person's `start.config`
    /// (ADR 0054, ANR-1093), so a control plane holds a `config` to what the workers' session
    /// tools do. The roles that run workers still refuse a bad value; a control plane takes it as
    /// the default and starts, as before.
    #[test]
    fn every_role_reads_max_parallel_for_the_session_bounds() {
        for role in ["all", "control-plane", "worker"] {
            let read = |value: Option<&'static str>| {
                let mut vars = with_role(role);
                if let Some(value) = value {
                    vars.insert("MAX_PARALLEL_TOOL_CALLS", value);
                }
                parse(&vars).map(|c| c.sessions.max_parallel_tool_calls)
            };
            assert_eq!(read(None).unwrap(), 10, "{role}");
            assert_eq!(read(Some(" 4 ")).unwrap(), 4, "{role}");
            assert_eq!(read(Some("32")).unwrap(), 32, "{role}");
            for bad in ["0", "33", "x"] {
                match (role, read(Some(bad))) {
                    ("control-plane", Ok(n)) => {
                        assert_eq!(n, 10, "{role}: {bad} is the default");
                    }
                    (_, Err(error)) => assert_eq!(
                        error
                            .problems
                            .iter()
                            .filter(|p| p.starts_with("MAX_PARALLEL_TOOL_CALLS"))
                            .count(),
                        1,
                        "{role}: {bad} is one problem, not two: {error:?}"
                    ),
                    (_, Ok(n)) => panic!("{role}: {bad} was read as {n}"),
                }
            }
        }
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
    fn the_activity_listener_is_off_unless_its_address_is_set() {
        let c = parse(&full()).expect("valid");
        assert!(c.activity.is_none(), "{:?}", c.activity);
        let mut vars = full();
        vars.insert("ACTIVITY_LISTEN_ADDR", "  ");
        assert!(parse(&vars).expect("blank is unset").activity.is_none());
    }

    #[test]
    fn the_activity_listener_reads_its_address_and_its_own_tokens_on_every_role() {
        for role in ["all", "control-plane", "worker"] {
            let mut vars = with_role(role);
            vars.insert("ACTIVITY_LISTEN_ADDR", "0.0.0.0:8081");
            vars.insert("ACTIVITY_BEARER_TOKENS", " act-1 ,, act-2 ");
            let c = parse(&vars).unwrap_or_else(|e| panic!("{role}: {e}"));
            let activity = c.activity.as_ref().expect("on");
            assert_eq!(activity.listen_addr, "0.0.0.0:8081".parse().unwrap());
            let tokens: Vec<_> = activity
                .bearer_tokens
                .iter()
                .map(|t| t.expose_secret().to_owned())
                .collect();
            assert_eq!(tokens, ["act-1", "act-2"], "{role}");
            // Debug says how many, never which.
            let debug = format!("{c:?}");
            assert!(
                !debug.contains("act-1") && !debug.contains("act-2"),
                "{debug}"
            );
        }
    }

    /// Fail closed: an activity listener with no token of its own is a startup error (exit 78), it
    /// never falls back to the A2A tokens or to serving anyone.
    #[test]
    fn an_activity_address_without_its_own_tokens_is_refused() {
        for role in ["all", "control-plane", "worker"] {
            let mut vars = with_role(role);
            vars.insert("ACTIVITY_LISTEN_ADDR", "0.0.0.0:8081");
            let problems = problems_of(&vars);
            let problem = problems
                .iter()
                .find(|p| p.starts_with("ACTIVITY_BEARER_TOKENS is required"))
                .unwrap_or_else(|| panic!("{role}: {problems:?}"));
            assert!(problem.contains("fail closed"), "{problem}");
            vars.insert("ACTIVITY_BEARER_TOKENS", " , ");
            assert!(
                problems_of(&vars)
                    .iter()
                    .any(|p| p == "ACTIVITY_BEARER_TOKENS has no usable token"),
                "{role}"
            );
        }
        let mut vars = full();
        vars.insert("ACTIVITY_LISTEN_ADDR", "0.0.0.0:8081");
        let error = parse(&vars).unwrap_err();
        assert_eq!(crate::exit_code(&error), 78);
    }

    #[test]
    fn activity_tokens_without_an_address_are_refused() {
        let mut vars = full();
        vars.insert("ACTIVITY_BEARER_TOKENS", "act-1");
        let problems = problems_of(&vars);
        assert!(
            problems
                .iter()
                .any(|p| p
                    .starts_with("ACTIVITY_BEARER_TOKENS is set and ACTIVITY_LISTEN_ADDR is not")),
            "{problems:?}"
        );
    }

    /// A token in both lists would open A2A to the caller of the activity route (and the reverse):
    /// refused, without saying which token.
    #[test]
    fn an_activity_token_that_is_also_an_a2a_token_is_refused() {
        let mut vars = full();
        vars.insert("ACTIVITY_LISTEN_ADDR", "0.0.0.0:8081");
        vars.insert("ACTIVITY_BEARER_TOKENS", "act-1,two");
        let problems = problems_of(&vars);
        let problem = problems
            .iter()
            .find(|p| p.starts_with("ACTIVITY_BEARER_TOKENS and A2A_BEARER_TOKENS share a token"))
            .unwrap_or_else(|| panic!("{problems:?}"));
        assert!(
            !problem.contains("two") && !problem.contains("act-1"),
            "{problem}"
        );
    }

    #[test]
    fn the_activity_address_must_be_an_address_on_another_port() {
        for (listen, activity) in [
            ("0.0.0.0:8080", "0.0.0.0:8080"),
            ("0.0.0.0:8080", "127.0.0.1:8080"),
            ("127.0.0.1:8080", "0.0.0.0:8080"),
            ("[::]:8080", "127.0.0.1:8080"),
        ] {
            let mut vars = full();
            vars.insert("LISTEN_ADDR", listen);
            vars.insert("ACTIVITY_LISTEN_ADDR", activity);
            vars.insert("ACTIVITY_BEARER_TOKENS", "act-1");
            assert!(
                problems_of(&vars)
                    .iter()
                    .any(|p| p.starts_with("ACTIVITY_LISTEN_ADDR") && p.contains("LISTEN_ADDR")),
                "{listen} {activity}"
            );
        }
        // Another port, another address on the same port, and port 0 (a free port each) are fine.
        for (listen, activity) in [
            ("0.0.0.0:8080", "0.0.0.0:8081"),
            ("127.0.0.1:8080", "127.0.0.2:8080"),
            ("127.0.0.1:0", "127.0.0.1:0"),
        ] {
            let mut vars = full();
            vars.insert("LISTEN_ADDR", listen);
            vars.insert("ACTIVITY_LISTEN_ADDR", activity);
            vars.insert("ACTIVITY_BEARER_TOKENS", "act-1");
            assert!(parse(&vars).is_ok(), "{listen} {activity}");
        }
        let mut vars = full();
        vars.insert("ACTIVITY_LISTEN_ADDR", "everywhere");
        vars.insert("ACTIVITY_BEARER_TOKENS", "act-1");
        assert!(mentions(
            &problems_of(&vars),
            "ACTIVITY_LISTEN_ADDR is invalid"
        ));
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
    fn the_no_output_limit_warning_needs_a_file_and_a_missing_limit() {
        use std::collections::BTreeMap;

        use adam_model::ModelProfile;

        let alias = "large";
        let file = std::path::Path::new("/etc/adam/model-profiles.json");
        let empty = ModelProfiles::default();
        let no_limit = ModelProfiles {
            profiles: BTreeMap::from([(
                alias.to_owned(),
                ModelProfile {
                    max_output_tokens: Some(64_000),
                    ..ModelProfile::default()
                },
            )]),
            presets: BTreeMap::new(),
        };
        let limited = ModelProfiles {
            profiles: BTreeMap::from([(
                alias.to_owned(),
                ModelProfile {
                    max_output_tokens_limit: Some(131_072),
                    ..ModelProfile::default()
                },
            )]),
            presets: BTreeMap::new(),
        };
        assert!(
            warns_no_output_limit(Some(file), &empty, alias),
            "a file with no profile for the alias"
        );
        assert!(
            warns_no_output_limit(Some(file), &no_limit, alias),
            "a profile with no limit"
        );
        assert!(
            !warns_no_output_limit(Some(file), &limited, alias),
            "a declared limit"
        );
        assert!(
            !warns_no_output_limit(None, &empty, alias),
            "no file is an info, not a warning"
        );
        assert!(
            !warns_no_output_limit(Some(file), &empty, ""),
            "an unset MODEL is not warned about"
        );
    }

    #[test]
    fn the_profiles_file_is_read_and_a_bad_one_names_the_file_and_the_key() {
        let dir =
            std::env::temp_dir().join(format!("adam-service-profiles-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("model-profiles.json");
        // The test's `Vars` holds `&'static str`: leak this one path for the test's lifetime.
        let shown: &'static str = Box::leak(file.display().to_string().into_boxed_str());
        let run = |text: Option<&str>| {
            let mut vars = model_vars();
            if let Some(text) = text {
                std::fs::write(&file, text).unwrap();
                vars.insert("MODEL_ALIASES_FILE", shown);
            }
            model_problems(&vars)
        };

        // Unset (or blank): no file, no profile.
        let (model, problems) = run(None);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(model.model_aliases_file, None);
        assert!(model.profiles.is_empty());
        let mut vars = model_vars();
        vars.insert("MODEL_ALIASES_FILE", "  ");
        let (model, problems) = model_problems(&vars);
        assert!(problems.is_empty(), "{problems:?}");
        assert!(model.profiles.is_empty());

        // The ADR 0064 example: read, validated, and the path and the keys (never a value) in `Debug`.
        let good = r#"{
            "presets": { "quick": { "reasoningEffort": "low" } },
            "profiles": {
                "large-coder": {
                    "preset": "quick",
                    "reasoningEfforts": ["low", "medium", "high", "max"],
                    "reasoningWire": "reasoning_effort",
                    "maxOutputTokensLimit": 131072,
                    "contextWindow": 1000000
                },
                "small-chat": { "reasoningEfforts": [], "maxOutputTokensLimit": 65536 }
            }
        }"#;
        let (model, problems) = run(Some(good));
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(model.model_aliases_file.as_deref(), Some(file.as_path()));
        assert_eq!(model.profiles.profiles.len(), 2);
        assert_eq!(
            model
                .profiles
                .profile("large-coder")
                .and_then(|p| p.reasoning_efforts.as_ref())
                .map(Vec::len),
            Some(4)
        );
        let debug = format!("{model:?}");
        assert!(debug.contains(shown), "{debug}");
        assert!(
            debug.contains("large-coder") && debug.contains("small-chat"),
            "{debug}"
        );
        assert!(!debug.contains("131072"), "{debug}");
        // The client is given the profiles: it applies a model's wire and answers `profile`.
        use adam_model::ModelClient as _;
        let client = model.client().expect("a usable client");
        assert_eq!(
            client.profile("large-coder").and_then(|p| p.reasoning_wire),
            Some(adam_model::ReasoningWire::ReasoningEffort)
        );
        assert!(client.profile("small").is_none(), "an undeclared alias");

        // An unknown level names the file, the profile and the key, and the run is refused.
        let (model, problems) = run(Some(
            r#"{"profiles": {"gpt-x": {"reasoningEfforts": ["ultra"], "reasoningWire": "reasoning_effort"}}}"#,
        ));
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].starts_with("MODEL_ALIASES_FILE"),
            "{problems:?}"
        );
        assert!(problems[0].contains(shown), "{problems:?}");
        assert!(problems[0].contains("gpt-x"), "{problems:?}");
        assert!(problems[0].contains("reasoningEfforts"), "{problems:?}");
        assert!(model.profiles.is_empty(), "a refused file leaves nothing");

        // A file that is not JSON, and a path that is not a file.
        let (_, problems) = run(Some("{oops"));
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("not valid JSON"), "{problems:?}");
        let mut vars = model_vars();
        vars.insert("MODEL_ALIASES_FILE", "/definitely/not/here.json");
        let (model, problems) = model_problems(&vars);
        assert!(mentions(&problems, "MODEL_ALIASES_FILE"), "{problems:?}");
        assert!(model.profiles.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
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
            vars.insert("DEFAULT_MODEL_CONTEXT_WINDOW", value);
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
        vars.insert("DEFAULT_MODEL_CONTEXT_WINDOW", "  ");
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
            // The new variable and its deprecated alias are held to the same bound.
            for name in ["DEFAULT_MODEL_CONTEXT_WINDOW", "MODEL_CONTEXT_WINDOW"] {
                let mut vars = model_vars();
                vars.insert(name, bad);
                let (model, problems) = model_problems(&vars);
                assert_eq!(problems.len(), 1, "{name}={bad}: {problems:?}");
                assert!(problems[0].starts_with(name), "{problems:?}");
                assert_eq!(model.context_window, None, "{name}={bad}");
            }
        }
    }

    /// The `warn`-and-above logs of the current thread while `f` runs.
    fn warnings_of<T>(f: impl FnOnce() -> T) -> (T, String) {
        #[derive(Clone, Default)]
        struct Sink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Sink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
            type Writer = Sink;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }
        let sink = Sink::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(sink.clone())
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .finish();
        let out = tracing::subscriber::with_default(subscriber, f);
        let logs = String::from_utf8_lossy(&sink.0.lock().unwrap()).into_owned();
        (out, logs)
    }

    /// ADR 0088 decision 2: `MODEL_CONTEXT_WINDOW` is the deprecated alias of
    /// `DEFAULT_MODEL_CONTEXT_WINDOW`: read only when that is unset, with one warning that names
    /// the new variable; both set agree or are a problem naming both, never their values.
    #[test]
    fn model_context_window_is_a_deprecated_alias_of_default_model_context_window() {
        use adam_model::ModelClient as _;
        const DEPRECATION: &str =
            "MODEL_CONTEXT_WINDOW is deprecated: rename it DEFAULT_MODEL_CONTEXT_WINDOW";
        let with = |pairs: &[(&'static str, &'static str)]| {
            let mut vars = model_vars();
            vars.extend(pairs.iter().copied());
            warnings_of(|| model_problems(&vars))
        };

        // The new variable alone: its value, no warning.
        let ((model, problems), logs) = with(&[("DEFAULT_MODEL_CONTEXT_WINDOW", "131072")]);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(model.context_window, Some(131_072));
        assert!(!logs.contains("deprecated"), "{logs}");

        // The alias alone still works: its value, for the alias `MODEL` names, and one warning.
        let ((model, problems), logs) = with(&[("MODEL_CONTEXT_WINDOW", "65536")]);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(model.context_window, Some(65_536));
        assert_eq!(logs.matches(DEPRECATION).count(), 1, "{logs}");
        let client = model.client().expect("a usable client");
        assert_eq!(client.context_window("large"), Some(65_536));
        assert_eq!(client.context_window("small"), None);

        // Both, equal (as numbers): the value, and the warning.
        let ((model, problems), logs) = with(&[
            ("DEFAULT_MODEL_CONTEXT_WINDOW", "131072"),
            ("MODEL_CONTEXT_WINDOW", " 131072 "),
        ]);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(model.context_window, Some(131_072));
        assert_eq!(logs.matches(DEPRECATION).count(), 1, "{logs}");

        // Both, different: one problem naming both variables and neither value, and no window.
        let ((model, problems), _) = with(&[
            ("DEFAULT_MODEL_CONTEXT_WINDOW", "131072"),
            ("MODEL_CONTEXT_WINDOW", "65536"),
        ]);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            mentions(&problems, "DEFAULT_MODEL_CONTEXT_WINDOW"),
            "{problems:?}"
        );
        assert!(
            problems[0].contains(" MODEL_CONTEXT_WINDOW"),
            "{problems:?}"
        );
        assert!(
            !problems[0].contains("131072") && !problems[0].contains("65536"),
            "{problems:?}"
        );
        assert_eq!(model.context_window, None);

        // Both, one of them invalid: that one's problem alone, not a conflict.
        let ((_, problems), _) = with(&[
            ("DEFAULT_MODEL_CONTEXT_WINDOW", "128k"),
            ("MODEL_CONTEXT_WINDOW", "65536"),
        ]);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].starts_with("DEFAULT_MODEL_CONTEXT_WINDOW must be"),
            "{problems:?}"
        );
        let ((_, problems), _) = with(&[
            ("DEFAULT_MODEL_CONTEXT_WINDOW", "131072"),
            ("MODEL_CONTEXT_WINDOW", "0"),
        ]);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].starts_with("MODEL_CONTEXT_WINDOW must be"),
            "{problems:?}"
        );

        // A blank alias is unset: no warning.
        let ((model, problems), logs) = with(&[
            ("DEFAULT_MODEL_CONTEXT_WINDOW", "8192"),
            ("MODEL_CONTEXT_WINDOW", "  "),
        ]);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(model.context_window, Some(8192));
        assert!(!logs.contains("deprecated"), "{logs}");
    }

    /// ADR 0088 decision 1: a profile's `contextWindow` is its alias's window, over
    /// `DEFAULT_MODEL_CONTEXT_WINDOW`, and an alias `MODEL` does not name (a subagent's) has its own.
    #[test]
    fn a_profiles_context_window_wins_over_default_model_context_window() {
        use adam_model::ModelClient as _;
        let dir = std::env::temp_dir().join(format!("adam-service-windows-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("model-profiles.json");
        let shown: &'static str = Box::leak(file.display().to_string().into_boxed_str());
        let client_with = |profiles: &str| {
            std::fs::write(&file, profiles).unwrap();
            let mut vars = model_vars();
            vars.insert("MODEL_ALIASES_FILE", shown);
            vars.insert("DEFAULT_MODEL_CONTEXT_WINDOW", "131072");
            let (model, problems) = model_problems(&vars);
            assert!(problems.is_empty(), "{problems:?}");
            model.client().expect("a usable client")
        };

        let client = client_with(
            r#"{"profiles": {"large": {"contextWindow": 400000}, "small": {"contextWindow": 64000}}}"#,
        );
        assert_eq!(
            client.context_window("large"),
            Some(400_000),
            "the profile wins"
        );
        assert_eq!(
            client.context_window("small"),
            Some(64_000),
            "another alias"
        );
        assert_eq!(client.context_window("other"), None, "nobody said");

        // A profile without a window leaves `DEFAULT_MODEL_CONTEXT_WINDOW` for its alias.
        let client = client_with(r#"{"profiles": {"large": {}}}"#);
        assert_eq!(client.context_window("large"), Some(131_072));

        // A window the loader refuses names the file, the profile and the key.
        std::fs::write(&file, r#"{"profiles": {"large": {"contextWindow": 0}}}"#).unwrap();
        let mut vars = model_vars();
        vars.insert("MODEL_ALIASES_FILE", shown);
        let (_, problems) = model_problems(&vars);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains(shown), "{problems:?}");
        assert!(problems[0].contains("contextWindow"), "{problems:?}");
        std::fs::remove_dir_all(&dir).unwrap();
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
    fn model_park_limit_is_a_day_by_default_and_a_minute_to_thirty_days() {
        let mut vars = model_vars();
        assert_eq!(
            model_problems(&vars).0.park_limit,
            Duration::from_secs(86_400)
        );
        for (raw, secs) in [
            ("3600", 3600),
            ("90m", 5400),
            ("2h", 7200),
            ("60s", 60),
            ("30d", 30 * 86_400),
            (" 12h ", 43_200),
        ] {
            vars.insert("MODEL_PARK_LIMIT", raw);
            let (model, problems) = model_problems(&vars);
            assert!(problems.is_empty(), "{raw}: {problems:?}");
            assert_eq!(model.park_limit, Duration::from_secs(secs), "{raw}");
        }
        for bad in [
            "59",
            "31d",
            "0",
            "1w",
            "-5m",
            "h",
            "1.5h",
            "24 h",
            "99999999999999999999d",
        ] {
            vars.insert("MODEL_PARK_LIMIT", bad);
            assert!(
                mentions(&model_problems(&vars).1, "MODEL_PARK_LIMIT"),
                "{bad} is refused"
            );
        }
    }

    /// `TURN_SIGNING_KEY_FILE`: a readable Ed25519 key turns request signing on, with its id;
    /// anything else is a problem naming the variable and never the key. Unset: nothing is signed.
    #[test]
    fn the_turn_signing_key_turns_on_request_signing() {
        // RFC 8032 §7.1 test 1's seed, as `openssl genpkey -algorithm ed25519` writes it.
        const PEM: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIJ1hsZ3v/VpguoRK9JLsLMREScVpezJpGXA7rAMcrn9g\n-----END PRIVATE KEY-----\n";
        let dir =
            std::env::temp_dir().join(format!("adam-service-turn-key-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = dir.join("turn.pem");
        std::fs::write(&key, PEM).unwrap();
        let not_a_key = dir.join("not.pem");
        std::fs::write(&not_a_key, "hello").unwrap();
        // The variables are `&'static str`: a test may leak two paths.
        let key: &'static str = Box::leak(key.to_str().unwrap().to_owned().into_boxed_str());
        let not_a_key: &'static str =
            Box::leak(not_a_key.to_str().unwrap().to_owned().into_boxed_str());

        let (model, problems) = model_problems(&model_vars());
        assert!(problems.is_empty(), "{problems:?}");
        assert!(model.turn_keys.is_none());

        let mut vars = model_vars();
        vars.insert("TURN_SIGNING_KEY_FILE", key);
        vars.insert("TURN_SIGNING_KEY_ID", "vault:turn-key#v1");
        let (model, problems) = model_problems(&vars);
        assert!(problems.is_empty(), "{problems:?}");
        let keys = model.turn_keys.clone().expect("signing is on");
        let current = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(keys.current())
            .unwrap();
        assert_eq!(current.kid().as_str(), "vault:turn-key#v1");
        assert!(!format!("{model:?}").contains("MC4C"));
        model.client().expect("a usable client");

        for (name, value, problem) in [
            ("TURN_SIGNING_KEY_FILE", not_a_key, "TURN_SIGNING_KEY_FILE"),
            ("TURN_SIGNING_KEY_ID", "has space", "TURN_SIGNING_KEY_ID"),
        ] {
            let mut bad = vars.clone();
            bad.insert(name, value);
            let problems = model_problems(&bad).1;
            assert!(mentions(&problems, problem), "{name}: {problems:?}");
            assert!(!problems.iter().any(|p| p.contains("MC4C")));
        }
        let mut orphan = model_vars();
        orphan.insert("TURN_SIGNING_KEY_ID", "k");
        assert!(mentions(
            &model_problems(&orphan).1,
            "TURN_SIGNING_KEY_ID needs TURN_SIGNING_KEY_FILE"
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A token file for the `RUN_CERT_*` tests: the path, leaked because `Vars` holds `&'static str`,
    /// and the directory to remove.
    fn run_cert_token(name: &str) -> (&'static str, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "adam-service-run-cert-{name}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let token = dir.join("token");
        std::fs::write(&token, "a-projected-token\n").unwrap();
        let path: &'static str = Box::leak(token.display().to_string().into_boxed_str());
        (path, dir)
    }

    fn run_cert_complete(token: &'static str) -> Vars {
        let mut vars = model_vars();
        vars.insert("RUN_CERT_ISSUER", "broker");
        vars.insert("RUN_CERT_ISSUER_URL", "https://broker.internal:8443");
        vars.insert("RUN_CERT_TOKEN_FILE", token);
        vars
    }

    #[test]
    fn run_certificates_are_off_until_every_required_variable_is_set() {
        let (model, problems) = model_problems(&model_vars());
        assert!(problems.is_empty(), "{problems:?}");
        assert!(model.run_cert.is_none(), "off when none is set");
    }

    #[test]
    fn a_half_set_run_certificate_names_the_variable_and_is_78() {
        let (token, dir) = run_cert_token("half-set");
        for missing in [
            "RUN_CERT_ISSUER",
            "RUN_CERT_ISSUER_URL",
            "RUN_CERT_TOKEN_FILE",
        ] {
            let mut vars = run_cert_complete(token);
            vars.remove(missing);
            let (model, problems) = model_problems(&vars);
            assert!(
                mentions(&problems, missing),
                "{missing} is not named: {problems:?}"
            );
            assert!(model.run_cert.is_none(), "{missing}");
            let error = ConfigError::check(problems).unwrap_err();
            assert_eq!(crate::exit_code(&error), 78, "{missing}");
        }
        // The optional two alone are half a configuration too, and the three required are named.
        for (name, value) in [
            ("RUN_CERT_CA_FILE", token),
            ("RUN_CERT_RENEW_FRACTION", "2/3"),
        ] {
            let mut vars = model_vars();
            vars.insert(name, value);
            let (model, problems) = model_problems(&vars);
            for required in [
                "RUN_CERT_ISSUER",
                "RUN_CERT_ISSUER_URL",
                "RUN_CERT_TOKEN_FILE",
            ] {
                assert!(
                    mentions(&problems, required),
                    "{name}: {required} is not named: {problems:?}"
                );
            }
            assert!(model.run_cert.is_none(), "{name}");
            assert_eq!(
                crate::exit_code(&ConfigError::check(problems).unwrap_err()),
                78,
                "{name}"
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_complete_run_certificate_builds_an_issuer_and_a_keyring_and_hides_the_token() {
        let (token, dir) = run_cert_token("complete");
        let vars = run_cert_complete(token);
        let (model, problems) = model_problems(&vars);
        assert!(problems.is_empty(), "{problems:?}");
        let settings = model.run_cert.as_ref().expect("run certificates are on");
        assert_eq!(settings.issuer, RunCertIssuerKind::Broker);
        assert_eq!(settings.renew_fraction, RenewFraction::TWO_THIRDS);
        assert_eq!(settings.token_file, std::path::PathBuf::from(token));

        let run = adam_core::RunId::new();
        let issuer = settings.issuer(run).expect("the broker issuer");
        assert_eq!(issuer.kind(), "broker");
        assert_eq!(format!("{issuer:?}"), "RunCertIssuer(broker)");
        let keyring = settings
            .keyring(adam_core::turn::RunSubject::new("st-abc", run).unwrap())
            .expect("the run's keyring");
        assert_eq!(keyring.renew_fraction(), RenewFraction::TWO_THIRDS);

        // The token is in the file and in none of the renderings.
        assert!(
            std::fs::read_to_string(token)
                .unwrap()
                .contains("a-projected-token")
        );
        for shown in [format!("{settings:?}"), format!("{model:?}")] {
            assert!(shown.contains("broker.internal"), "{shown}");
            assert!(!shown.contains("a-projected-token"), "{shown}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_run_certificate_issuer_is_a_closed_set() {
        let (token, dir) = run_cert_token("closed-set");
        for (value, problem) in [
            ("openbao", "RUN_CERT_ISSUER=openbao is not built yet"),
            ("vault", "RUN_CERT_ISSUER is invalid"),
        ] {
            let mut vars = run_cert_complete(token);
            vars.insert("RUN_CERT_ISSUER", value);
            let (model, problems) = model_problems(&vars);
            assert!(
                problems.iter().any(|p| p.starts_with(problem)),
                "{value}: {problems:?}"
            );
            assert!(model.run_cert.is_none(), "{value}");
            assert_eq!(
                crate::exit_code(&ConfigError::check(problems).unwrap_err()),
                78,
                "{value}"
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_bad_run_certificate_setting_names_its_variable() {
        let (token, dir) = run_cert_token("bad-settings");
        let ca = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../adam-model-openai/tests/fixtures/test-ca.pem");
        let ca: &'static str = Box::leak(ca.display().to_string().into_boxed_str());
        let missing = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../adam-model-openai/tests/fixtures/not-here.pem");
        let missing: &'static str = Box::leak(missing.display().to_string().into_boxed_str());

        for (name, value) in [
            ("RUN_CERT_ISSUER_URL", "ftp://broker.internal"),
            ("RUN_CERT_ISSUER_URL", "broker.internal"),
            ("RUN_CERT_ISSUER_URL", "https://broker.internal/?tenant=1"),
            ("RUN_CERT_TOKEN_FILE", missing),
            ("RUN_CERT_CA_FILE", missing),
            ("RUN_CERT_CA_FILE", token),
            ("RUN_CERT_RENEW_FRACTION", "0/1"),
            ("RUN_CERT_RENEW_FRACTION", "1/1"),
            ("RUN_CERT_RENEW_FRACTION", "2/1"),
            ("RUN_CERT_RENEW_FRACTION", "2/0"),
            ("RUN_CERT_RENEW_FRACTION", "2"),
            ("RUN_CERT_RENEW_FRACTION", "-1/2"),
        ] {
            let mut vars = run_cert_complete(token);
            vars.insert(name, value);
            let (model, problems) = model_problems(&vars);
            assert!(mentions(&problems, name), "{name}={value}: {problems:?}");
            assert!(model.run_cert.is_none(), "{name}={value}");
            assert_eq!(
                crate::exit_code(&ConfigError::check(problems).unwrap_err()),
                78,
                "{name}={value}"
            );
        }

        // A good CA and fraction are read, and the fraction reaches the keyring.
        let mut vars = run_cert_complete(token);
        vars.insert("RUN_CERT_CA_FILE", ca);
        vars.insert("RUN_CERT_RENEW_FRACTION", " 1 / 2 ");
        let (model, problems) = model_problems(&vars);
        assert!(problems.is_empty(), "{problems:?}");
        let settings = model.run_cert.expect("on");
        assert_eq!(settings.ca_file.as_deref(), Some(std::path::Path::new(ca)));
        assert_eq!(settings.renew_fraction, RenewFraction::new(1, 2).unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn model_auth_injector_sends_the_run_and_refuses_a_key() {
        let mut vars = model_vars();
        vars.remove("MODEL_API_KEY");
        vars.insert("MODEL_AUTH", "injector");
        let (model, problems) = model_problems(&vars);
        assert!(problems.is_empty(), "{problems:?}");
        assert!(model.injected_run);
        assert!(model.device_grant.is_none());
        model.client().expect("a usable client");
        vars.insert("MODEL_AUTH", "Injector");
        assert!(model_problems(&vars).0.injected_run);
        // An empty key is accepted beside it; a key is not, and the process exits 78.
        vars.insert("MODEL_API_KEY", "");
        assert!(model_problems(&vars).1.is_empty());
        vars.insert("MODEL_API_KEY", "sk-secret");
        let (_, problems) = model_problems(&vars);
        assert!(
            mentions(&problems, "MODEL_API_KEY and MODEL_AUTH=injector"),
            "{problems:?}"
        );
        assert!(
            problems.iter().all(|p| !p.contains("sk-secret")),
            "{problems:?}"
        );
        let error = ConfigError::check(problems).unwrap_err();
        assert_eq!(crate::exit_code(&error), 78);
        // A device-grant setting beside the injector is a contradiction, named, and exit 78.
        vars.insert("MODEL_API_KEY", "");
        for name in [
            "MODEL_OAUTH_ISSUER",
            "MODEL_OAUTH_CLIENT_ID",
            "MODEL_OAUTH_SCOPE",
        ] {
            let mut with = vars.clone();
            with.insert(name, "https://id.example.test");
            let (_, problems) = model_problems(&with);
            assert!(mentions(&problems, name), "{name}: {problems:?}");
            let error = ConfigError::check(problems).unwrap_err();
            assert_eq!(crate::exit_code(&error), 78);
        }
        // `key`, the default, is not the injector.
        vars.insert("MODEL_AUTH", "key");
        assert!(!model_problems(&vars).0.injected_run);
    }

    #[test]
    fn model_ca_file_is_read_at_startup_and_a_bad_one_is_78() {
        let dir = std::env::temp_dir().join(format!("adam-service-ca-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ca = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../adam-model-openai/tests/fixtures/test-ca.pem");
        // `Vars` holds static strings: a test may leak three short ones.
        let ca_text: &'static str = ca.display().to_string().leak();
        let empty = dir.join("empty.pem");
        std::fs::write(&empty, "no certificate here").unwrap();
        let empty_text: &'static str = empty.display().to_string().leak();
        let missing_text: &'static str = dir.join("missing.pem").display().to_string().leak();

        let mut vars = model_vars();
        let (model, problems) = model_problems(&vars);
        assert!(problems.is_empty() && model.ca_roots.is_none());
        vars.insert("MODEL_CA_FILE", ca_text);
        let (model, problems) = model_problems(&vars);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(model.ca_roots.as_ref().map(ExtraRoots::len), Some(1));
        assert!(format!("{model:?}").contains("ExtraRoots(1)"));
        model.client().expect("a client that trusts the CA");

        for (file, says) in [
            (empty_text, "no certificate"),
            (missing_text, "cannot be read"),
        ] {
            vars.insert("MODEL_CA_FILE", file);
            let (model, problems) = model_problems(&vars);
            assert!(model.ca_roots.is_none());
            assert!(mentions(&problems, "MODEL_CA_FILE"), "{problems:?}");
            assert!(problems[0].contains(says), "{problems:?}");
            let error = ConfigError::check(problems).unwrap_err();
            assert_eq!(crate::exit_code(&error), 78);
        }
        let _ = std::fs::remove_dir_all(&dir);
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
