//! A reusable, durable LLM tool-calling agent for adam-rs.
//!
//! [`LlmAgent`] runs the model <-> tools loop on top of [`adam_runtime`]:
//! every model call and every tool call is a journaled `Ctx::step`, so a
//! restarted worker replays what already happened instead of repeating a side
//! effect or losing history. Any adam-rs agent is then instructions + a model
//! + a toolset:
//!
//! ```
//! use std::sync::Arc;
//! use adam_llm_agent::{LlmAgent, Limits, Tool, ToolCtx, ToolError, ToolOutput};
//! use adam_model::{MockModel, ToolSpec};
//! use async_trait::async_trait;
//! use serde_json::{Value, json};
//!
//! struct Clock;
//!
//! #[async_trait]
//! impl Tool for Clock {
//!     fn spec(&self) -> ToolSpec {
//!         ToolSpec {
//!             name: "clock".into(),
//!             description: "What time is it?".into(),
//!             parameters: json!({"type": "object", "properties": {}}),
//!         }
//!     }
//!     async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
//!         Ok(ToolOutput::text("12:00"))
//!     }
//! }
//!
//! let agent = LlmAgent::builder("assistant", Arc::new(MockModel::new()), "my-model")
//!     .instructions("Be brief.")
//!     .tool(Clock)
//!     .limits(Limits { max_turns: 10, ..Limits::default() })
//!     .build();
//! # let _ = agent;
//! ```
//!
//! Register the agent on a `Runtime` and start a run with
//! [`user_message`]`("...")`.
//!
//! # Inbound messages
//!
//! Anything delivered to a run (and the start input) is read as a user
//! message: [`Inbound`](adam_runtime::Inbound) with kind [`MESSAGE_KIND`] and
//! payload `{"text": "..."}` (a bare JSON string works too). While the run is
//! parked on a [`ToolError::NeedsInput`] question, the first message is that
//! tool call's answer.
//!
//! # Observing a run
//!
//! `Runtime::view(run).state` deserializes into [`Conversation`]: the full
//! history, counters, and `pending_question` while the run waits for input.
//! Live progress arrives as `RunEvent`s; see [`LlmAgent`] for their shapes.

#![warn(missing_docs)]

mod agent;
mod conversation;
mod history;
mod tool;

pub use adam_runtime::Artifact;
pub use agent::{Limits, LlmAgent, LlmAgentBuilder};
pub use conversation::{ArtifactRef, Conversation, MESSAGE_KIND, PendingQuestion, user_message};
pub use history::TRUNCATION_MARKER_PREFIX;
pub use tool::{DynTool, Tool, ToolCtx, ToolError, ToolOutput};
