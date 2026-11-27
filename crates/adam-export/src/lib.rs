//! Export adam-rs runs as training records ([ADR 0085]): one JSON Lines record per run, in the
//! chat shape most trainers read, with the facets a stack filter needs, the outcome the store
//! holds, every string scrubbed and every id a pseudonym.
//!
//! * [`record_of`] turns a run's bookkeeping ([`RunMeta`]) and its `Conversation` into a
//!   [`Record`]. Pure.
//! * [`samples_of`] turns it into one [`Record`] per assistant turn since the last compaction,
//!   each the request the model was sent at that call and its answer ([`Samples::Turn`]). Pure.
//! * [`Exporter`] is the pipeline: it reads the runs of one or several [`RunSource`]s (the port of
//!   where runs come from) and puts the records a [`Filter`] accepts into a [`RecordSink`] (the
//!   port of where they go). [`RuntimeSource`] is the source over any `Store` through a
//!   [`Runtime`](adam_runtime::Runtime); [`JsonLines`] is the sink over any [`std::io::Write`].
//!   `adam-export-testkit` holds the conformance suites of both ports.
//! * [`export_runs`] is the convenience path: one runtime's agents as JSON Lines to a writer.
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
mod runtime_source;
mod sample;
mod scrub;
mod sink;
mod source;

pub use convert::{
    ExportOptions, IMAGE_PLACEHOLDER, RequestShape, RunMeta, record_of, record_with,
};
pub use export::{ExportError, Exporter, Summary, export_runs, write_record};
pub use filter::Filter;
pub use pseudonym::{NoRandomness, Pseudonyms};
pub use record::{
    ChatMessage, ChatToolCall, Facets, FunctionCall, FunctionDef, Outcome, OutcomeArtifact, Record,
    SCHEMA, Sample, ToolDef, ToolsSource, TrainOn,
};
pub use runtime_source::RuntimeSource;
pub use sample::{Samples, TurnRequest, estimated_tokens, samples_of, turns_lost_to_compaction};
pub use scrub::{NoSecrets, Scrub, scrub_known_shapes};
pub use sink::{JsonLines, RecordSink, SinkError};
pub use source::{Cursor, RunSource, Skipped, SourceError, SourcePage, SourceQuery, SourceRun};
