//! One connection to one MCP server: the recipe that makes it, the live session, and the
//! reconnect that a broken session gets on the next call.
//!
//! ```mermaid
//! stateDiagram-v2
//!     [*] --> Connecting: connect
//!     Connecting --> Ready: initialized, tools listed
//!     Connecting --> [*]: startup error (fail closed)
//!     Ready --> Broken: transport closed or died during a call
//!     Broken --> Ready: next call, one reconnect from the same recipe
//!     Broken --> Broken: reconnect failed (the call gets an error result)
//!     Ready --> Closed: shutdown
//!     Broken --> Closed: shutdown
//!     Closed --> [*]
//! ```

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{HeaderName, HeaderValue};
use rmcp::model::{ClientCapabilities, ClientConfig, Implementation, Tool as ListedTool};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{Peer, RoleClient, ServiceExt};
use secrecy::{ExposeSecret, SecretString};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt};
use tokio::process::ChildStderr;
use url::Url;

use crate::error::Error;
use crate::redact::Redactor;
use crate::text::{MAX_MESSAGE_BYTES, cap_text};

pub(crate) type Service = RunningService<RoleClient, ClientConfig>;

/// The variables a child process gets when it does not inherit the environment.
const SAFE_VARS: &[&str] = &[
    "PATH",
    "HOME",
    "LANG",
    "TMPDIR",
    #[cfg(windows)]
    "SystemRoot",
    #[cfg(windows)]
    "TEMP",
    #[cfg(windows)]
    "USERPROFILE",
];

/// A line of a child's stderr longer than this is dropped whole: a cut line could end in the
/// first half of a secret that the redactor would no longer recognise.
const STDERR_LINE_MAX: usize = 4 * 1024;

/// How long a connection is given to close cleanly before it is dropped (which kills a child).
const CLOSE_GRACE: Duration = Duration::from_secs(3);

/// How the server is reached, with every `${VAR}` already expanded. Holds secrets: never `Debug`.
pub(crate) enum Transport {
    Stdio {
        command: SecretString,
        args: Vec<SecretString>,
        env: Vec<(String, SecretString)>,
        inherit_env: bool,
    },
    Http {
        url: Url,
        headers: Vec<(HeaderName, HeaderValue)>,
    },
}

/// Everything needed to connect to a server, again and again: what `connect` decided from the
/// file, the environment and the policy.
pub(crate) struct Recipe {
    pub(crate) server: String,
    /// The command as the file writes it (before expansion), for messages.
    pub(crate) command_as_written: String,
    pub(crate) transport: Transport,
    pub(crate) redactor: Arc<Redactor>,
    pub(crate) connect_timeout: Duration,
}

impl Recipe {
    /// Scrub a third-party message and cap it.
    pub(crate) fn scrub(&self, text: &str) -> String {
        cap_text(self.redactor.scrub(text), MAX_MESSAGE_BYTES)
    }

    fn connect_error(&self, message: &str) -> Error {
        Error::Connect {
            server: self.server.clone(),
            message: self.scrub(message),
        }
    }

    /// Start (or dial) the server and run the handshake, within the connect timeout.
    pub(crate) async fn dial(&self) -> Result<Service, Error> {
        match tokio::time::timeout(self.connect_timeout, self.dial_untimed()).await {
            Ok(result) => result,
            Err(_) => Err(self.connect_error(&format!(
                "no answer within {} ms",
                self.connect_timeout.as_millis()
            ))),
        }
    }

    async fn dial_untimed(&self) -> Result<Service, Error> {
        let config = ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("adam-mcp", env!("CARGO_PKG_VERSION")),
        );
        match &self.transport {
            Transport::Stdio { .. } => {
                let (transport, stderr) = self.spawn()?;
                if let Some(stderr) = stderr {
                    self.log_stderr(stderr);
                }
                config
                    .serve(transport)
                    .await
                    .map_err(|e| self.connect_error(&adam_error::report(&e)))
            }
            Transport::Http { url, headers } => {
                let client = reqwest::Client::builder()
                    // A redirect could carry the request, and its headers, somewhere nobody named.
                    .redirect(reqwest::redirect::Policy::none())
                    .connect_timeout(Duration::from_secs(10))
                    // No connection is kept idle: a stale one is the classic cause of a request
                    // that fails once after the server restarted.
                    .pool_max_idle_per_host(0)
                    .user_agent(concat!("adam-mcp/", env!("CARGO_PKG_VERSION")))
                    // No overall timeout: it would cut the response streams (SSE) of a call
                    // that takes a while. Calls have their own timeout.
                    .build()
                    .map_err(|e| {
                        self.connect_error(&format!("cannot build an HTTP client: {e}"))
                    })?;
                let custom = sensitive_headers(headers);
                let transport = StreamableHttpClientTransport::with_client(
                    client,
                    StreamableHttpClientTransportConfig::with_uri(url.to_string())
                        .custom_headers(custom)
                        // We reconnect ourselves, once per call and never inside a request, so a
                        // request is not sent twice behind our back.
                        .reinit_on_expired_session(false),
                );
                config
                    .serve(transport)
                    .await
                    .map_err(|e| self.connect_error(&adam_error::report(&e)))
            }
        }
    }

    /// Start the child process: its own environment, and stderr piped so that it can be logged
    /// redacted instead of written to this process's stderr.
    fn spawn(&self) -> Result<(TokioChildProcess, Option<ChildStderr>), Error> {
        let Transport::Stdio {
            command,
            args,
            env,
            inherit_env,
        } = &self.transport
        else {
            return Err(self.connect_error("not a local process"));
        };
        let mut cmd = tokio::process::Command::new(command.expose_secret());
        cmd.args(args.iter().map(|a| a.expose_secret()));
        // A child must never outlive the process that started it, however the connection ends.
        cmd.kill_on_drop(true);
        if !inherit_env {
            cmd.env_clear();
            for name in SAFE_VARS {
                if let Some(value) = std::env::var_os(name) {
                    cmd.env(name, value);
                }
            }
        }
        for (name, value) in env {
            cmd.env(name, value.expose_secret());
        }
        TokioChildProcess::builder(cmd)
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::Spawn {
                server: self.server.clone(),
                command: self.command_as_written.clone(),
                message: self.scrub(&e.to_string()),
            })
    }

    /// Log what the child writes to stderr, at debug level, redacted, one line at a time.
    fn log_stderr(&self, stderr: ChildStderr) {
        let server = self.server.clone();
        let redactor = Arc::clone(&self.redactor);
        tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(stderr);
            let mut line = Vec::new();
            loop {
                match next_line(&mut reader, &mut line).await {
                    Ok(Line::Text) => {
                        let text = redactor.scrub(&String::from_utf8_lossy(&line));
                        tracing::debug!(
                            target: "adam_mcp::stderr",
                            server = %server,
                            "{}",
                            text.trim_end()
                        );
                    }
                    Ok(Line::TooLong) => {
                        tracing::debug!(
                            target: "adam_mcp::stderr",
                            server = %server,
                            "a stderr line over {STDERR_LINE_MAX} bytes was dropped"
                        );
                    }
                    Ok(Line::End) | Err(_) => break,
                }
            }
        });
    }
}

/// The headers of the file as the transport is given them, every value marked sensitive: a
/// sensitive value shows as `Sensitive` in `Debug` output, wherever a library prints the request
/// or the transport's configuration, and HTTP/2 does not index it.
fn sensitive_headers(headers: &[(HeaderName, HeaderValue)]) -> HashMap<HeaderName, HeaderValue> {
    headers
        .iter()
        .map(|(name, value)| {
            let mut value = value.clone();
            value.set_sensitive(true);
            (name.clone(), value)
        })
        .collect()
}

enum Line {
    Text,
    TooLong,
    End,
}

/// The next line of `reader` into `out` (without reading more than [`STDERR_LINE_MAX`] bytes of
/// it into memory at once).
async fn next_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    out: &mut Vec<u8>,
) -> std::io::Result<Line> {
    out.clear();
    let limit = STDERR_LINE_MAX as u64;
    let read = (&mut *reader).take(limit).read_until(b'\n', out).await?;
    if read == 0 {
        return Ok(Line::End);
    }
    if out.last() == Some(&b'\n') || (read as u64) < limit {
        return Ok(Line::Text);
    }
    // The limit was reached inside a line: skip the rest of it.
    out.clear();
    let mut rest = Vec::new();
    loop {
        rest.clear();
        let read = (&mut *reader)
            .take(limit)
            .read_until(b'\n', &mut rest)
            .await?;
        if read == 0 || rest.last() == Some(&b'\n') {
            return Ok(Line::TooLong);
        }
    }
}

/// Ask `service` (just dialled from `recipe`) for its tools, every page, within the connect timeout.
pub(crate) async fn list_with(
    recipe: &Recipe,
    service: &Service,
) -> Result<Vec<ListedTool>, Error> {
    match tokio::time::timeout(recipe.connect_timeout, service.peer().list_all_tools()).await {
        Ok(Ok(tools)) => Ok(tools),
        Ok(Err(e)) => Err(Error::ListTools {
            server: recipe.server.clone(),
            message: recipe.scrub(&adam_error::report(&e)),
        }),
        Err(_) => Err(Error::ListTools {
            server: recipe.server.clone(),
            message: format!("no answer within {} ms", recipe.connect_timeout.as_millis()),
        }),
    }
}

/// The state of one connection.
struct Live {
    service: Option<Service>,
    /// Counts the sessions made so far, so that a call that finds its session broken does not
    /// break the newer one that another call made in the meantime.
    generation: u64,
    closed: bool,
}

/// A server, kept: the recipe, and the session that is live now (if any).
pub(crate) struct Connection {
    recipe: Recipe,
    live: tokio::sync::Mutex<Live>,
}

impl Connection {
    /// Connect for the first time and list the tools. A failure is a startup error.
    pub(crate) async fn open(recipe: Recipe) -> Result<(Arc<Self>, Vec<ListedTool>), Error> {
        let service = recipe.dial().await?;
        let tools = list_with(&recipe, &service).await?;
        let connection = Arc::new(Self {
            recipe,
            live: tokio::sync::Mutex::new(Live {
                service: Some(service),
                generation: 1,
                closed: false,
            }),
        });
        Ok((connection, tools))
    }

    /// The values this server's messages may not repeat.
    pub(crate) fn redactor(&self) -> &Redactor {
        &self.recipe.redactor
    }

    /// The peer to call, and the number of its session. A session that is gone is replaced, once,
    /// from the same recipe (a stdio server is started again); the tools are not listed again.
    /// Calls wait for each other here, so a dead server is redialled once and not once per call.
    ///
    /// # Errors
    ///
    /// The text of an error result: the connection was shut down, or the reconnect failed.
    pub(crate) async fn peer(&self) -> Result<(Peer<RoleClient>, u64), String> {
        let mut live = self.live.lock().await;
        if live.closed {
            return Err(format!(
                "the connection to the MCP server `{}` was shut down",
                self.recipe.server
            ));
        }
        if let Some(service) = &live.service {
            if !service.is_closed() && !service.peer().is_transport_closed() {
                return Ok((service.peer().clone(), live.generation));
            }
            tracing::debug!(server = %self.recipe.server, "the MCP session is gone");
            live.service = None;
        }
        match self.recipe.dial().await {
            Ok(service) => {
                tracing::info!(server = %self.recipe.server, "reconnected to the MCP server");
                live.generation += 1;
                let peer = service.peer().clone();
                live.service = Some(service);
                Ok((peer, live.generation))
            }
            Err(error) => Err(format!(
                "the connection to the MCP server `{}` was lost and could not be restored: {error}",
                self.recipe.server
            )),
        }
    }

    /// The session of `generation` failed: drop it (which ends a child process), so that the next
    /// call starts a new one. A newer session is left alone.
    pub(crate) async fn mark_broken(&self, generation: u64) {
        let mut live = self.live.lock().await;
        if live.generation == generation {
            live.service = None;
        }
    }

    /// Close the session, and refuse every later call.
    pub(crate) async fn close(&self) {
        let mut live = self.live.lock().await;
        live.closed = true;
        if let Some(mut service) = live.service.take() {
            let _ = service.close_with_timeout(CLOSE_GRACE).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_values_are_marked_sensitive_and_never_debug_printed() {
        let plain = HeaderValue::from_static("Bearer tok-5e1a-secret");
        assert!(!plain.is_sensitive());
        let headers = vec![
            (HeaderName::from_static("authorization"), plain),
            (
                HeaderName::from_static("x-api-key"),
                HeaderValue::from_static("key-5e1a-secret"),
            ),
        ];
        let custom = sensitive_headers(&headers);
        assert_eq!(custom.len(), 2);
        assert!(custom.values().all(HeaderValue::is_sensitive));
        // What a library that prints the transport's configuration would show.
        let config = StreamableHttpClientTransportConfig::with_uri("http://127.0.0.1:1/mcp")
            .custom_headers(custom);
        let shown = format!("{config:?}");
        assert!(shown.contains("Sensitive"), "{shown}");
        assert!(!shown.contains("5e1a-secret"), "{shown}");
    }
}
