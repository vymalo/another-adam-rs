//! The `#[tool]` attribute macro of adam-rs.
//!
//! Use it through the `adam` facade (`adam::prelude::*`), which also carries
//! the paths the generated code refers to. This crate is only the macro: the
//! expansion lives in a pure function over token streams (`expand`), so that
//! it is unit-tested like any other code.
//!
//! ```ignore
//! use adam::prelude::*;
//!
//! /// Ask the person who gave you the task a question and wait for the answer.
//! #[tool(asks_user)]
//! pub async fn ask_user(
//!     /// What you need to know
//!     question: String,
//! ) -> Result<ToolOutput, ToolError> {
//!     Err(ToolError::needs_input(question))
//! }
//!
//! let tools = tools![AskUser];
//! ```
//!
//! See the `adam` crate for the full contract, and `docs/authoring.md`.

use proc_macro::TokenStream;

mod expand;

/// Turns an `async fn` into a tool: keeps the function, and generates a unit
/// struct (the function's name in `UpperCamelCase`) that implements
/// `adam::Tool`.
///
/// * The doc comment of the function is the tool's description, and the doc
///   comment of each parameter is that argument's description. Wrapped lines
///   are joined, blank lines separate paragraphs.
/// * A `&ToolCtx` parameter (at most one) receives the call's context, and a
///   `State<T>` parameter receives the shared `T` the agent was given (and is
///   reported by `Tool::required_state`). Every other parameter is an
///   argument the model fills in.
/// * `#[args] a: MyArgs` takes an existing `Deserialize + JsonSchema` struct
///   as the whole argument object.
/// * The return type is `Result<T, E>` or a bare `T`, with `T: IntoToolOutput`
///   and `E: Into<ToolError>`.
///
/// Options: `name = "..."` (the tool's name, default the function's),
/// `type = Ident` (the generated struct, default the function's name in
/// `UpperCamelCase`), `strict` (unknown argument fields are an error),
/// `classify` (the error is `adam_error::Classify`: retryable errors become
/// `ToolError::Transient`, others `Permanent`), `asks_user` (the tool can end a call with
/// `ToolError::NeedsInput`: `Tool::asks_user` says `true`, and a subagent may not have it) and
/// `step = "subagent"`, `label = "OpenCode"` and `icon = "agent"` (how a call is drawn as a step,
/// `Tool::step_style`: the kind, a label instead of the tool's name, and an icon; each from the closed
/// vocabulary of the `steps/v1` extension, and any other word is an error that lists them) and
/// `crate = path` (where
/// `Tool` and `__private` are, default `::adam`; `::adam_llm_agent` for a
/// crate without the facade).
#[proc_macro_attribute]
pub fn tool(attr: TokenStream, item: TokenStream) -> TokenStream {
    expand::expand(attr.into(), item.into()).into()
}
