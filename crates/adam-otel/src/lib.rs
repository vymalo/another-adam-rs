//! The **OpenTelemetry layer** of adam-rs: OTLP/HTTP spans, **off unless an endpoint is set**
//! ([ADR 0075]).
//!
//! `OTEL_EXPORTER_OTLP_ENDPOINT` unset means no exporter and no thread: [`Otel::start`] answers
//! `None` before it builds anything. Set, it builds one OTLP/HTTP exporter over the workspace's
//! `reqwest` (protobuf, no gRPC/tonic) behind a bounded batch queue driven by a task on the
//! process's Tokio runtime, and [`Otel::layer`] gives the layer a host composes beside
//! `tracing_subscriber::fmt()`.
//!
//! The **logs** half rides it too: the process's `tracing` events become OTLP log records through an
//! allowlist ([`logs`]), so a record carries a class and a message. The allowlist governs **fields**;
//! the message is the format string the macro wrote, so a call site that formats a value into it
//! exports that value (as the JSON layer already does).
//!
//! The **metrics** half rides the same switch: [`Otel::meter`] gives the meter a run's instruments
//! are created from, exported by a periodic reader on the same runtime, to `{endpoint}/v1/metrics`.
//! Recording a measurement never blocks a run (the SDK aggregates in the process), a failed export
//! is the SDK's internal log line, and a label is a type from [`labels`], so a value outside the set
//! an instrument declares cannot be built. [`catalogue`] is the list of instruments the workspace
//! actually records, and `crates/adam-otel/tests/dashboards.rs` holds every metric a panel of
//! `deploy/dashboards/` queries to it.
//!
//! * **The closed set holds.** The layer's own extra attributes (`code.filepath`, `code.lineno`,
//!   `busy_ns`, `idle_ns`, `thread.name`, `thread.id`) are switched off, so a span exports exactly
//!   the fields its `tracing` macro wrote — the closed set of ANR-1345 and ANR-1346. Events are
//!   filtered out, so a log line never becomes a span event.
//! * **A failed export never fails a run.** The queue is bounded ([`MAX_QUEUE_SIZE`]); a full
//!   queue drops and the run goes on, and the SDK reports an export failure through `tracing`
//!   (its `internal-logs`), never to the caller. The batch task lives on the Tokio runtime, so
//!   [`Otel::start`] is called from an async `main` and refuses ([`StartError::NoRuntime`]) rather
//!   than panicking outside one.
//! * **No secret is echoed.** [`SettingsError`] names the variable and the reason, never the value
//!   (an endpoint may carry a credential in its userinfo).
//!
//! The binaries and the configuration that reaches them are later cards (ANR-1385, ANR-1386);
//! this crate is the foundation.
//!
//! [ADR 0075]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0075-metrics-and-traces-are-off-by-default-and-say-nothing-about-a-person.md

#![forbid(unsafe_code)]
#![warn(missing_docs)]

#[cfg(feature = "sdk")]
mod install;
#[cfg(feature = "sdk")]
mod sdk;

pub mod catalogue;
pub mod labels;
#[cfg(feature = "sdk")]
pub mod logs;

#[cfg(feature = "sdk")]
pub use install::init;
#[cfg(feature = "sdk")]
pub use sdk::*;
