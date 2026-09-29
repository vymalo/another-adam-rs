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
//! See [`RuntimeTaskBackend`] for the mapping (tasks are runs), ownership and
//! why subscriptions survive restarts.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod backend;
mod convert;
mod subscribe;

pub use backend::{DEFAULT_POLL_INTERVAL, RuntimeTaskBackend};
pub use convert::{
    InboundFn, PromptFn, artifact_id, artifact_of, default_inbound, default_prompt, task_state,
};
