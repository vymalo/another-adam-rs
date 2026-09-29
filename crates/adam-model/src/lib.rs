//! The model-client seam of adam-rs.
//!
//! Agents talk to language models only through [`ModelClient`]. The trait and
//! its data types live here; implementations live in their own crates (for
//! example `adam-model-openai`), so swapping the backend means swapping one
//! crate and never touches agent code.
//!
//! # Persisted history
//!
//! Every data type derives `Serialize`/`Deserialize`: a run persists its
//! conversation history ([`Message`]) in its durable state. The serde shapes
//! are deliberately readable and stable:
//!
//! ```json
//! {"role":"user","content":[{"type":"text","text":"What is the weather?"}]}
//! {"role":"assistant","content":[],"tool_calls":[{"id":"call_1","name":"weather","arguments":{"city":"Paris"}}]}
//! {"role":"tool","call_id":"call_1","content":"18C, sunny","is_error":false}
//! ```
//!
//! # Testing agents
//!
//! [`MockModel`] is a scripted [`ModelClient`] that records the requests it
//! receives. It is always compiled (no feature flag) so other crates can use
//! it from their tests with a plain dependency on `adam-model`.
//!
//! ```
//! use adam_model::{MockModel, ModelClient, ModelRequest, Message};
//!
//! # futures::executor::block_on(async {
//! let model = MockModel::new();
//! model.push_text("hello");
//!
//! let mut req = ModelRequest::new("any-alias");
//! req.messages.push(Message::user_text("hi"));
//! let resp = model.complete(req).await.unwrap();
//!
//! assert_eq!(resp.message.text(), "hello");
//! assert_eq!(model.requests().len(), 1);
//! # });
//! ```
//!
//! # Retries
//!
//! Implementations never retry. [`ModelError::is_retryable`] tells the runtime
//! which failures are worth retrying; the runtime owns backoff.

#![warn(missing_docs)]

mod client;
mod error;
mod mock;
mod types;

pub use client::{DynModel, ModelClient};
pub use error::ModelError;
pub use mock::{MockModel, RecordedCall};
pub use types::{
    ContentPart, FinishReason, Message, ModelDelta, ModelRequest, ModelResponse, ToolCall,
    ToolChoice, ToolSpec, Usage,
};
