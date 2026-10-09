//! [`adam_a2a::TaskBackend`] over the durable [`adam_runtime::Runtime`]:
//! serve any adam-rs agent as an A2A agent.
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use adam_a2a::{A2aServer, AgentCardConfig, AuthConfig};
//! # use adam_a2a_runtime::RuntimeTaskBackend;
//! # use adam_runtime::{BroadcastSink, Runtime};
//! # async fn demo(store: adam_core::DynStore, agent: impl adam_runtime::Agent) {
//! let events = BroadcastSink::default();
//! let runtime = Runtime::builder(store)
//!     .agent(agent)
//!     .event_sink(events.clone())
//!     .build();
//! let backend = RuntimeTaskBackend::new(runtime.clone(), events, "my-agent");
//! let card = AgentCardConfig::new("my-agent", "Does things", "http://localhost:8080/".parse().unwrap(), "0.1.0");
//! let app = A2aServer::router(card, Arc::new(backend), AuthConfig::AllowAnonymous);
//! // serve `app` with axum, and run `runtime.run_worker(shutdown)` next to it
//! # let _ = app;
//! # }
//! ```
//!
//! A front process that only accepts tasks can register the agent's
//! `AgentStarter` (`RuntimeBuilder::starter`) instead of the agent: the backend
//! never steps a run, so a worker with the full agent elsewhere does.
//!
//! See [`RuntimeTaskBackend`] for the mapping (tasks are runs), how a new task that
//! references a finished one continues its conversation, ownership and why
//! subscriptions survive restarts.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod backend;
mod convert;
mod ids;
mod list;
mod push;
mod steps;
mod subscribe;
mod text_stream;
mod usage;
mod vymalo;

pub use backend::{DEFAULT_POLL_INTERVAL, MAX_REFERENCES, RuntimeTaskBackend};
pub use convert::{
    InboundFn, PromptFn, artifact_id, artifact_of, default_inbound, default_prompt, task_state,
};
pub use ids::task_id_for;
pub use list::MAX_SCAN;
pub use push::StorePushStore;
pub use usage::MAX_TOKEN_COUNT;
pub use vymalo::{
    CONTEXT_MENTIONS, CONTEXT_THREAD_TOOLS, CONTEXT_UI_CATALOG, CONTEXT_UI_REF,
    MAX_ACTION_CONTEXT_CHARS, MAX_MENTIONS, integral_numbers, vymalo_inbound,
};
