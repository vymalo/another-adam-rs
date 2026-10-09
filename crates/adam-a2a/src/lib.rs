//! Expose an adam-rs agent as an [A2A](https://a2a-protocol.org) server.
//!
//! The crate defines its own backend seam, [`TaskBackend`], so it is
//! independent of the agent runtime; `adam-a2a-runtime` implements it over the
//! durable runtime. [`A2aServer::router`] turns a backend into an [`axum::Router`]
//! that the orchestrator (or any A2A client) can talk to with no adam-specific
//! code.
//!
//! ```no_run
//! # #[cfg(feature = "test-util")] {
//! use std::sync::Arc;
//! use adam_a2a::{A2aServer, AgentCardConfig, AuthConfig, InMemoryBackend};
//! use secrecy::SecretString;
//!
//! # async fn run() -> std::io::Result<()> {
//! let card = AgentCardConfig::new(
//!     "echo", "Echoes what it is told", "http://127.0.0.1:8080/".parse().unwrap(), "0.1.0",
//! );
//! let auth = AuthConfig::BearerTokens(vec![SecretString::from("s3cret")]);
//! let app = A2aServer::router(card, Arc::new(InMemoryBackend::new()), auth);
//! let listener = tokio::net::TcpListener::bind("127.0.0.1:8080").await?;
//! // axum::serve(listener, app).await
//! # Ok(()) }
//! # }
//! ```
//!
//! # Protocol version (deviation from the issue text)
//!
//! The official SDK (`a2a-lf` 0.3, `a2a-server-lf` 0.4) speaks **A2A 1.0
//! only**. It does not implement the 0.3 dialect, and this crate does not
//! hand-roll one. The issue's method names map to the 1.0 ones:
//!
//! | Issue text | A2A 1.0 (served) |
//! |---|---|
//! | `message/send` | `SendMessage` |
//! | `message/stream` | `SendStreamingMessage` |
//! | `tasks/get` | `GetTask` |
//! | `tasks/cancel` | `CancelTask` |
//! | `tasks/resubscribe` | `SubscribeToTask` |
//!
//! States on the wire are `TASK_STATE_*`. The other 1.0 methods are served too, and what is optional is
//! **off by default** and the card says so: `ListTasks` ([`TaskBackend::list`]); the four push-notification
//! methods (the [`push`] module: `PushNotificationNotSupported` unless [`ServerOptions::with_push`]
//! gave a policy that allows a webhook); and `GetExtendedAgentCard` (the public card plus an
//! [`ExtendedCardConfig`], for authenticated callers only: `UnsupportedOperation` without one). The card is signed
//! when [`ServerOptions::with_card_signer`] is given a [`CardSigner`].
//!
//! # Why not the SDK's `DefaultRequestHandler`
//!
//! Its resubscribe only works for a task running in the same process, which
//! contradicts stateless replicas over one durable log. This crate implements
//! the SDK's public `RequestHandler` trait on top of [`TaskBackend`] instead
//! and mounts it with the SDK's own `jsonrpc_router`, `rest_router` and
//! `agent_card_router`, so JSON-RPC and HTTP+JSON parsing, ProtoJSON and SSE
//! framing stay the SDK's job. Nothing in this crate holds task state.
//!
//! # Two bindings, one handler, and the docs
//!
//! The JSON-RPC endpoint (`POST /`) and the HTTP+JSON binding (`POST /message:send`,
//! `GET /tasks/{id}`, ...) are the same handler behind the same layers; the card lists
//! JSON-RPC first and HTTP+JSON second, at the same URL. `GET /docs` serves Swagger UI and
//! `GET /openapi.json` the OpenAPI 3.1 document of both, publicly (the calls need the token);
//! [`ServerOptions::with_docs`] turns them off.
//!
//! # Streaming
//!
//! `SendStreamingMessage` is `submit` followed by `subscribe`; the first frame
//! is the task snapshot, then status/artifact updates, and the stream ends
//! after a terminal state or `input-required`/`auth-required`.
//! `SubscribeToTask` is `subscribe`. A task's work is independent of any
//! connection: a client disconnect drops the subscription and nothing else.
//! The SDK sends an SSE comment frame every 15 s (a constant it does not let
//! us change); [`ServerOptions::with_keepalive_interval`] adds a shorter
//! interval on top.
//!
//! # Authentication
//!
//! [`AuthConfig::BearerTokens`] compares in constant time (SHA-256 digests, so
//! token length is hidden too) and answers 401 with `WWW-Authenticate: Bearer`
//! and an error body (a JSON-RPC error with code `-32000` at `POST /`, since A2A
//! defines no "unauthorized" code; a `google.rpc.Status` `UNAUTHENTICATED` on the
//! HTTP+JSON paths) on every route but the agent card, `/healthz`, the docs and,
//! when the card is signed, its key set.
//! The middleware strips any client-sent identity header and injects the
//! trusted [`Caller`] (`token-<index>` or `anonymous`); the SDK gives request
//! handlers nothing but headers, so that is the channel.
//! [`AuthConfig::AllowAnonymous`] logs a warning at construction.
//!
//! # Data caveat
//!
//! The SDK encodes RPC payloads as ProtoJSON, so numbers inside data parts and
//! metadata come back as floats (`1` becomes `1.0`). Send exact integers and
//! money as strings.
//!
//! # TLS
//!
//! The SDK crates default to rustls with `aws-lc-rs`, which needs a C
//! toolchain to build. TLS is used by the delivery client of the [`push`] module (webhooks
//! over `https`), which is this crate's own (`reqwest`), not the SDK's push sender.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod activation;
mod auth;
mod backend;
mod card;
mod docs;
mod extensions;
mod handler;
#[cfg(feature = "test-util")]
mod memory;
mod openapi;
mod page;
pub mod push;
mod rest;
mod server;
mod signing;

pub use auth::AuthConfig;
pub use backend::{BackendError, Caller, DynTaskBackend, TaskBackend, TaskEvent};
pub use card::{AgentCardConfig, ExtendedCardConfig, ExtensionConfig, SkillConfig};
pub use extensions::{
    A2UI_BASIC_CATALOG_V0_9_1, A2UI_EXTENSION_V0_9_1, A2UI_MEDIA_TYPE, BUILD_EXTENSION,
    MENTIONS_EXTENSION, STEER_EXTENSION, STEPS_EXTENSION, TEXT_STREAM_EXTENSION,
    TEXT_STREAM_KIND_REASONING, THREAD_TOOLS_EXTENSION, UI_CATALOG_EXTENSION, UNKNOWN_REVISION,
    USAGE_EXTENSION, build_version, revision_of,
};
#[cfg(feature = "test-util")]
pub use memory::{InMemoryBackend, InMemoryConfig};
pub use page::{DEFAULT_PAGE_SIZE, MAX_PAGE_SIZE, PageToken, TaskPage, TaskQuery};
pub use server::{A2aServer, SDK_KEEPALIVE_INTERVAL, ServerOptions};
#[cfg(feature = "test-util")]
pub use signing::generate_signing_key_pem;
pub use signing::{
    CardSigner, SigningError, VerifyError, VerifyingKey, canonical_payload, canonicalize,
};
