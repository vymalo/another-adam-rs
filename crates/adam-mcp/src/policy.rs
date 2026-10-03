//! [`McpPolicy`]: what the deployment decides about MCP servers. The files say which servers an
//! agent uses; the deployment says which kinds it allows and how long it waits.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use adam_llm_agent::ToolError;
use async_trait::async_trait;
use secrecy::SecretString;
use serde_json::{Map, Value};
use url::Url;

/// How long [`McpServers::connect`](crate::McpServers::connect) waits for one server to start,
/// initialize and list its tools (the default of [`McpPolicy::connect_timeout`]).
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long one tool call waits for its answer (the default of [`McpPolicy::call_timeout`]).
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// The longest a call to the thread-tools endpoint may be waited for, whatever the tool says (the
/// default of [`McpPolicy::thread_tools_max_call`]): one hour.
pub const DEFAULT_THREAD_TOOLS_MAX_CALL: Duration = Duration::from_secs(3600);

/// The bearer token a deployment gives one MCP server, **one call at a time**: for a server that
/// serves many accounts with one process (the credential that fits a call depends on what the call
/// is about), a token fixed in `mcp.json` or in the environment at startup is the wrong shape.
///
/// Bind it with [`McpPolicy::bearer_per_call`]. A bound `http` server is listed with
/// [`for_listing`](Self::for_listing) and dialled again for **every** call, with the token
/// [`for_call`](Self::for_call) returns as `Authorization: Bearer <token>` (one connection per
/// call, as [`Endpoint`](crate::Endpoint) does). The token is registered with the redactor of that
/// call, so neither the call's result nor its error repeats it.
///
/// What [`for_call`](Self::for_call) returns decides what the call becomes:
///
/// * `Err(ToolError::Permanent(why))`: the call is **not sent**, and `why` is the error result the
///   model reads (the run goes on). Say what the model can do about it.
/// * `Err(ToolError::Transient(why))`: the call is **not sent** and the tool returns the error as
///   it is, so the run's step fails transiently and the runtime retries it. That is safe because
///   nothing reached the server.
/// * any other [`ToolError`] is returned as it is, and nothing is sent.
///
/// `tool` is the tool's name **on the server** (`get_file_contents`, not `github__get_file_contents`),
/// and `arguments` are the model's, as it wrote them (a call whose arguments are not an object is
/// answered before this is asked). The same value may be asked by many calls at once.
#[async_trait]
pub trait CallBearer: Send + Sync + 'static {
    /// The token to list the server's tools with, once, when
    /// [`McpServers::connect`](crate::McpServers::connect) connects it. A server that checks the
    /// token's form and not its owner accepts a placeholder of the right shape.
    ///
    /// # Errors
    ///
    /// Any [`ToolError`]: connecting fails, as
    /// [`Error::BearerBinding`](crate::Error::BearerBinding) for a permanent error and as
    /// [`Error::ListTools`](crate::Error::ListTools) for a transient one.
    async fn for_listing(&self) -> Result<SecretString, ToolError>;

    /// The token for one call of `tool` with `arguments`.
    ///
    /// # Errors
    ///
    /// See the trait's documentation.
    async fn for_call(
        &self,
        tool: &str,
        arguments: &Map<String, Value>,
    ) -> Result<SecretString, ToolError>;
}

/// A server name bound to an origin and a [`CallBearer`].
#[derive(Clone)]
pub(crate) struct Binding {
    /// The origin as the deployment wrote it, or its normal form (what is shown).
    pub(crate) origin: String,
    /// Whether `origin` was a URL with an origin at all: a binding that is not can match nothing.
    pub(crate) valid: bool,
    pub(crate) bearer: Arc<dyn CallBearer>,
}

impl Binding {
    /// Whether `url` is at the bound origin: same scheme, host and port (a default port is the
    /// same as none).
    pub(crate) fn matches(&self, url: &Url) -> bool {
        self.valid && url.origin().ascii_serialization() == self.origin
    }
}

/// What the deployment allows and how long it waits. The default is the safe one: no local
/// processes, no plain `http` to other machines, no `${VAR}` in a URL, a child that inherits
/// nothing from the environment.
///
/// ```
/// use std::time::Duration;
/// use adam_mcp::McpPolicy;
///
/// let policy = McpPolicy::default()
///     .allow_stdio(true)
///     .call_timeout(Duration::from_secs(120));
/// assert!(policy.stdio_allowed());
/// assert!(!policy.insecure_allowed());
/// assert!(!policy.url_secrets_allowed());
/// ```
#[derive(Clone)]
pub struct McpPolicy {
    allow_stdio: bool,
    allow_insecure: bool,
    allow_url_secrets: bool,
    inherit_env: bool,
    connect_timeout: Duration,
    call_timeout: Duration,
    thread_tools_max_call: Duration,
    bindings: BTreeMap<String, Binding>,
}

impl std::fmt::Debug for McpPolicy {
    /// The settings, and the servers bound to a bearer by name and origin: never a token.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let bound: Vec<String> = self
            .bindings
            .iter()
            .map(|(server, binding)| format!("{server} at {}", binding.origin))
            .collect();
        f.debug_struct("McpPolicy")
            .field("allow_stdio", &self.allow_stdio)
            .field("allow_insecure", &self.allow_insecure)
            .field("allow_url_secrets", &self.allow_url_secrets)
            .field("inherit_env", &self.inherit_env)
            .field("connect_timeout", &self.connect_timeout)
            .field("call_timeout", &self.call_timeout)
            .field("thread_tools_max_call", &self.thread_tools_max_call)
            .field("bearer_per_call", &bound)
            .finish()
    }
}

impl Default for McpPolicy {
    fn default() -> Self {
        Self {
            allow_stdio: false,
            allow_insecure: false,
            allow_url_secrets: false,
            inherit_env: false,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            call_timeout: DEFAULT_CALL_TIMEOUT,
            thread_tools_max_call: DEFAULT_THREAD_TOOLS_MAX_CALL,
            bindings: BTreeMap::new(),
        }
    }
}

impl McpPolicy {
    /// The default policy, as [`Default`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Allow servers that are local processes (`command` in `mcp.json`). **Off by default:** the
    /// file would decide what this process runs, with this process's rights. With it off,
    /// [`connect`](crate::McpServers::connect) refuses such a server before it starts anything
    /// ([`Error::StdioNotAllowed`](crate::Error::StdioNotAllowed)).
    #[must_use]
    pub fn allow_stdio(mut self, allow: bool) -> Self {
        self.allow_stdio = allow;
        self
    }

    /// Allow remote servers at plain `http` URLs that are not this machine. **Development only:**
    /// every request, and its headers, cross the network in the clear. `localhost`,
    /// `*.localhost` and loopback addresses never need it.
    #[must_use]
    pub fn allow_insecure(mut self, allow: bool) -> Self {
        self.allow_insecure = allow;
        self
    }

    /// Allow `${VAR}` in the `url` of a remote server (a key in the query, a token in the path, a
    /// whole URL from the environment). **Off by default:** the MCP library (`rmcp`) and the
    /// HTTP client under it log the URL they dial, for instance when a request fails, and those
    /// log lines do not pass through this crate's redactor. With it off,
    /// [`connect`](crate::McpServers::connect) refuses such a server before any request
    /// ([`Error::UrlSecret`](crate::Error::UrlSecret)); credentials belong in `headers`, which are
    /// never logged.
    ///
    /// **When you turn it on, filter the `rmcp` log target** (and `reqwest` and `hyper` at
    /// `debug` and below) out of any log that leaves the process: the expanded URL, secret
    /// included, appears there. The errors and tool results this crate produces stay scrubbed
    /// either way.
    #[must_use]
    pub fn allow_url_secrets(mut self, allow: bool) -> Self {
        self.allow_url_secrets = allow;
        self
    }

    /// Let a stdio server's process inherit this process's whole environment. **Off by default:**
    /// the child gets `PATH`, `HOME`, `LANG` and `TMPDIR` (and on Windows `SystemRoot`, `TEMP`
    /// and `USERPROFILE`) plus the `env` the file declares, so a server cannot read the
    /// deployment's other secrets. A server that needs more (a proxy setting, a certificate
    /// path) should have it declared in `env` as `${VAR}`; this is the escape hatch.
    #[must_use]
    pub fn inherit_env(mut self, inherit: bool) -> Self {
        self.inherit_env = inherit;
        self
    }

    /// How long one server may take to start, initialize and list its tools before startup
    /// fails ([`Error::Connect`](crate::Error::Connect)). Default 30 seconds. A zero is raised
    /// to one millisecond.
    #[must_use]
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout.max(Duration::from_millis(1));
        self
    }

    /// How long one tool call waits for the server's answer before it is answered with an error
    /// result (the call may still be running on the server). Default 60 seconds. A zero is raised
    /// to one millisecond. For a call to the thread-tools endpoint this is the wait for a tool that
    /// does not say how long it may take ([`thread_tools_max_call`](Self::thread_tools_max_call) caps
    /// the ones that do).
    #[must_use]
    pub fn call_timeout(mut self, timeout: Duration) -> Self {
        self.call_timeout = timeout.max(Duration::from_millis(1));
        self
    }

    /// Give the server called `server` a bearer token **per call**, from `bearer`
    /// ([`CallBearer`]), when its `url` is at `origin`.
    ///
    /// The deployment binds a **name and an origin**, because the agent's files say which servers
    /// exist and the deployment says whose credential each one gets. `origin` is a URL of which
    /// only the origin (scheme, host and port) counts: `http://127.0.0.1:8082` and
    /// `http://127.0.0.1:8082/mcp` are the same. [`connect`](crate::McpServers::connect) then:
    ///
    /// * refuses a file that points `server` at another origin, or that gives it an
    ///   `Authorization` header of its own (a credential the deployment did not choose would be
    ///   sent beside, or instead of, the one it did): [`Error::BearerBinding`](crate::Error::BearerBinding),
    ///   before any request;
    /// * lists the server's tools with [`CallBearer::for_listing`], and sends each call with
    ///   [`CallBearer::for_call`] (see there);
    /// * leaves a **stdio** server of that name alone, with a warning: it has no URL to send a
    ///   bearer to, and a folder written before the binding existed keeps working.
    ///
    /// A second binding of the same name replaces the first. A name no file uses binds nothing.
    /// An `origin` that is not a URL binds nothing that can be reached: every server of that name
    /// is refused.
    #[must_use]
    pub fn bearer_per_call(
        mut self,
        server: impl Into<String>,
        origin: &str,
        bearer: Arc<dyn CallBearer>,
    ) -> Self {
        let parsed = Url::parse(origin.trim())
            .ok()
            .map(|url| url.origin())
            .filter(url::Origin::is_tuple);
        let binding = Binding {
            valid: parsed.is_some(),
            origin: parsed.map_or_else(
                || "<not an origin>".to_owned(),
                |origin| origin.ascii_serialization(),
            ),
            bearer,
        };
        self.bindings.insert(server.into(), binding);
        self
    }

    pub(crate) fn binding(&self, server: &str) -> Option<&Binding> {
        self.bindings.get(server)
    }

    /// Whether local processes are allowed.
    pub fn stdio_allowed(&self) -> bool {
        self.allow_stdio
    }

    /// Whether plain `http` to other machines is allowed.
    pub fn insecure_allowed(&self) -> bool {
        self.allow_insecure
    }

    /// Whether `${VAR}` may appear in the URL of a remote server.
    pub fn url_secrets_allowed(&self) -> bool {
        self.allow_url_secrets
    }

    /// Whether a child process inherits the environment.
    pub fn inherits_env(&self) -> bool {
        self.inherit_env
    }

    /// The longest an agent waits for one call to a tool of the thread-tools endpoint, whatever
    /// time the tool says it may take (`_meta["thread-tools/v1"].timeoutSecs`): a cap, never a
    /// default. Default one hour ([`DEFAULT_THREAD_TOOLS_MAX_CALL`]). A zero is raised to one
    /// millisecond. The `THREAD_TOOLS_MAX_CALL_SECS` variable of the binaries.
    #[must_use]
    pub fn thread_tools_max_call(mut self, cap: Duration) -> Self {
        self.thread_tools_max_call = cap.max(Duration::from_millis(1));
        self
    }

    /// The connect timeout.
    pub fn connect_timeout_value(&self) -> Duration {
        self.connect_timeout
    }

    /// The call timeout.
    pub fn call_timeout_value(&self) -> Duration {
        self.call_timeout
    }

    /// The cap on a call to the thread-tools endpoint.
    pub fn thread_tools_max_call_value(&self) -> Duration {
        self.thread_tools_max_call
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Holds a secret, to prove `Debug` of the policy cannot reach it.
    struct Holds(#[allow(dead_code)] String);

    #[async_trait]
    impl CallBearer for Holds {
        async fn for_listing(&self) -> Result<SecretString, ToolError> {
            Ok(SecretString::from(self.0.clone()))
        }

        async fn for_call(
            &self,
            _tool: &str,
            _arguments: &Map<String, Value>,
        ) -> Result<SecretString, ToolError> {
            Ok(SecretString::from(self.0.clone()))
        }
    }

    #[test]
    fn debug_names_the_binding_and_never_a_token() {
        let policy = McpPolicy::default().bearer_per_call(
            "github",
            "http://127.0.0.1:8082/some/path?x=secret-query",
            Arc::new(Holds("ghs_secret-token-8c1e".to_owned())),
        );
        let shown = format!("{policy:?} / {policy:#?}");
        assert!(shown.contains("github at http://127.0.0.1:8082"), "{shown}");
        assert!(
            !shown.contains("secret-token") && !shown.contains("secret-query"),
            "{shown}"
        );
        assert!(!shown.contains("some/path"), "{shown}");
        // The settings are still shown.
        assert!(shown.contains("allow_stdio: false"), "{shown}");
        // No binding: an empty list, not a missing field.
        let plain = format!("{:?}", McpPolicy::default());
        assert!(plain.contains("bearer_per_call: []"), "{plain}");
    }

    #[test]
    fn an_origin_is_scheme_host_and_port() {
        let policy = McpPolicy::default().bearer_per_call(
            "github",
            "HTTPS://Example.COM:443/x",
            Arc::new(Holds(String::new())),
        );
        let binding = policy.binding("github").unwrap();
        for same in ["https://example.com/mcp", "https://example.com:443/other"] {
            assert!(binding.matches(&Url::parse(same).unwrap()), "{same}");
        }
        for other in [
            "http://example.com/mcp",
            "https://example.com:8443/mcp",
            "https://example.org/mcp",
            "https://example.com.evil.test/mcp",
            "https://user@example.com/mcp",
        ] {
            // A user name does not change the origin, which is why connect refuses credentials
            // in a URL before it asks about the binding; every other difference does.
            let url = Url::parse(other).unwrap();
            assert_eq!(binding.matches(&url), other.contains("user@"), "{other}");
        }
        assert!(policy.binding("other").is_none());
    }
}
