//! Export adam-rs runs as training records ([ADR 0085]): one JSON Lines record per run, in the
//! chat shape most trainers read, with the facets a stack filter needs, the outcome the store
//! holds, every string scrubbed and every id a pseudonym.
//!
//! * [`record_of`] turns a run's bookkeeping ([`RunMeta`]) and its `Conversation` into a
//!   [`Record`]. Pure.
//! * [`export_runs`] lists an agent's runs from a [`Runtime`](adam_runtime::Runtime), joins each
//!   one and writes the records a [`Filter`] accepts.
//! * [`facets`] derives `lang` and `dep` from a transcript; [`Scrub`] and
//!   [`scrub_known_shapes`] remove secrets; [`Pseudonyms`] hide ids.
//!
//! [ADR 0085]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0085-runs-are-exported-as-training-records.md

mod convert;
mod export;
pub mod facets;
mod filter;
mod pseudonym;
mod record;
mod scrub;

pub use convert::{
    ExportOptions, IMAGE_PLACEHOLDER, RequestShape, RunMeta, record_of, record_with,
};
pub use export::{ExportError, Summary, export_runs, write_record};
pub use filter::Filter;
pub use pseudonym::{NoRandomness, Pseudonyms};
pub use record::{
    ChatMessage, ChatToolCall, Facets, FunctionCall, FunctionDef, Outcome, OutcomeArtifact, Record,
    SCHEMA, ToolDef, ToolsSource,
};
pub use scrub::{NoSecrets, Scrub, scrub_known_shapes};
