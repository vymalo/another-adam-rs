//! [`McpPolicy`]: what the deployment decides about MCP servers. The files say which servers an
//! agent uses; the deployment says which kinds it allows and how long it waits.

use std::time::Duration;

/// How long [`McpServers::connect`](crate::McpServers::connect) waits for one server to start,
/// initialize and list its tools (the default of [`McpPolicy::connect_timeout`]).
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long one tool call waits for its answer (the default of [`McpPolicy::call_timeout`]).
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// The longest a call to the thread-tools endpoint may be waited for, whatever the tool says (the
/// default of [`McpPolicy::thread_tools_max_call`]): one hour.
pub const DEFAULT_THREAD_TOOLS_MAX_CALL: Duration = Duration::from_secs(3600);

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
#[derive(Debug, Clone)]
pub struct McpPolicy {
    allow_stdio: bool,
    allow_insecure: bool,
    allow_url_secrets: bool,
    inherit_env: bool,
    connect_timeout: Duration,
    call_timeout: Duration,
    thread_tools_max_call: Duration,
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
