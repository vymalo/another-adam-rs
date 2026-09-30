//! An OpenAI-compatible [`ModelClient`].
//!
//! [`OpenAiCompatible`] speaks the chat-completions protocol
//! (`POST {base_url}/chat/completions`) and so works against OpenAI itself and
//! against any gateway or server that mirrors it: EAIG / Agent Router, AISIX,
//! LiteLLM, vLLM, Ollama, and so on. Agents never see this crate's types; they
//! hold an [`adam_model::DynModel`].
//!
//! ```no_run
//! use std::sync::Arc;
//! use std::time::Duration;
//! use adam_model::DynModel;
//! use adam_model_openai::{OpenAiCompatible, OpenAiConfig};
//! use secrecy::SecretString;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let model: DynModel = Arc::new(OpenAiCompatible::new(OpenAiConfig {
//!     base_url: "https://gateway.example.com/v1".into(),
//!     api_key: SecretString::from("sk-..."),
//!     timeout: Duration::from_secs(120),
//!     extra_headers: Default::default(),
//! })?);
//! # Ok(()) }
//! ```
//!
//! # Behaviour worth knowing
//!
//! * **No retries.** Failures are mapped onto [`adam_model::ModelError`] and
//!   returned; the runtime decides whether to retry
//!   ([`Classify::is_retryable`]).
//!   `Retry-After` (seconds or HTTP-date) is surfaced in
//!   [`ModelError::RateLimited`].
//! * **Timeouts.** [`OpenAiConfig::timeout`] bounds a whole `complete` call.
//!   For `stream` it bounds the wait for the response headers and then the
//!   silence between chunks, so a long generation is never cut off while
//!   tokens keep arriving. Expiry is [`Transient`](adam_model::ModelError::Transient).
//! * **Secrets.** The API key lives in a [`SecretString`]; it is sent as a
//!   sensitive `Authorization: Bearer` header and never appears in `Debug`
//!   output, error messages or logs. Extra header *values* are hidden from
//!   `Debug` too. An empty key sends no `Authorization` header (local servers).
//! * **Tool calls** arrive in streamed deltas keyed by index and are assembled
//!   before parsing; malformed argument JSON is a
//!   [`Protocol`](adam_model::ModelError::Protocol) error, empty arguments are
//!   `{}`. A `stop` finish that carries tool calls is reported as
//!   [`ToolCalls`](adam_model::FinishReason::ToolCalls).
//! * **`is_error` on tool messages** has no counterpart in the chat-completions
//!   format and is not sent; tools should describe failures in their content.
//! * **`max_output_tokens`** is sent as `max_tokens` by default, which every
//!   server accepts. OpenAI's newer reasoning models want
//!   `max_completion_tokens`; use [`OpenAiCompatible::with_max_tokens_field`].
//! * **`metadata`** is sent as the request's `metadata` object only when
//!   non-empty.

#![warn(missing_docs)]

mod errors;
mod sse;
mod wire;

use std::collections::BTreeMap;
use std::fmt;
use std::time::{Duration, SystemTime};

use adam_error::{BoxError, Classify, ErrorClass};
use adam_model::{ModelClient, ModelDelta, ModelError, ModelRequest, ModelResponse};
use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use secrecy::{ExposeSecret, SecretString};

/// Configuration of an [`OpenAiCompatible`] client.
#[derive(Clone)]
pub struct OpenAiConfig {
    /// Base URL of the API including the version prefix, e.g.
    /// `https://api.openai.com/v1`. `/chat/completions` is appended.
    pub base_url: String,
    /// Bearer token. Empty means "send no `Authorization` header".
    pub api_key: SecretString,
    /// Request timeout; see the [crate docs](crate#behaviour-worth-knowing).
    pub timeout: Duration,
    /// Extra headers sent with every request (gateway routing, tenant ids...).
    pub extra_headers: BTreeMap<String, String>,
}

impl OpenAiConfig {
    /// A config with a 120 second timeout and no extra headers.
    pub fn new(base_url: impl Into<String>, api_key: SecretString) -> Self {
        Self {
            base_url: base_url.into(),
            api_key,
            timeout: Duration::from_secs(120),
            extra_headers: BTreeMap::new(),
        }
    }
}

impl fmt::Debug for OpenAiConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiConfig")
            .field("base_url", &self.base_url)
            .field("api_key", &"[REDACTED]")
            .field("timeout", &self.timeout)
            // Header values can carry credentials (`x-api-key`); names cannot.
            .field(
                "extra_headers",
                &self.extra_headers.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// Which JSON field carries [`ModelRequest::max_output_tokens`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MaxTokensField {
    /// `max_tokens`: accepted by virtually every OpenAI-compatible server.
    #[default]
    MaxTokens,
    /// `max_completion_tokens`: required by OpenAI's reasoning models, and
    /// accepted by most current gateways.
    MaxCompletionTokens,
}

/// The configuration could not be turned into a client.
///
/// Classified as [`ErrorClass::Invalid`] (the configuration is wrong), except
/// [`Client`](Self::Client), which is [`ErrorClass::Internal`]. No message carries the URL or
/// the key, which may hold credentials.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OpenAiConfigError {
    /// `base_url` is not an absolute http(s) URL.
    #[error("invalid base_url: {reason}")]
    InvalidBaseUrl {
        /// What is wrong with it.
        reason: String,
        /// The parser's own error, when there is one.
        #[source]
        source: Option<BoxError>,
    },
    /// An entry of `extra_headers` is not a valid HTTP header.
    #[error("invalid header `{0}`")]
    InvalidHeader(String),
    /// The API key contains characters that cannot appear in a header.
    #[error("api_key is not a valid bearer token")]
    InvalidApiKey,
    /// The HTTP client could not be built (for example, no TLS backend).
    #[error("could not build the HTTP client")]
    Client(#[source] BoxError),
}

impl Classify for OpenAiConfigError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::InvalidBaseUrl { .. } | Self::InvalidHeader(_) | Self::InvalidApiKey => {
                ErrorClass::Invalid
            }
            Self::Client(_) => ErrorClass::Internal,
        }
    }
}

/// A [`ModelClient`] for any OpenAI-compatible chat-completions endpoint.
///
/// Cheap to clone; clones share one connection pool.
#[derive(Clone)]
pub struct OpenAiCompatible {
    http: reqwest::Client,
    url: String,
    timeout: Duration,
    max_tokens_field: MaxTokensField,
}

impl fmt::Debug for OpenAiCompatible {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiCompatible")
            .field("url", &self.url)
            .field("timeout", &self.timeout)
            .field("max_tokens_field", &self.max_tokens_field)
            .finish_non_exhaustive()
    }
}

impl OpenAiCompatible {
    /// Build a client. Validates the URL and headers; makes no network call.
    pub fn new(config: OpenAiConfig) -> Result<Self, OpenAiConfigError> {
        let base = config.base_url.trim().trim_end_matches('/');
        let parsed = reqwest::Url::parse(base).map_err(|e| OpenAiConfigError::InvalidBaseUrl {
            reason: e.to_string(),
            source: Some(Box::new(e)),
        })?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err(OpenAiConfigError::InvalidBaseUrl {
                reason: "expected an absolute http(s) URL".into(),
                source: None,
            });
        }

        let mut headers = HeaderMap::new();
        for (name, value) in &config.extra_headers {
            let header_name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| OpenAiConfigError::InvalidHeader(name.clone()))?;
            let mut header_value = HeaderValue::from_str(value)
                .map_err(|_| OpenAiConfigError::InvalidHeader(name.clone()))?;
            header_value.set_sensitive(true);
            headers.insert(header_name, header_value);
        }
        let key = config.api_key.expose_secret();
        if !key.is_empty() {
            let mut value = HeaderValue::from_str(&format!("Bearer {key}"))
                .map_err(|_| OpenAiConfigError::InvalidApiKey)?;
            value.set_sensitive(true);
            headers.insert(AUTHORIZATION, value);
        }

        let http = reqwest::Client::builder()
            .default_headers(headers)
            .user_agent(concat!("adam-model-openai/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(config.timeout)
            // Never replay a POST carrying credentials somewhere else.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| OpenAiConfigError::Client(Box::new(e.without_url())))?;

        Ok(Self {
            http,
            url: format!("{base}/chat/completions"),
            timeout: config.timeout,
            max_tokens_field: MaxTokensField::default(),
        })
    }

    /// Choose the JSON field used for `max_output_tokens`.
    pub fn with_max_tokens_field(mut self, field: MaxTokensField) -> Self {
        self.max_tokens_field = field;
        self
    }

    /// Send the request and return the response if it is a success.
    async fn send(&self, body: Vec<u8>, streaming: bool) -> Result<reqwest::Response, ModelError> {
        let mut request = self
            .http
            .post(&self.url)
            .header(CONTENT_TYPE, "application/json")
            .body(body);
        if streaming {
            request = request.header(ACCEPT, "text/event-stream");
        } else {
            request = request.timeout(self.timeout);
        }

        // For streams the timeout covers only the wait for response headers.
        let response = tokio::time::timeout(self.timeout, request.send())
            .await
            .map_err(|_| ModelError::transient(format!("no response within {:?}", self.timeout)))?
            .map_err(transport_error)?;

        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| errors::parse_retry_after(v, SystemTime::now()));
        let text = tokio::time::timeout(self.timeout, response.text())
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        Err(errors::from_http(status, retry_after, &text))
    }
}

fn transport_error(e: reqwest::Error) -> ModelError {
    // The URL is not secret, but it is noise; the key is never in it.
    let e = e.without_url();
    if e.is_timeout() {
        ModelError::transient("request timed out").with_source(e)
    } else if e.is_connect() {
        ModelError::transient("connection failed").with_source(e)
    } else if e.is_builder() {
        ModelError::invalid_request("the request could not be built").with_source(e)
    } else {
        ModelError::transient("transport error").with_source(e)
    }
}

#[async_trait]
impl ModelClient for OpenAiCompatible {
    #[tracing::instrument(name = "model.complete", skip_all, fields(model = %req.model, url = %self.url))]
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse, ModelError> {
        let body = wire::build_request(&req, false, self.max_tokens_field)?;
        let response = self.send(body, false).await?;
        let bytes = response.bytes().await.map_err(transport_error)?;
        let parsed = wire::parse_completion(&bytes);
        match &parsed {
            Ok(r) => tracing::debug!(
                finish = ?r.finish,
                input_tokens = r.usage.input_tokens,
                output_tokens = r.usage.output_tokens,
                "model call finished"
            ),
            Err(e) => tracing::warn!(error = %e, "model call failed"),
        }
        parsed
    }

    #[tracing::instrument(name = "model.stream", skip_all, fields(model = %req.model, url = %self.url))]
    async fn stream(
        &self,
        req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError> {
        let body = wire::build_request(&req, true, self.max_tokens_field)?;
        let response = self.send(body, true).await?;
        Ok(sse::deltas(response.bytes_stream(), self.timeout).boxed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(base_url: &str) -> OpenAiConfig {
        OpenAiConfig::new(base_url, SecretString::from("sk-super-secret"))
    }

    #[test]
    fn debug_never_shows_secrets() {
        let mut cfg = config("https://x.example/v1");
        cfg.extra_headers
            .insert("x-api-key".into(), "header-secret".into());
        let shown = format!("{cfg:?}");
        assert!(!shown.contains("sk-super-secret"), "{shown}");
        assert!(!shown.contains("header-secret"), "{shown}");
        assert!(shown.contains("x-api-key"), "{shown}");

        let client = OpenAiCompatible::new(cfg).unwrap();
        let shown = format!("{client:?}");
        assert!(!shown.contains("sk-super-secret"), "{shown}");
        assert!(!shown.contains("header-secret"), "{shown}");
    }

    #[test]
    fn base_url_is_normalised_and_validated() {
        let c = OpenAiCompatible::new(config("https://x.example/v1/")).unwrap();
        assert_eq!(c.url, "https://x.example/v1/chat/completions");
        for bad in ["", "not a url", "ftp://x.example", "/relative"] {
            assert!(
                matches!(
                    OpenAiCompatible::new(config(bad)),
                    Err(OpenAiConfigError::InvalidBaseUrl { .. })
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn config_error_class_table() {
        #[derive(Debug, thiserror::Error)]
        #[error("build")]
        struct Build;
        // Exhaustive: a new variant forces a class decision here.
        let expected = |e: &OpenAiConfigError| match e {
            OpenAiConfigError::InvalidBaseUrl { .. } => ErrorClass::Invalid,
            OpenAiConfigError::InvalidHeader(_) => ErrorClass::Invalid,
            OpenAiConfigError::InvalidApiKey => ErrorClass::Invalid,
            OpenAiConfigError::Client(_) => ErrorClass::Internal,
        };
        for e in [
            OpenAiConfigError::InvalidBaseUrl {
                reason: "x".into(),
                source: None,
            },
            OpenAiConfigError::InvalidHeader("x".into()),
            OpenAiConfigError::InvalidApiKey,
            OpenAiConfigError::Client(Box::new(Build)),
        ] {
            assert_eq!(e.class(), expected(&e), "{e}");
            assert!(!e.is_retryable());
        }
        let e = OpenAiCompatible::new(config("not a url")).unwrap_err();
        assert!(matches!(
            e,
            OpenAiConfigError::InvalidBaseUrl {
                source: Some(_),
                ..
            }
        ));
        assert!(!e.to_string().contains("not a url"));
    }

    #[test]
    fn bad_headers_and_keys_are_rejected() {
        let mut cfg = config("https://x.example/v1");
        cfg.extra_headers.insert("bad name".into(), "v".into());
        assert!(matches!(
            OpenAiCompatible::new(cfg),
            Err(OpenAiConfigError::InvalidHeader(_))
        ));
        let cfg = OpenAiConfig::new("https://x.example/v1", SecretString::from("line\nbreak"));
        assert!(matches!(
            OpenAiCompatible::new(cfg),
            Err(OpenAiConfigError::InvalidApiKey)
        ));
    }
}
