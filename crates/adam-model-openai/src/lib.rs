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
//!   tokens keep arriving. Expiry is [`Transient`](adam_model::ModelError::Transient). A
//!   connection that cannot be made (refused, no route) is
//!   [`Unreachable`](adam_model::ModelError::Unreachable), class `Transient`; a connection that
//!   times out is a timeout, and a TLS handshake that fails is `Transient`.
//! * **Refusals** ([ADR 0067](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0067-a-model-call-through-an-injector-names-its-run-and-a-refusal-parks-the-run.md)
//!   decision 3). For an answer of status 400 or above the client builds a
//!   [`Refusal`] (the status, the slug of an `application/problem+json`
//!   body's `type`, the `Retry-After`) and asks the provider whether it parks
//!   ([`ModelCredentials::parks`](adam_model::ModelCredentials::parks)). The slug `model_locked`
//!   is [`ModelLocked`](adam_model::ModelError::ModelLocked) for every provider. A park is
//!   [`Refused`](adam_model::ModelError::Refused), which holds no body and no header; no park is
//!   the error the status always gave. An error object inside a stream is the model's and never parks.
//! * **Response size caps.** A body over 16 MiB, an SSE line over 1 MiB, one
//!   event's `data:` over 4 MiB, or a stream over 64 MiB is a
//!   [`Protocol`](adam_model::ModelError::Protocol) error (class `Corrupt`),
//!   checked before the buffer grows past the cap; the stream is then dropped.
//!   The caps are constants, not settings.
//! * **Secrets.** The API key lives in a [`SecretString`]; it is sent as a
//!   sensitive `Authorization: Bearer` header and never appears in `Debug`
//!   output, error messages or logs. Extra header *values* are hidden from
//!   `Debug` too. An empty key sends no `Authorization` header (local servers).
//! * **Credentials per call** ([ADR 0058](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0058-the-models-credentials-come-from-a-provider-asked-for-each-call.md)).
//!   The key is the default [`ModelCredentials`](adam_model::ModelCredentials) provider;
//!   [`OpenAiCompatible::with_credentials`] gives another, asked for the headers of every call and told
//!   about a `401`, after which the call is sent once more if the provider asks.
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
//! * **Reasoning** (*sources and dates in the crate README*): `delta.reasoning_content` (DeepSeek, GLM,
//!   LiteLLM) and `delta.reasoning` (OpenRouter, Ollama, current vLLM) of a stream arrive as
//!   [`ModelDelta::Reasoning`], and `message.reasoning_content` / `message.reasoning` of a
//!   completion as [`ModelResponse::reasoning`]; a value that is not a string is not reasoning
//!   text (it is kept raw in the carrier, [`Message::Assistant`]'s `provider`, and sent back only
//!   when the client echoes reasoning). It is
//!   never the answer's text and, unless [`with_echo_reasoning`](OpenAiCompatible::with_echo_reasoning)
//!   is set, never in the history and never sent back.
//! * **[`with_prompt_cache_key`](OpenAiCompatible::with_prompt_cache_key)** (off by default) sends
//!   a request's `prompt_cache_key`; a member of the extra body of the same name wins.
//! * **[`with_extra_body`](OpenAiCompatible::with_extra_body)** merges a JSON object into every
//!   request, for a flag that makes a model emit reasoning.
//! * **[`with_profiles`](OpenAiCompatible::with_profiles)** gives the client the deployment's
//!   per-model profiles (`MODEL_ALIASES_FILE`, ADR 0063 and ADR 0064): a request's
//!   `reasoning_effort` goes out through the profile's `reasoningWire`, after the extra body, which
//!   it wins over where the two would duplicate a member.

#![warn(missing_docs)]

mod errors;
mod sse;
mod wire;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use adam_error::{BoxError, Classify, ErrorClass};
use adam_model::{
    AfterRejection, CredentialHeaders, CredentialRequest, DynCredentials, InputModalities,
    MODEL_LOCKED_SLUG, Message, ModelClient, ModelDelta, ModelError, ModelProfile, ModelProfiles,
    ModelRequest, ModelResponse, Park, Refusal, Rejection, StaticBearer,
};
use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{BoxStream, StreamExt};
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use secrecy::{ExposeSecret, SecretString};

/// `url` as a log line may show it: its scheme, host and port, nothing else. A gateway's address is
/// often a secret of the deployment (an internal host, a path that names a tenant, a key in the
/// query or the user part), so no `Debug` of this crate or of a configuration built on it prints
/// more. `"<unset>"` for a blank `url` and `"<set>"` for one that is not an absolute URL with a
/// host, which says that something is configured and nothing of what.
#[must_use]
pub fn endpoint_for_logs(url: &str) -> String {
    let url = url.trim();
    if url.is_empty() {
        return "<unset>".to_owned();
    }
    match reqwest::Url::parse(url) {
        Ok(parsed) => match parsed.host_str() {
            Some(host) => match parsed.port() {
                Some(port) => format!("{}://{host}:{port}", parsed.scheme()),
                None => format!("{}://{host}", parsed.scheme()),
            },
            None => "<set>".to_owned(),
        },
        Err(_) => "<set>".to_owned(),
    }
}

/// The stable id of the endpoint at `base`: `openai:` and the first 16 hexadecimal digits of the
/// SHA-256 of the normalised base URL. A provider's opaque members go back only to the endpoint
/// (and model alias) that produced them, and this is how a client recognises its own. It is not a
/// secret, but a fingerprint of the base URL: it confirms a guess.
fn endpoint_id(base: &str) -> String {
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(base.as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("openai:{}", &hex[..16])
}

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
            // Scheme and host only: see `endpoint_for_logs`.
            .field("base_url", &endpoint_for_logs(&self.base_url))
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

/// The name of the member that carries a model's reasoning when a client sends it back
/// ([`OpenAiCompatible::with_echo_reasoning`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningField {
    /// `reasoning_content`: DeepSeek, GLM (Z.ai), LiteLLM.
    ReasoningContent,
    /// `reasoning`: OpenRouter, current vLLM.
    Reasoning,
}

impl ReasoningField {
    /// The JSON member name.
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::ReasoningContent => "reasoning_content",
            Self::Reasoning => "reasoning",
        }
    }
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
    /// The extra request body sets a member the runtime owns.
    #[error("the extra request body may not set `{0}`: the runtime owns it")]
    ReservedBodyKey(String),
    /// The CA file of the model endpoint (`MODEL_CA_FILE`) cannot be used: it cannot be read, is
    /// not PEM, holds no certificate, or holds one the TLS backend refuses.
    #[error("the model endpoint's CA file cannot be used: {reason}")]
    InvalidCaFile {
        /// What is wrong with it.
        reason: String,
        /// The reader's or the TLS backend's own error, when there is one.
        #[source]
        source: Option<BoxError>,
    },
    /// The CA file of the model endpoint is larger than [`MAX_CA_FILE_BYTES`]: no bundle of
    /// roots is, so it is not one.
    #[error("the model endpoint's CA file is over {limit} bytes")]
    CaFileTooLarge {
        /// The cap, [`MAX_CA_FILE_BYTES`].
        limit: u64,
    },
    /// An entry of `extra_headers` names a header that carries a credential (`authorization`,
    /// `proxy-authorization`): credentials come from the provider alone ([ADR 0058], and ADR 0067
    /// decision 1: with an injector nothing in the process may send one).
    ///
    /// [ADR 0058]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0058-the-models-credentials-come-from-a-provider-asked-for-each-call.md
    #[error("extra header `{0}` carries a credential: credentials come from the provider")]
    CredentialHeader(String),
    /// The HTTP client could not be built (for example, no TLS backend).
    #[error("could not build the HTTP client")]
    Client(#[source] BoxError),
}

impl Classify for OpenAiConfigError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::InvalidBaseUrl { .. }
            | Self::InvalidHeader(_)
            | Self::InvalidApiKey
            | Self::InvalidCaFile { .. }
            | Self::CaFileTooLarge { .. }
            | Self::CredentialHeader(_)
            | Self::ReservedBodyKey(_) => ErrorClass::Invalid,
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
    /// What the HTTP client was built with, so [`with_extra_roots`](Self::with_extra_roots) can
    /// build it again.
    default_headers: HeaderMap,
    url: String,
    /// The stable id of the endpoint, from its base URL: what a provider's opaque members are
    /// bound to, beside the model alias. Not a secret, but a fingerprint of the base URL: it
    /// confirms a guess.
    endpoint: String,
    credentials: DynCredentials,
    timeout: Duration,
    max_tokens_field: MaxTokensField,
    extra_body: Option<serde_json::Map<String, serde_json::Value>>,
    echo_reasoning: Option<ReasoningField>,
    prompt_cache_key: bool,
    context_windows: BTreeMap<String, u64>,
    input_modalities: BTreeMap<String, InputModalities>,
    /// The deployment's per-model profiles (`MODEL_ALIASES_FILE`): what the reasoning wire applies
    /// and what [`ModelClient::profile`] answers. Empty by default.
    profiles: ModelProfiles,
}

/// What [`OpenAiCompatible`] says its provider is ([`ModelClient::provider`]): the protocol it
/// speaks, whatever gateway or model is behind it.
pub const PROVIDER: &str = "openai";

impl fmt::Debug for OpenAiCompatible {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiCompatible")
            .field("url", &endpoint_for_logs(&self.url))
            // A provider's own `Debug` decides what it shows; the shipped ones show no value.
            .field("credentials", &"[provider]")
            .field("timeout", &self.timeout)
            .field("max_tokens_field", &self.max_tokens_field)
            // Which members, not their values: an extra body is not a secret, but it is not a log line.
            .field(
                "extra_body",
                &self
                    .extra_body
                    .as_ref()
                    .map(|m| m.keys().collect::<Vec<_>>()),
            )
            .field("echo_reasoning", &self.echo_reasoning)
            .field("prompt_cache_key", &self.prompt_cache_key)
            .field("context_windows", &self.context_windows)
            .field("input_modalities", &self.input_modalities)
            // The model ids, never a profile's values.
            .field(
                "profiles",
                &self.profiles.profiles.keys().collect::<Vec<_>>(),
            )
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
            if header_name == reqwest::header::AUTHORIZATION
                || header_name == reqwest::header::PROXY_AUTHORIZATION
            {
                return Err(OpenAiConfigError::CredentialHeader(name.clone()));
            }
            let mut header_value = HeaderValue::from_str(value)
                .map_err(|_| OpenAiConfigError::InvalidHeader(name.clone()))?;
            header_value.set_sensitive(true);
            headers.insert(header_name, header_value);
        }
        // The key is no default header: it is the default provider (ADR 0058), asked for each call
        // like any other, so `with_credentials` replaces it. Checked here, so a bad key still fails
        // at build time and never at the first call.
        let key = config.api_key.expose_secret();
        if !key.is_empty() {
            HeaderValue::from_str(&format!("Bearer {key}"))
                .map_err(|_| OpenAiConfigError::InvalidApiKey)?;
        }

        let http = build_http(&headers, config.timeout, None)
            .map_err(|e| OpenAiConfigError::Client(Box::new(e)))?;

        Ok(Self {
            http,
            default_headers: headers,
            url: format!("{base}/chat/completions"),
            endpoint: endpoint_id(base),
            credentials: Arc::new(StaticBearer::new(config.api_key.clone())),
            timeout: config.timeout,
            max_tokens_field: MaxTokensField::default(),
            extra_body: None,
            echo_reasoning: None,
            prompt_cache_key: false,
            context_windows: BTreeMap::new(),
            input_modalities: BTreeMap::new(),
            profiles: ModelProfiles::default(),
        })
    }

    /// Say that the model the alias `model` names has a context window of `window` tokens, as the
    /// deployment knows it (`MODEL_CONTEXT_WINDOW`): what [`ModelClient::context_window`] answers
    /// for that alias, and nothing for any other. The client sends nothing of it; a report of a
    /// call says it, so a screen can show how full the context is.
    #[must_use]
    pub fn with_context_window(mut self, model: impl Into<String>, window: u64) -> Self {
        self.context_windows.insert(model.into(), window);
        self
    }

    /// Give the client the deployment's per-model profiles (`MODEL_ALIASES_FILE`, ADR 0063 and
    /// ADR 0064): what [`ModelClient::profile`] answers, and how a request's
    /// [`reasoning_effort`](adam_model::ModelRequest::reasoning_effort) is sent — through the
    /// `reasoningWire` of the request's model, after the extra body (the crate README, *Reasoning
    /// effort*). Empty by default: no model has a profile, and nothing is applied.
    #[must_use]
    pub fn with_profiles(mut self, profiles: ModelProfiles) -> Self {
        self.profiles = profiles;
        self
    }

    /// Say that the model the alias `model` can read what `modalities` says, as the deployment
    /// declared it (`MODEL_INPUT_MODALITIES`): what [`ModelClient::input_modalities`] answers for
    /// that alias, and text only for any other, which fails closed. The client sends an image
    /// only as a `data:` URL part of a message and only when the request carries its bytes
    /// ([`ModelRequest::image_data`](adam_model::ModelRequest::image_data)).
    #[must_use]
    pub fn with_input_modalities(
        mut self,
        model: impl Into<String>,
        modalities: InputModalities,
    ) -> Self {
        self.input_modalities.insert(model.into(), modalities);
        self
    }

    /// Members merged into the body of **every** request, at its top level (they win over what
    /// the client wrote, except the members a typed reasoning level's wire duplicates, where the
    /// level wins: ADR 0063 decision 6): a flag that makes a gateway or a model emit its reasoning,
    /// for example `{"reasoning_effort": "medium"}`, `{"thinking": {"type": "enabled"}}` or
    /// `{"chat_template_kwargs": {"enable_thinking": true}}`. Not secret: it is sent as written
    /// and is not hidden from `Debug` beyond its member names. An empty object is no extra body.
    ///
    /// # Errors
    ///
    /// [`OpenAiConfigError::ReservedBodyKey`] for a member the client owns: `model`, `messages`,
    /// `tools`, `tool_choice` and `stream`.
    pub fn with_extra_body(
        mut self,
        extra: serde_json::Map<String, serde_json::Value>,
    ) -> Result<Self, OpenAiConfigError> {
        if let Some(key) = wire::RESERVED_BODY_KEYS
            .iter()
            .find(|key| extra.contains_key(**key))
        {
            return Err(OpenAiConfigError::ReservedBodyKey((*key).to_owned()));
        }
        self.extra_body = Some(extra).filter(|extra| !extra.is_empty());
        Ok(self)
    }

    /// Send the reasoning back: a model's reasoning is kept in the history
    /// ([`Message::Assistant::reasoning`](adam_model::Message)) and goes out on its assistant
    /// message under `field`'s name. **Off by default**, which is what almost every provider wants
    /// (the reasoning of an earlier turn is not part of the conversation); on, for a provider that
    /// requires it: DeepSeek's thinking mode with tools answers a request without the reasoning of
    /// its earlier turns with a 400 (*verified 2026-10-05*,
    /// <https://api-docs.deepseek.com/guides/thinking_mode>). On, the reasoning is also in the
    /// run's stored history, and so in every later request.
    pub fn with_echo_reasoning(mut self, field: Option<ReasoningField>) -> Self {
        self.echo_reasoning = field;
        self
    }

    /// Send [`ModelRequest::prompt_cache_key`](adam_model::ModelRequest::prompt_cache_key), when a
    /// request has one, as the body's `prompt_cache_key`: OpenAI's way to route the requests of one
    /// conversation to the same prompt cache. **Off by default**: a gateway or a server that
    /// rejects an unknown member would fail every call (*unverified* for each: most ignore
    /// it). A member of [`with_extra_body`](Self::with_extra_body) of the same name wins.
    #[must_use]
    pub fn with_prompt_cache_key(mut self, on: bool) -> Self {
        self.prompt_cache_key = on;
        self
    }

    fn options(&self) -> wire::RequestOptions<'_> {
        wire::RequestOptions {
            max_tokens_field: self.max_tokens_field,
            extra_body: self.extra_body.as_ref(),
            echo_reasoning: self.echo_reasoning,
            prompt_cache_key: self.prompt_cache_key,
            endpoint: &self.endpoint,
            profiles: Some(&self.profiles),
        }
    }

    /// `response` as this client reports it: the reasoning is kept in the message only when the
    /// client echoes it (a raw block is cleared otherwise, ADR 0020), and the provider's opaque
    /// members are stamped with the model alias and endpoint that produced them (a parse never
    /// stamps: it does not know either).
    fn echoed(&self, mut response: ModelResponse, model: &str) -> ModelResponse {
        let echo = self.echo_reasoning.is_some();
        if let Message::Assistant {
            reasoning,
            provider,
            tool_calls,
            ..
        } = &mut response.message
        {
            if echo {
                reasoning.clone_from(&response.reasoning);
            } else if let Some(turn) = provider.as_mut() {
                // A structured reasoning block is only meaningful when it is sent back; off (the
                // default) nothing of it is stored (ADR 0020). The opaque members stay: they are
                // signatures, not reasoning.
                turn.reasoning = None;
            }
            // A turn with nothing left is no carrier: a response that only had a block, with echo
            // off, is the bytes it always was.
            let calls_opaque = tool_calls.iter().any(|call| {
                call.opaque
                    .as_ref()
                    .is_some_and(|members| !members.is_empty())
            });
            if provider.as_ref().is_some_and(|turn| {
                turn.members.is_empty() && turn.reasoning.is_none() && !calls_opaque
            }) {
                *provider = None;
            }
        }
        if let Message::Assistant {
            provider: Some(provider),
            ..
        } = &mut response.message
        {
            provider.model = model.to_owned();
            self.endpoint.clone_into(&mut provider.endpoint);
        }
        response
    }

    /// Choose the JSON field used for `max_output_tokens`.
    pub fn with_max_tokens_field(mut self, field: MaxTokensField) -> Self {
        self.max_tokens_field = field;
        self
    }

    /// Ask `credentials` for the headers of every call instead of sending `api_key` (ADR 0058):
    /// a host that mints, renews or proves a credential at the moment of use, or an injector's run
    /// header with no key at all. It replaces the key of the [`OpenAiConfig`]. On a `401` the
    /// provider is told why ([`ModelCredentials::rejected`](adam_model::ModelCredentials::rejected)) and, if it asks, the call is sent once
    /// more with new headers; a second `401` is the call's answer.
    #[must_use]
    pub fn with_credentials(mut self, credentials: DynCredentials) -> Self {
        self.credentials = credentials;
        self
    }

    /// Trust `roots` for the model endpoint, beside the default roots (`MODEL_CA_FILE`, ADR 0067):
    /// a gateway inside a cluster whose certificate no public root store holds. The HTTP client is
    /// built again with them; the default roots are kept.
    ///
    /// # Errors
    ///
    /// [`OpenAiConfigError::InvalidCaFile`] when the TLS backend refuses them.
    pub fn with_extra_roots(mut self, roots: &ExtraRoots) -> Result<Self, OpenAiConfigError> {
        self.http = build_http(&self.default_headers, self.timeout, Some(roots))
            .map_err(|e| ca_error("the TLS backend refused it", Some(Box::new(e))))?;
        Ok(self)
    }

    /// Send the request of the run `run` for the model `model` and return the response if it is a
    /// success.
    async fn send(
        &self,
        body: Vec<u8>,
        streaming: bool,
        model: &str,
        run: Option<&str>,
    ) -> Result<reqwest::Response, ModelError> {
        let body = Bytes::from(body);
        let mut attempt = 0;
        let response = loop {
            let call = CredentialRequest {
                method: "POST",
                url: &self.url,
                attempt,
                run,
            };
            let credentials = self.credentials.headers(call).await?;
            let mut request = self
                .http
                .post(&self.url)
                .headers(credential_headers(&credentials)?)
                .header(CONTENT_TYPE, "application/json")
                .body(body.clone());
            if streaming {
                request = request.header(ACCEPT, "text/event-stream");
            } else {
                request = request.timeout(self.timeout);
            }

            // For streams the timeout covers only the wait for response headers.
            let response = tokio::time::timeout(self.timeout, request.send())
                .await
                .map_err(|_| {
                    ModelError::transient(format!("no response within {:?}", self.timeout))
                })?
                .map_err(transport_error)?;
            if response.status() != reqwest::StatusCode::UNAUTHORIZED || attempt > 0 {
                break response;
            }
            let rejection = rejection_of(&response);
            if self.credentials.rejected(call, &rejection).await == AfterRejection::GiveUp {
                break response;
            }
            attempt += 1;
        };

        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| errors::parse_retry_after(v, SystemTime::now()));
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let text = tokio::time::timeout(self.timeout, read_body(response))
            .await
            .ok()
            .and_then(Result::ok)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        // A refusal (ADR 0067 decision 3) is read by its problem type, and its provider says whether
        // it parks the run. A gateway refuses before a response streams, so this is the only place
        // a refusal is read: an error object inside a stream is the upstream model's (`sse.rs`,
        // `errors::from_error_object`) and never parks.
        if status.as_u16() >= 400 {
            let refusal = Refusal::new(status.as_u16())
                .with_problem(errors::problem_type(content_type.as_deref(), &text))
                .with_retry_after(retry_after);
            if refusal.slug() == Some(MODEL_LOCKED_SLUG) {
                return Err(ModelError::ModelLocked {
                    model: model.to_owned(),
                });
            }
            match self.credentials.parks(&refusal) {
                Park::No => {}
                // The status, the slug and the wait only: the body and the headers stay here.
                park => {
                    return Err(ModelError::Refused {
                        status: refusal.status,
                        slug: refusal.slug().map(str::to_owned),
                        retry_after: refusal.retry_after,
                        park,
                    });
                }
            }
        }
        Err(errors::from_http(status, retry_after, &text))
    }
}

/// The HTTP client of the model endpoint: `headers` on every request, no redirect, and `roots`
/// trusted beside the default roots.
fn build_http(
    headers: &HeaderMap,
    timeout: Duration,
    roots: Option<&ExtraRoots>,
) -> Result<reqwest::Client, reqwest::Error> {
    let mut builder = reqwest::Client::builder()
        .default_headers(headers.clone())
        .user_agent(concat!("adam-model-openai/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(timeout)
        // Never replay a POST carrying credentials somewhere else.
        .redirect(reqwest::redirect::Policy::none());
    if let Some(roots) = roots {
        builder = builder.tls_certs_merge(roots.0.iter().cloned());
    }
    builder.build().map_err(reqwest::Error::without_url)
}

/// The largest CA file [`ExtraRoots::read`] reads: 1 MiB, far above any bundle of roots.
pub const MAX_CA_FILE_BYTES: u64 = 1 << 20;

fn ca_error(reason: &str, source: Option<BoxError>) -> OpenAiConfigError {
    OpenAiConfigError::InvalidCaFile {
        reason: reason.to_owned(),
        source,
    }
}

/// Certificates trusted for the model endpoint beside the default roots: the PEM file of
/// `MODEL_CA_FILE` ([ADR 0067](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0067-a-model-call-through-an-injector-names-its-run-and-a-refusal-parks-the-run.md)
/// decision 9), for [`OpenAiCompatible::with_extra_roots`]. Only the model endpoint's client
/// trusts them. Its `Debug` says how many.
#[derive(Clone)]
pub struct ExtraRoots(Vec<reqwest::Certificate>);

impl ExtraRoots {
    /// The certificates of a PEM bundle. Checked here, with the TLS backend, so a bad file fails
    /// at startup and never at the first call.
    ///
    /// # Errors
    ///
    /// [`OpenAiConfigError::InvalidCaFile`] when the text is not PEM, holds no certificate, or
    /// holds one the TLS backend refuses.
    pub fn from_pem(pem: &[u8]) -> Result<Self, OpenAiConfigError> {
        let certs = reqwest::Certificate::from_pem_bundle(pem)
            .map_err(|e| ca_error("it is not PEM", Some(Box::new(e.without_url()))))?;
        if certs.is_empty() {
            return Err(ca_error("it holds no certificate", None));
        }
        let roots = Self(certs);
        build_http(&HeaderMap::new(), Duration::from_secs(1), Some(&roots)).map_err(|e| {
            ca_error(
                "it holds a certificate the TLS backend refuses",
                Some(Box::new(e)),
            )
        })?;
        Ok(roots)
    }

    /// The certificates of the PEM file at `path`.
    ///
    /// # Errors
    ///
    /// [`OpenAiConfigError::InvalidCaFile`] when the file cannot be read, or as
    /// [`from_pem`](Self::from_pem); [`OpenAiConfigError::CaFileTooLarge`] for a file over
    /// [`MAX_CA_FILE_BYTES`], which is not read past the cap.
    pub fn read(path: &std::path::Path) -> Result<Self, OpenAiConfigError> {
        use std::io::Read as _;
        let unreadable = |e: std::io::Error| {
            let reason = format!("it cannot be read: {e}");
            ca_error(&reason, Some(Box::new(e)))
        };
        let file = std::fs::File::open(path).map_err(unreadable)?;
        let mut pem = Vec::new();
        file.take(MAX_CA_FILE_BYTES + 1)
            .read_to_end(&mut pem)
            .map_err(unreadable)?;
        if pem.len() as u64 > MAX_CA_FILE_BYTES {
            return Err(OpenAiConfigError::CaFileTooLarge {
                limit: MAX_CA_FILE_BYTES,
            });
        }
        Self::from_pem(&pem)
    }

    /// How many certificates.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether there is none (never, for a value [`from_pem`](Self::from_pem) gave).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for ExtraRoots {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ExtraRoots").field(&self.0.len()).finish()
    }
}

/// The most bytes one response body may carry, read whole or streamed.
pub(crate) const MAX_BODY_BYTES: usize = 16 << 20;

/// Read a response body, refusing one over [`MAX_BODY_BYTES`] before it grows past it.
async fn read_body(mut response: reqwest::Response) -> Result<Vec<u8>, ModelError> {
    let mut body = BoundedBody::new(MAX_BODY_BYTES);
    if response
        .content_length()
        .is_some_and(|len| len > MAX_BODY_BYTES as u64)
    {
        return Err(body.over());
    }
    while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
        body.push(&chunk)?;
    }
    Ok(body.bytes)
}

/// A body being read under a cap.
struct BoundedBody {
    bytes: Vec<u8>,
    cap: usize,
}

impl BoundedBody {
    fn new(cap: usize) -> Self {
        Self {
            bytes: Vec::new(),
            cap,
        }
    }

    fn push(&mut self, chunk: &[u8]) -> Result<(), ModelError> {
        if self.bytes.len() + chunk.len() > self.cap {
            return Err(self.over());
        }
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }

    fn over(&self) -> ModelError {
        ModelError::protocol(format!("the model sent a body over {} bytes", self.cap))
    }
}

/// The headers a provider gave, as a request's: names checked, values sensitive (never in `Debug`).
/// A provider may not set the headers that frame the request.
fn credential_headers(credentials: &CredentialHeaders) -> Result<HeaderMap, ModelError> {
    const FRAMING: [&str; 5] = [
        "content-type",
        "content-length",
        "accept",
        "host",
        "transfer-encoding",
    ];
    let mut headers = HeaderMap::new();
    for (name, value) in credentials.iter() {
        let refused = || {
            ModelError::invalid_request(format!(
                "the credential provider gave a header that cannot be sent: {name}"
            ))
        };
        if FRAMING.iter().any(|f| name.eq_ignore_ascii_case(f)) {
            return Err(refused());
        }
        let header_name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| refused())?;
        let mut header_value =
            HeaderValue::from_str(value.expose_secret()).map_err(|_| refused())?;
        header_value.set_sensitive(true);
        headers.insert(header_name, header_value);
    }
    Ok(headers)
}

/// What a provider is told about a `401`.
fn rejection_of(response: &reqwest::Response) -> Rejection {
    let text = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    Rejection {
        status: response.status().as_u16(),
        www_authenticate: text("www-authenticate"),
        dpop_nonce: text("dpop-nonce"),
    }
}

/// Whether `err`'s chain holds a [`rustls::Error`]. `tokio-rustls` wraps it in an `io::Error`, and
/// hyper-rustls wraps that in another one; an `io::Error` built around an error does not give it as
/// its `source` (it gives that error's source), so each `io::Error` is opened with `get_ref`.
fn is_tls_failure(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(err);
    while let Some(err) = current {
        if err.is::<rustls::Error>() {
            return true;
        }
        if let Some(inner) = err
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::get_ref)
            && is_tls_failure(inner)
        {
            return true;
        }
        current = err.source();
    }
    false
}

fn transport_error(e: reqwest::Error) -> ModelError {
    // The URL is not secret, but it is noise; the key is never in it.
    let e = e.without_url();
    // A timeout first: a connection that timed out is a timeout, never `Unreachable` (ADR 0067
    // decision 3: a timeout keeps today's retries and failure, and never parks a run). A failed TLS
    // handshake (a CA not trusted, a name that does not match) is no endpoint that waiting brings
    // back: `Transient`, as before, never `Unreachable`.
    if e.is_timeout() {
        ModelError::transient("request timed out").with_source(e)
    } else if e.is_connect() && is_tls_failure(&e) {
        ModelError::transient("TLS handshake failed").with_source(e)
    } else if e.is_connect() {
        ModelError::unreachable().with_source(e)
    } else if e.is_builder() {
        ModelError::invalid_request("the request could not be built").with_source(e)
    } else {
        ModelError::transient("transport error").with_source(e)
    }
}

#[async_trait]
impl ModelClient for OpenAiCompatible {
    fn provider(&self) -> Option<&str> {
        Some(PROVIDER)
    }

    fn context_window(&self, model: &str) -> Option<u64> {
        self.context_windows.get(model).copied()
    }

    fn input_modalities(&self, model: &str) -> InputModalities {
        self.input_modalities
            .get(model)
            .cloned()
            .unwrap_or_default()
    }

    fn profile(&self, model: &str) -> Option<&ModelProfile> {
        self.profiles.profile(model)
    }

    #[tracing::instrument(name = "model.complete", skip_all, fields(model = %req.model, url = %self.url))]
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse, ModelError> {
        let body = wire::build_request(&req, false, self.options())?;
        let response = self
            .send(body, false, &req.model, req.run.as_deref())
            .await?;
        let bytes = read_body(response).await?;
        let parsed = wire::parse_completion(&bytes).map(|r| self.echoed(r, &req.model));
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
        let body = wire::build_request(&req, true, self.options())?;
        let response = self
            .send(body, true, &req.model, req.run.as_deref())
            .await?;
        let this = self.clone();
        let model = req.model.clone();
        Ok(sse::deltas(response.bytes_stream(), self.timeout)
            .map(move |item| {
                item.map(|delta| match delta {
                    ModelDelta::Finished(response) => {
                        ModelDelta::Finished(this.echoed(response, &model))
                    }
                    other => other,
                })
            })
            .boxed())
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
    fn the_provider_is_openai_and_a_window_is_said_for_its_alias_only() {
        let client = OpenAiCompatible::new(config("https://x.example/v1")).unwrap();
        assert_eq!(client.provider(), Some(PROVIDER));
        assert_eq!(client.context_window("glm-5.3"), None);
        let client = client.with_context_window("glm-5.3", 131_072);
        assert_eq!(client.context_window("glm-5.3"), Some(131_072));
        assert_eq!(client.context_window("another"), None);
        let shared: adam_model::DynModel = std::sync::Arc::new(client);
        assert_eq!(shared.provider(), Some("openai"));
        assert_eq!(shared.context_window("glm-5.3"), Some(131_072));
    }

    #[test]
    fn the_profiles_are_answered_for_their_models_and_debug_shows_ids_only() {
        let profiles = ModelProfiles::parse_json(
            r#"{"profiles":{"large":{"reasoningEfforts":["low","medium"],"reasoningWire":"reasoning_effort"}}}"#,
        )
        .unwrap();
        let client = OpenAiCompatible::new(config("https://x.example/v1")).unwrap();
        assert_eq!(client.profile("large"), None, "no profiles by default");
        let client = client.with_profiles(profiles);
        assert_eq!(
            client.profile("large").and_then(|p| p.reasoning_wire),
            Some(adam_model::ReasoningWire::ReasoningEffort)
        );
        assert_eq!(client.profile("small"), None);
        // The ids, never a profile's values.
        let shown = format!("{client:?}");
        assert!(shown.contains("large"), "{shown}");
        assert!(!shown.contains("reasoning_effort"), "{shown}");
        // A `DynModel` forwards it.
        let shared: adam_model::DynModel = std::sync::Arc::new(client);
        assert!(shared.profile("large").is_some());
        assert!(shared.profile("small").is_none());
    }

    #[test]
    fn debug_shows_the_gateway_by_scheme_and_host_only() {
        // The gateway's address is a secret of the deployment: a path, a query and a user part
        // never reach a log line.
        let secret = "https://user:pw@gw.internal:8443/tenant-4711/v1?key=k-123";
        let shown = format!("{:?}", config(secret));
        assert!(shown.contains("https://gw.internal:8443"), "{shown}");
        for hidden in ["tenant-4711", "k-123", "user", "pw", "/v1"] {
            assert!(!shown.contains(hidden), "{hidden} leaked: {shown}");
        }
        let client = OpenAiCompatible::new(config(secret)).unwrap();
        let shown = format!("{client:?}");
        assert!(shown.contains("https://gw.internal:8443"), "{shown}");
        for hidden in ["tenant-4711", "k-123", "pw", "chat/completions"] {
            assert!(!shown.contains(hidden), "{hidden} leaked: {shown}");
        }
        assert_eq!(
            endpoint_for_logs("https://api.example.com/v1"),
            "https://api.example.com"
        );
        assert_eq!(
            endpoint_for_logs("http://localhost:8080/v1"),
            "http://localhost:8080"
        );
        assert_eq!(endpoint_for_logs("  "), "<unset>");
        assert_eq!(endpoint_for_logs("not a url"), "<set>");
        assert_eq!(endpoint_for_logs("mailto:someone@example.com"), "<set>");
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
    fn the_endpoint_id_is_stable_short_and_never_the_url() {
        let first = endpoint_id("https://gw.example/v1");
        assert_eq!(first, endpoint_id("https://gw.example/v1"), "stable");
        assert!(first.starts_with("openai:"), "{first}");
        assert_eq!(first.len(), "openai:".len() + 16, "{first}");
        assert!(
            first["openai:".len()..]
                .chars()
                .all(|c| c.is_ascii_hexdigit())
        );
        assert!(!first.contains("gw.example"), "{first}");
        assert_ne!(first, endpoint_id("https://gw.example/v2"), "another path");
        assert_ne!(
            first,
            endpoint_id("https://other.example/v1"),
            "another host"
        );
        // The client trims a trailing slash before it hashes, so the two spellings are one
        // endpoint.
        let plain = OpenAiCompatible::new(config("https://x.example/v1")).unwrap();
        let slashed = OpenAiCompatible::new(config("https://x.example/v1/")).unwrap();
        assert_eq!(plain.endpoint, slashed.endpoint);
    }

    #[test]
    fn a_body_is_refused_before_it_grows_past_the_cap() {
        let mut body = BoundedBody::new(8);
        body.push(b"12345").expect("5 of 8");
        body.push(b"678").expect("exactly 8");
        let err = body.push(b"9").expect_err("the 9th byte");
        assert!(matches!(err, ModelError::Protocol { .. }), "{err:?}");
        assert!(err.to_string().contains("over 8 bytes"), "{err}");
        assert_eq!(body.bytes, b"12345678");

        // One chunk over the cap is refused whole: nothing is copied first.
        let mut body = BoundedBody::new(4);
        let err = body.push(b"12345").expect_err("one chunk over");
        assert!(err.to_string().contains("over 4 bytes"), "{err}");
        assert!(body.bytes.is_empty());
    }

    #[test]
    fn the_default_body_cap_is_the_documented_one() {
        assert_eq!(MAX_BODY_BYTES, 16 << 20, "one body: 16 MiB");
    }

    #[test]
    fn a_rustls_error_is_found_however_it_is_wrapped() {
        #[derive(Debug, thiserror::Error)]
        #[error("outer")]
        struct Outer(#[source] std::io::Error);
        let tls = || rustls::Error::General("bad certificate".into());
        // As tokio-rustls and hyper-rustls wrap it.
        let wrapped =
            std::io::Error::other(std::io::Error::new(std::io::ErrorKind::InvalidData, tls()));
        assert!(is_tls_failure(&wrapped));
        assert!(is_tls_failure(&Outer(wrapped)));
        assert!(is_tls_failure(&tls()));
        let refused = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        assert!(!is_tls_failure(&refused));
        assert!(!is_tls_failure(&Outer(std::io::Error::other("no route"))));
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
            OpenAiConfigError::ReservedBodyKey(_) => ErrorClass::Invalid,
            OpenAiConfigError::InvalidCaFile { .. } => ErrorClass::Invalid,
            OpenAiConfigError::CaFileTooLarge { .. } => ErrorClass::Invalid,
            OpenAiConfigError::CredentialHeader(_) => ErrorClass::Invalid,
            OpenAiConfigError::Client(_) => ErrorClass::Internal,
        };
        for e in [
            OpenAiConfigError::InvalidBaseUrl {
                reason: "x".into(),
                source: None,
            },
            OpenAiConfigError::InvalidHeader("x".into()),
            OpenAiConfigError::InvalidApiKey,
            OpenAiConfigError::ReservedBodyKey("model".into()),
            OpenAiConfigError::InvalidCaFile {
                reason: "x".into(),
                source: None,
            },
            OpenAiConfigError::CaFileTooLarge { limit: 1 },
            OpenAiConfigError::CredentialHeader("authorization".into()),
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
        // A credential comes from the provider alone (ADR 0067 decision 1): no extra header may
        // carry one, whatever its case.
        for name in ["authorization", "Authorization", "PROXY-AUTHORIZATION"] {
            let mut cfg = config("https://x.example/v1");
            cfg.extra_headers.insert(name.into(), "Bearer extra".into());
            assert!(
                matches!(
                    OpenAiCompatible::new(cfg),
                    Err(OpenAiConfigError::CredentialHeader(ref n)) if n == name
                ),
                "{name}"
            );
        }
        let cfg = OpenAiConfig::new("https://x.example/v1", SecretString::from("line\nbreak"));
        assert!(matches!(
            OpenAiCompatible::new(cfg),
            Err(OpenAiConfigError::InvalidApiKey)
        ));
    }
}
