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
//! # Reasoning
//!
//! A model in thinking mode writes its reasoning before its answer. It reaches a caller as
//! [`ModelDelta::Reasoning`] pieces while the model writes and whole as [`ModelResponse::reasoning`].
//! It is **not part of the answer**: [`Message::text`] never holds it, and it is not part of the
//! history, so a client does not send it back, with one exception that the client chooses: a
//! provider that **requires** it (DeepSeek's thinking mode with tools, *verified 2026-10-05*) is
//! served by a client set to echo it, which then keeps it in [`Message::Assistant::reasoning`].
//!
//! The other direction is [`ModelRequest::reasoning_effort`] ([ADR 0063](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0063-a-reasoning-effort-is-one-setting.md)):
//! a caller may ask for a level, and the client sends it through the wire of the request's model
//! profile. Absent sends nothing and leaves the model's own default.//!
//! # Streaming a tool call
//!
//! A streaming client says [`ModelDelta::ToolCallComplete`] once per tool call, when its arguments
//! are whole: the first time the accumulated argument text parses as a JSON object, and for every
//! call it did not say it for, immediately before [`ModelDelta::Finished`]. The call is the one the
//! response holds (same id, name and arguments), so a caller may start it before the turn ends
//! ([ADR 0049](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0049-the-tool-calls-of-a-turn-run-at-the-same-time.md),
//! amended). Empty argument text is not complete on its own. A call with no name yet waits for its
//! name. [`ModelDelta`] is `#[non_exhaustive]`
//! since this variant: a `match` over it needs one more arm.
//!
//! # Images
//!
//! A message names an image by reference ([`ContentPart::Image`], [`Message::Tool`]'s `images`):
//! its SHA-256, type, size, dimensions and origin, never its bytes, so no history and no journal
//! holds one. A client that can send them reads them from [`ModelRequest::image_data`], which the
//! agent loop fills for the one call and which is never serialized. Whether a model can see is the
//! deployment's word, [`ModelClient::input_modalities`], text only by default.
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
//! Implementations never retry. [`Classify::is_retryable`] tells the runtime
//! which failures are worth retrying and [`Classify::retry_after`] how long the
//! provider asked to wait; the runtime owns backoff.

#![warn(missing_docs)]

mod client;
mod credentials;
mod error;
mod mock;
mod profiles;
mod types;

pub use adam_error::{Classify, ErrorClass};
pub use client::{DynModel, ModelClient};
pub use credentials::{
    AfterRejection, CredentialHeaders, CredentialRequest, DynCredentials, ModelCredentials,
    Rejection, RenewingCredentials, StaticBearer,
};
pub use error::ModelError;
pub use mock::{MockModel, RecordedCall};
pub use profiles::{
    ModelProfile, ModelProfiles, NotAWire, NotAnEffort, Preset, RESERVED_EXTRA_BODY_KEYS,
    ReasoningEffort, ReasoningWire,
};
pub use types::{
    ContentPart, FinishReason, INVALID_ARGUMENTS_MARKER, ImageData, ImageLimits, ImageOrigin,
    ImageRef, InputModalities, Message, ModelDelta, ModelRequest, ModelResponse, ToolCall,
    ToolChoice, ToolSpec, Usage, human_bytes, type_word,
};
