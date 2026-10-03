//! A server the deployment gave a bearer per call ([`CallBearer`](crate::CallBearer)): listed
//! once at startup, dialled again for every call.
//!
//! ```mermaid
//! sequenceDiagram
//!     participant T as McpTool
//!     participant B as CallBearer (the deployment)
//!     participant S as MCP server (http, at the bound origin)
//!     T->>B: for_call(tool, arguments)
//!     alt Permanent
//!         B-->>T: error, nothing is sent: an error result for the model
//!     else Transient
//!         B-->>T: error, nothing is sent: ToolError::Transient
//!     else a token
//!         B-->>T: token
//!         T->>S: initialize (Authorization: Bearer token)
//!         S-->>T: initialized
//!         T->>S: tools/call
//!         S-->>T: result
//!         T->>S: close
//!         Note over T: the result is scrubbed of the token, and of every value a ${VAR} put into the server's text
//!     end
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use adam_llm_agent::ToolError;
use reqwest::header::{AUTHORIZATION, HeaderName, HeaderValue};
use rmcp::model::Tool as ListedTool;
use secrecy::{ExposeSecret, SecretString};
use url::Url;

use crate::connection::{Recipe, Transport, list_with};
use crate::error::Error;
use crate::policy::CallBearer;
use crate::redact::Redactor;
use crate::text::{MAX_MESSAGE_BYTES, cap_text};

/// How long a per-call connection is given to close before it is dropped.
pub(crate) const CLOSE_GRACE: Duration = Duration::from_secs(2);

/// The token cannot be sent: empty, or not a header value. Carries nothing, so nothing can leak.
#[derive(Debug)]
pub(crate) struct UnusableBearer;

/// What a bound server is dialled with, apart from the token of each call.
pub(crate) struct PerCall {
    pub(crate) bearer: Arc<dyn CallBearer>,
    server: String,
    url: Url,
    /// The headers of the file (never an `Authorization`: the binding refuses one).
    headers: Vec<(HeaderName, HeaderValue)>,
    /// What the file's expansions put in the server's text; each call adds its token to a copy.
    redactor: Redactor,
    connect_timeout: Duration,
    closed: AtomicBool,
}

impl PerCall {
    pub(crate) fn new(
        server: &str,
        bearer: Arc<dyn CallBearer>,
        url: Url,
        headers: Vec<(HeaderName, HeaderValue)>,
        redactor: Redactor,
        connect_timeout: Duration,
    ) -> Self {
        Self {
            bearer,
            server: server.to_owned(),
            url,
            headers,
            redactor,
            connect_timeout,
            closed: AtomicBool::new(false),
        }
    }

    /// The recipe of one connection, carrying `token`, with `token` in its redactor.
    pub(crate) fn recipe(&self, token: &SecretString) -> Result<Recipe, UnusableBearer> {
        let secret = token.expose_secret();
        if secret.trim().is_empty() {
            return Err(UnusableBearer);
        }
        let mut value =
            HeaderValue::from_str(&format!("Bearer {secret}")).map_err(|_| UnusableBearer)?;
        value.set_sensitive(true);
        let mut headers = self.headers.clone();
        headers.push((AUTHORIZATION, value));
        let mut redactor = self.redactor.clone();
        // The token alone, and as the header carries it: the same two a file's `Bearer ${TOKEN}`
        // registers.
        redactor.add(secret);
        redactor.add(&format!("Bearer {secret}"));
        Ok(Recipe {
            server: self.server.clone(),
            command_as_written: String::new(),
            transport: Transport::Http {
                url: self.url.clone(),
                headers,
            },
            redactor: Arc::new(redactor),
            connect_timeout: self.connect_timeout,
        })
    }

    /// Scrub a message that came from the deployment's bearer, and cap it.
    pub(crate) fn scrub(&self, text: &str) -> String {
        cap_text(self.redactor.scrub(text), MAX_MESSAGE_BYTES)
    }

    /// Make every later call an error result.
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// The tools of the server, listed once with the listing bearer, on a connection that is
    /// closed again.
    pub(crate) async fn list(&self) -> Result<Vec<ListedTool>, Error> {
        let refused = |why: String| Error::BearerBinding {
            server: self.server.clone(),
            why,
        };
        let token = match self.bearer.for_listing().await {
            Ok(token) => token,
            Err(ToolError::Transient(message)) => {
                return Err(Error::ListTools {
                    server: self.server.clone(),
                    message: self.scrub(&format!(
                        "the bearer for listing is not available: {message}"
                    )),
                });
            }
            Err(other) => {
                return Err(refused(self.scrub(&format!(
                    "the bearer for listing was refused: {}",
                    reason(&other)
                ))));
            }
        };
        let recipe = self.recipe(&token).map_err(|UnusableBearer| {
            refused("the bearer for listing is empty or not a valid header value".to_owned())
        })?;
        let mut service = recipe.dial().await?;
        let listed = list_with(&recipe, &service).await;
        let _ = service.close_with_timeout(CLOSE_GRACE).await;
        listed
    }
}

/// What a [`ToolError`] says, without its own prefix.
pub(crate) fn reason(error: &ToolError) -> String {
    match error {
        ToolError::Transient(message) | ToolError::Permanent(message) => message.clone(),
        other => other.to_string(),
    }
}
