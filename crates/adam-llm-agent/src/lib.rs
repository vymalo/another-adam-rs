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
//! A process that only accepts requests registers an [`LlmStarter`] of the same
//! name instead: it starts runs (`init` needs no model or tools) and a worker
//! with the [`LlmAgent`] steps them.
//!
//! # Typed tool helpers
//!
//! A tool does not have to build its [`ToolOutput`] and read its arguments by
//! hand. [`parse_args`] reads the model's JSON into a struct (a mistake becomes
//! an error output for the model), [`IntoToolOutput`] / [`IntoToolResult`] let
//! a function return a `String`, [`Json`] or a `Result`, and
//! [`spec_for`] (feature `schema`) derives the argument schema from
//! `schemars::JsonSchema`. Shared dependencies travel as [`State<T>`]:
//! give them to the builder with [`LlmAgentBuilder::state`], read them with
//! [`ToolCtx::state`], declare them in [`Tool::required_state`], and
//! [`LlmAgentBuilder::try_build`] fails at startup when one is missing.
//! [`ToolSet`] / [`tools!`] group tools and [`FnTool`] makes one from a
//! closure. The `#[tool]` macro generates all of this from a function.
//!
//! ```
//! use std::sync::Arc;
//! use adam_llm_agent::{LlmAgent, StateKey, Tool, ToolCtx, ToolError, ToolOutput, parse_args, tools};
//! use adam_model::{MockModel, ToolSpec};
//! use async_trait::async_trait;
//! use serde::Deserialize;
//! use serde_json::{Value, json};
//!
//! struct Greeting(&'static str);
//!
//! #[derive(Deserialize)]
//! struct Args { name: String }
//!
//! struct Greet;
//!
//! #[async_trait]
//! impl Tool for Greet {
//!     fn spec(&self) -> ToolSpec {
//!         ToolSpec {
//!             name: "greet".into(),
//!             description: "Greet someone by name.".into(),
//!             parameters: json!({"type": "object", "properties": {"name": {"type": "string"}},
//!                                "required": ["name"]}),
//!         }
//!     }
//!     fn required_state(&self) -> Vec<StateKey> {
//!         vec![StateKey::of::<Greeting>()]
//!     }
//!     async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
//!         let args: Args = match parse_args("greet", args) {
//!             Ok(args) => args,
//!             Err(refusal) => return Ok(refusal),
//!         };
//!         let greeting = ctx.require_state::<Greeting>()?;
//!         Ok(ToolOutput::text(format!("{}, {}!", greeting.0, args.name)))
//!     }
//! }
//!
//! let agent = LlmAgent::builder("assistant", Arc::new(MockModel::new()), "my-model")
//!     .state(Arc::new(Greeting("Hello")))
//!     .tools(tools![Greet])
//!     .try_build()
//!     .expect("every tool has its state");
//! # let _ = agent;
//! ```
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
mod fn_tool;
mod history;
#[cfg(feature = "schema")]
mod schema;
mod state;
mod tool;
mod toolset;
mod typed;

pub use adam_runtime::Artifact;
pub use agent::{BuildError, Limits, LlmAgent, LlmAgentBuilder, LlmStarter};
pub use conversation::{ArtifactRef, Conversation, MESSAGE_KIND, PendingQuestion, user_message};
pub use fn_tool::FnTool;
#[cfg(feature = "schema")]
pub use fn_tool::{FnToolBuilder, TypedFnToolBuilder};
pub use history::TRUNCATION_MARKER_PREFIX;
#[cfg(feature = "schema")]
pub use schema::{ToolSpecExt, spec_for};
pub use state::{Extensions, State, StateKey};
pub use tool::{DynTool, Tool, ToolCtx, ToolError, ToolOutput};
pub use toolset::ToolSet;
pub use typed::{IntoToolOutput, IntoToolResult, Json, parse_args};
