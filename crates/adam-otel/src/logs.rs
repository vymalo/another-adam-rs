//! The logs half: the process's `tracing` events as OTLP log records, through an **allowlist**
//! ([ADR 0075]).
//!
//! The spans export only the fields their macros wrote, from a closed set; a log record follows the
//! same discipline, which is what "through the same allowlist and redactor as the spans" means here.
//! A record carries its target, its severity, the message the macro wrote, and **only** the fields
//! named in [`ALLOWED`] — a class, a state, a count. Everything else the event carries (a prompt, a
//! tool's arguments, a path, a token, an id) is dropped before the record is built, so it cannot
//! reach an exporter even by accident.
//!
//! The coder's redactor holds per-run secrets the process-level layer cannot see; the allowlist is
//! what makes that unnecessary: a value that is not a named class never becomes a field.
//!
//! [ADR 0075]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0075-metrics-and-traces-are-off-by-default-and-say-nothing-about-a-person.md

use std::fmt;

use opentelemetry::logs::{AnyValue, LogRecord as _, Logger as _, LoggerProvider as _, Severity};
use opentelemetry_sdk::logs::SdkLogger;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

/// The fields a log record may carry, and nothing else.
///
/// Every entry is a class or a number the code already computes: an error's class, a run's state, a
/// count, the name of a declared tool or model alias, a worker id, an attempt number. A field
/// outside this list is dropped by [`Fields`] before a record exists.
pub const ALLOWED: &[&str] = &[
    "attempt",
    "count",
    "error",
    "error.type",
    "model",
    "run.state",
    "state",
    "tool",
    "worker",
];

/// The layer: `tracing` events to OTLP log records, allowlisted.
pub struct Logs {
    logger: SdkLogger,
}

impl fmt::Debug for Logs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Logs").finish_non_exhaustive()
    }
}

impl Logs {
    /// The layer over a logger provider.
    pub fn new(provider: &opentelemetry_sdk::logs::SdkLoggerProvider) -> Self {
        Self {
            logger: provider.logger(crate::SCOPE_NAME),
        }
    }
}

impl<S: Subscriber> Layer<S> for Logs {
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        let metadata = event.metadata();
        let mut fields = Fields::default();
        event.record(&mut fields);
        let mut record = self.logger.create_log_record();
        record.set_target(metadata.target());
        record.set_severity_number(severity(metadata.level()));
        record.set_severity_text(severity_text(metadata.level()));
        if let Some(message) = fields.message {
            record.set_body(AnyValue::String(message.into()));
        }
        for (name, value) in fields.allowed {
            record.add_attribute(name.to_string(), AnyValue::String(value.into()));
        }
        self.logger.emit(record);
    }
}

/// `tracing`'s level as the OTLP severity number.
fn severity(level: &Level) -> Severity {
    match *level {
        Level::TRACE => Severity::Trace,
        Level::DEBUG => Severity::Debug,
        Level::INFO => Severity::Info,
        Level::WARN => Severity::Warn,
        Level::ERROR => Severity::Error,
    }
}

/// `tracing`'s level as the OTLP severity text.
fn severity_text(level: &Level) -> &'static str {
    match *level {
        Level::TRACE => "TRACE",
        Level::DEBUG => "DEBUG",
        Level::INFO => "INFO",
        Level::WARN => "WARN",
        Level::ERROR => "ERROR",
    }
}

/// What an event carries, split into the message and the allowlisted fields.
#[derive(Default)]
struct Fields {
    message: Option<String>,
    allowed: Vec<(&'static str, String)>,
}

impl Fields {
    fn keep(&mut self, field: &Field, value: String) {
        let name = field.name();
        if name == "message" {
            self.message = Some(value);
            return;
        }
        if let Some(known) = ALLOWED.iter().find(|allowed| **allowed == name) {
            self.allowed.push((known, value));
        }
        // Every other field is dropped here: it never becomes a record attribute.
    }
}

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.keep(field, format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.keep(field, value.to_owned());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.keep(field, value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.keep(field, value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.keep(field, value.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_sdk::logs::{InMemoryLogExporter, SdkLoggerProvider};
    use tracing_subscriber::layer::SubscriberExt as _;

    /// The record reaches the exporter with its message and its allowlisted fields, and **without**
    /// the ones beside them: a prompt-shaped string, an argument, a token.
    #[test]
    fn a_prompt_is_not_a_field_and_a_class_is() {
        let exporter = InMemoryLogExporter::default();
        let provider = SdkLoggerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let subscriber = tracing_subscriber::registry().with(Logs::new(&provider));
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!(
                error = "Transient",
                prompt = "the person's message",
                args = "{ \"path\": \"/etc/shadow\" }",
                token = "ghp_0123456789",
                attempt = 2,
                "the step failed"
            );
        });
        let records = exporter.get_emitted_logs().expect("records");
        assert_eq!(records.len(), 1, "{records:?}");
        let record = &records[0].record;
        let body = match record.body() {
            Some(AnyValue::String(body)) => body.as_str().to_owned(),
            other => panic!("not a string body: {other:?}"),
        };
        assert_eq!(body, "the step failed");
        let attributes: Vec<(String, String)> = record
            .attributes_iter()
            .map(|(key, value)| {
                let shown = match value {
                    AnyValue::String(value) => value.as_str().to_owned(),
                    other => format!("{other:?}"),
                };
                (key.to_string(), shown)
            })
            .collect();
        let names: Vec<&str> = attributes.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            names,
            vec!["error", "attempt"],
            "a field outside the allowlist reached the exporter: {attributes:?}"
        );
        let text = format!("{attributes:?}");
        for secret in ["the person's message", "/etc/shadow", "ghp_0123456789"] {
            assert!(!text.contains(secret), "{secret} reached the exporter");
        }
    }
}
