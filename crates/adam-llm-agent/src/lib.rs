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
//! closure. The `#[tool]` macro (crate `adam-macros`, through the `adam` facade)
//! generates all of this from a function.
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
//! # Continuing a conversation
//!
//! A run that the runtime starts as the continuation of another
//! (`Runtime::start_with_id_continuing`; over A2A, a new task that references the one before it)
//! starts from that run's conversation: [`LlmAgent`] and [`LlmStarter`] implement
//! `init_continuing` with [`Conversation::continued`]. The history and the new user message are
//! carried, a tool call that never got its result and the wait it was parked on are dropped, the
//! per-run counters start again, and the history is bounded by [`MAX_CARRIED_BYTES`].
//!
//! # Observing a run
//!
//! `Runtime::view(run).state` deserializes into [`Conversation`]: the full
//! history, counters, `continued_from` for a run that carries on another, and `pending_wait` while
//! the run waits for input or for a child run.
//! Live progress arrives as `RunEvent`s; see [`LlmAgent`] for their shapes.

#![warn(missing_docs)]

#[cfg(feature = "schema")]
#[doc(hidden)]
#[path = "private.rs"]
pub mod __private;
mod agent;
mod conversation;
mod fn_tool;
mod history;
mod kept;
pub mod paging;
mod plan;
#[cfg(feature = "schema")]
mod schema;
mod sessions;
mod source;
mod state;
mod step_io;
mod text_stream;
mod tool;
mod toolset;
mod typed;

pub use adam_runtime::{Artifact, StepEvent, StepIcon, StepKind, StepOutput, StepState};
pub use agent::{
    BuildError, CLOSING_TURN, DEFAULT_WAIT_POLL, Limits, LlmAgent, LlmAgentBuilder, LlmStarter,
};
pub use conversation::{
    ACCESS_TIER_CONTEXT_KEY, ArtifactRef, CALLER_CONTEXT_KEY, Conversation, LimitHit, LimitKind,
    MAX_CARRIED_BYTES, MAX_CONTEXT_BYTES, MAX_PROTECTED_VALUE_BYTES, MAX_READ_IDS, MESSAGE_KIND,
    OMITTED_MARKER_PREFIX, PROTECTED_CONTEXT_KEYS, PendingQuestion, PendingRemote, PendingRun,
    PendingSession, PendingWait, Repeats, STOPPED_BY_THE_PERSON, user_message,
};
pub use fn_tool::FnTool;
#[cfg(feature = "schema")]
pub use fn_tool::{FnToolBuilder, TypedFnToolBuilder};
pub use history::{SHORTENING_FLOOR, TRUNCATION_MARKER_PREFIX};
pub use kept::{
    KeptError, KeptInfo, KeptMap, KeptOutput, KeptOutputs, KeptStore, MAX_KEPT_BYTES,
    MAX_RUN_KEPT_BYTES, MemoryKept, READ_OUTPUT, ReadOutput, SCRUB_OVERLAP, call_key,
    canonical_pointer_id, count_lines, find_pointer, is_output_id, keep_long, keep_long_cut,
    kept_header, kept_outputs_conformance, kept_view, output_id, output_id_of, page_of,
    pointer_line, prepare, read_pointer, scrub_open_end, scrub_open_start, scrubbed_for_keeping,
    unkept_view,
};
pub use plan::{MAX_PLAN_ITEMS, Plan, PlanItem, PlanStatus};
#[cfg(feature = "schema")]
pub use schema::{ToolSpecExt, spec_for};
pub use sessions::{
    MAX_DESCRIPTION_CHARS, MAX_OPEN_SESSIONS, MAX_SESSION_ROUNDS, MAX_TEXT_CHARS,
    MAX_WORKING_ROUNDS, SESSION_TOOL_TIME_LIMIT, SESSION_TOOLS, SUBAGENT_CLOSE, SUBAGENT_LIST,
    SUBAGENT_READ, SUBAGENT_SEND, SUBAGENT_START, SessionAgent, SessionTools,
};
pub use source::{
    DynToolSource, Listing, MAX_SOURCE_TOOLS, SourceCtx, ToolNote, ToolSource, Withheld,
};
pub use state::{Extensions, State, StateKey};
pub use step_io::StepIo;
pub use tool::{
    DEFAULT_TOOL_TIME_LIMIT, DynTool, RemotePoll, StepStyle, TOOL_STOP_GRACE, Tool, ToolCtx,
    ToolError, ToolOutput,
};
pub use toolset::ToolSet;
pub use typed::{IntoToolOutput, IntoToolResult, Json, parse_args};
