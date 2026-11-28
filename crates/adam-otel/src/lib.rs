//! The **OpenTelemetry layer** of adam-rs: OTLP/HTTP spans, **off unless an endpoint is set**
//! ([ADR 0075]).
//!
//! `OTEL_EXPORTER_OTLP_ENDPOINT` unset means no exporter and no thread: [`Otel::start`] answers
//! `None` before it builds anything. Set, it builds one OTLP/HTTP exporter over the workspace's
//! `reqwest` (protobuf, no gRPC/tonic) behind a bounded batch queue driven by a task on the
//! process's Tokio runtime, and [`Otel::layer`] gives the layer a host composes beside
//! `tracing_subscriber::fmt()`.
//!
//! The **metrics** half rides the same switch: [`Otel::meter`] gives the meter a run's instruments
//! are created from, exported by a periodic reader on the same runtime, to `{endpoint}/v1/metrics`.
//! Recording a measurement never blocks a run (the SDK aggregates in the process), a failed export
//! is the SDK's internal log line, and a label is a type from [`labels`], so a value outside the set
//! an instrument declares cannot be built.
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

mod install;

pub mod labels;

pub use install::init;

use std::fmt;
use std::time::Duration;

use adam_error::{BoxError, Classify, ErrorClass};
use opentelemetry::metrics::{Meter, MeterProvider as _};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig as _;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::metrics::exporter::PushMetricExporter;
use opentelemetry_sdk::metrics::periodic_reader_with_async_runtime::PeriodicReader;
use opentelemetry_sdk::trace::span_processor_with_async_runtime::BatchSpanProcessor;
use opentelemetry_sdk::trace::{BatchConfigBuilder, SdkTracer, SdkTracerProvider, SpanExporter};
use opentelemetry_sdk::{Resource, runtime};
use tracing::{Metadata, Subscriber};
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::Layer as _;
use tracing_subscriber::filter::Filtered;
use tracing_subscriber::layer::{Context, Filter};
use tracing_subscriber::registry::LookupSpan;
use url::Url;

/// The variable that turns traces on: an OTLP/HTTP endpoint such as `http://collector:4318`.
///
/// Unset or blank: no exporter, no thread.
pub const ENDPOINT_VAR: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";

/// The variable that names the service in every span's resource.
pub const SERVICE_NAME_VAR: &str = "OTEL_SERVICE_NAME";

/// The service name when [`SERVICE_NAME_VAR`] is unset or blank.
pub const DEFAULT_SERVICE_NAME: &str = "adam";

/// The most spans the batch queue holds. A full queue drops, it never blocks the run.
pub const MAX_QUEUE_SIZE: usize = 2048;

/// The most spans of one export.
pub const MAX_EXPORT_BATCH_SIZE: usize = 512;

/// How often a partial batch is exported.
pub const EXPORT_INTERVAL: Duration = Duration::from_secs(5);

/// How long one export may take before the batch processor gives up on it.
pub const EXPORT_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the periodic metrics reader exports what it aggregated.
pub const METRICS_INTERVAL: Duration = Duration::from_secs(15);

/// How long [`Otel::shutdown`] waits for the queue to drain.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// The instrumentation scope name of adam's tracer.
pub const SCOPE_NAME: &str = "adam";

/// The tracing settings read from the environment.
///
/// Only [`ENDPOINT_VAR`] and [`SERVICE_NAME_VAR`] are read; any other `OTEL_*` name is ignored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    endpoint: Option<Url>,
    service_name: String,
}

impl Settings {
    /// Reads [`ENDPOINT_VAR`] and [`SERVICE_NAME_VAR`] from the process environment.
    ///
    /// A variable that is not UTF-8 is [`SettingsError::NotUnicode`]; a variable that is unset or
    /// blank leaves its default, so a build tool that exports an empty endpoint never stops an
    /// agent.
    pub fn from_env() -> Result<Self, SettingsError> {
        let mut pairs = Vec::with_capacity(2);
        for name in [ENDPOINT_VAR, SERVICE_NAME_VAR] {
            match std::env::var(name) {
                Ok(value) => pairs.push((name, value)),
                Err(std::env::VarError::NotPresent) => {}
                Err(std::env::VarError::NotUnicode(_)) => {
                    return Err(SettingsError::NotUnicode { name });
                }
            }
        }
        Self::from_pairs(pairs)
    }

    /// Builds settings from name/value pairs, ignoring every name this crate does not read.
    pub fn from_pairs<I, K, V>(pairs: I) -> Result<Self, SettingsError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let mut settings = Self::disabled();
        for (name, value) in pairs {
            let name = name.as_ref();
            let value = value.as_ref();
            if value.trim().is_empty() {
                continue;
            }
            match name {
                ENDPOINT_VAR => settings.endpoint = Some(parse_endpoint(ENDPOINT_VAR, value)?),
                SERVICE_NAME_VAR => settings.service_name = value.trim().to_owned(),
                _ => {}
            }
        }
        Ok(settings)
    }

    /// Settings with no endpoint: [`Otel::start`] starts nothing.
    pub fn disabled() -> Self {
        Self {
            endpoint: None,
            service_name: DEFAULT_SERVICE_NAME.to_owned(),
        }
    }

    /// Settings for one endpoint, with [`DEFAULT_SERVICE_NAME`].
    pub fn enabled(endpoint: impl AsRef<str>) -> Result<Self, SettingsError> {
        Ok(Self {
            endpoint: Some(parse_endpoint(ENDPOINT_VAR, endpoint.as_ref())?),
            service_name: DEFAULT_SERVICE_NAME.to_owned(),
        })
    }

    /// The same settings under another service name; a blank name keeps the default.
    pub fn with_service_name(mut self, name: impl AsRef<str>) -> Self {
        let name = name.as_ref().trim();
        if !name.is_empty() {
            self.service_name = name.to_owned();
        }
        self
    }

    /// The OTLP endpoint, or `None` when tracing is off.
    ///
    /// This is the base URL; [`Otel::start`] posts traces to `{endpoint}/v1/traces`.
    pub fn endpoint(&self) -> Option<&str> {
        self.endpoint.as_ref().map(Url::as_str)
    }

    /// The service name every span's resource carries.
    pub fn service_name(&self) -> &str {
        &self.service_name
    }

    /// Whether [`Otel::start`] builds an exporter.
    pub fn is_enabled(&self) -> bool {
        self.endpoint.is_some()
    }
}

fn parse_endpoint(name: &'static str, value: &str) -> Result<Url, SettingsError> {
    let url = Url::parse(value).map_err(|source| SettingsError::Endpoint { name, source })?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(SettingsError::EndpointShape { name });
    }
    Ok(url)
}

/// Why the tracing settings were refused. The class is [`ErrorClass::Invalid`]: the same value
/// never succeeds.
///
/// The message names the variable and the reason, never its value: an endpoint may carry a
/// credential in its userinfo, and a secret is never in a log.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SettingsError {
    /// The endpoint does not parse as a URL.
    #[error("{name} is not a URL: {source}")]
    Endpoint {
        /// The variable's name.
        name: &'static str,
        /// Why the value does not parse; it carries a position, not the value.
        #[source]
        source: url::ParseError,
    },
    /// The endpoint parses but is not one this exporter speaks.
    #[error(
        "{name} must be an absolute http or https URL with no user, password, query or fragment"
    )]
    EndpointShape {
        /// The variable's name.
        name: &'static str,
    },
    /// A variable this crate reads is not UTF-8.
    #[error("{name} is not valid UTF-8")]
    NotUnicode {
        /// The variable's name.
        name: &'static str,
    },
}

impl Classify for SettingsError {
    fn class(&self) -> ErrorClass {
        ErrorClass::Invalid
    }
}

/// Why the OTLP exporter could not be built.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StartError {
    /// The exporter or its HTTP client could not be built from the endpoint.
    #[error("the OTLP span exporter could not be built")]
    Exporter(#[source] BoxError),
    /// [`Otel::start`] was called outside a Tokio runtime. The async OTLP client is driven by a
    /// task on the process's runtime, so the host calls it from its async `main`, never from a
    /// blocking one.
    #[error("adam-otel must be started inside a Tokio runtime")]
    NoRuntime,
}

impl Classify for StartError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::Exporter(_) => ErrorClass::Invalid,
            Self::NoRuntime => ErrorClass::Internal,
        }
    }
}

/// The running OTel layer: one provider, one bounded batch queue, one batch task on the process's
/// Tokio runtime.
///
/// Built by [`Otel::start`]. Dropping it shuts the provider down (flushing for
/// [`SHUTDOWN_TIMEOUT`]) once the layer's last tracer is gone. It is never a reason a run fails:
/// a failed export is the SDK's internal log line, and a full queue drops.
pub struct Otel {
    provider: SdkTracerProvider,
    metrics: SdkMeterProvider,
}

impl fmt::Debug for Otel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Otel")
            .field("provider", &self.provider)
            .field("metrics", &self.metrics)
            .finish()
    }
}

impl Otel {
    /// Builds the layer for `settings`, or answers `None` **without starting an exporter or a
    /// thread** when no endpoint is set.
    ///
    /// The exporter is the async `reqwest` client, driven by a task on the process's Tokio
    /// runtime: call this from an async `main`, never from a blocking one. Outside a Tokio runtime
    /// it answers [`StartError::NoRuntime`] instead of panicking.
    pub fn start(settings: &Settings) -> Result<Option<Self>, StartError> {
        Self::start_with(settings, otlp_spans, otlp_metrics)
    }

    /// [`Otel::start`] with the exporters as arguments: the seam the tests use to hand in in-memory
    /// exporters, and the only place an exporter is built.
    fn start_with<E, M>(
        settings: &Settings,
        make_spans: impl FnOnce(&str) -> Result<E, BoxError>,
        make_metrics: impl FnOnce(&str) -> Result<M, BoxError>,
    ) -> Result<Option<Self>, StartError>
    where
        E: SpanExporter + 'static,
        M: PushMetricExporter + 'static,
    {
        let Some(endpoint) = settings.endpoint() else {
            return Ok(None);
        };
        if tokio::runtime::Handle::try_current().is_err() {
            return Err(StartError::NoRuntime);
        }
        let exporter = make_spans(&traces_endpoint(endpoint)).map_err(StartError::Exporter)?;
        let metric_exporter =
            make_metrics(&metrics_endpoint(endpoint)).map_err(StartError::Exporter)?;
        let batch = BatchConfigBuilder::default()
            .with_max_queue_size(MAX_QUEUE_SIZE)
            .with_max_export_batch_size(MAX_EXPORT_BATCH_SIZE)
            .with_scheduled_delay(EXPORT_INTERVAL)
            .build();
        let processor = BatchSpanProcessor::builder(exporter, runtime::Tokio)
            .with_batch_config(batch)
            .build();
        let resource = Resource::builder()
            .with_service_name(settings.service_name().to_owned())
            .build();
        let provider = SdkTracerProvider::builder()
            .with_resource(resource.clone())
            .with_span_processor(processor)
            .build();
        // The metrics half: a periodic reader on the same runtime. Recording a measurement is the
        // SDK's own aggregation in the process, so nothing here can block a run; a failed export is
        // the SDK's `internal-logs` line.
        let reader = PeriodicReader::builder(metric_exporter, runtime::Tokio)
            .with_interval(METRICS_INTERVAL)
            .with_timeout(EXPORT_TIMEOUT)
            .build();
        let metrics = SdkMeterProvider::builder()
            .with_resource(resource)
            .with_reader(reader)
            .build();
        // Installed globally so that a crate which emits a measurement does not have to be handed a
        // meter through every layer (ANR-1449): `opentelemetry::global::meter(..)` finds this one, and
        // answers a no-op meter in a process where nothing was started.
        opentelemetry::global::set_meter_provider(metrics.clone());
        Ok(Some(Self { provider, metrics }))
    }

    /// The layer to compose beside `tracing_subscriber::fmt()`.
    ///
    /// Every extra attribute the bridge would add is off, and [`SpansOnly`] keeps events out, so
    /// a span exports the fields its macro wrote and nothing else.
    pub fn layer<S>(&self) -> OtelLayer<S>
    where
        S: Subscriber + for<'span> LookupSpan<'span>,
    {
        tracing_opentelemetry::layer()
            .with_tracer(self.provider.tracer(SCOPE_NAME))
            .with_target(false)
            .with_location(false)
            .with_tracked_inactivity(false)
            .with_threads(false)
            .with_filter(SpansOnly)
    }

    /// The meter a run's instruments are created from.
    ///
    /// Its labels come from [`labels`]: a value outside the set an instrument declares cannot be
    /// built, so the series a backend receives are the ones the deployment planned for.
    pub fn meter(&self) -> Meter {
        self.metrics.meter(SCOPE_NAME)
    }

    /// Flushes the queues and stops the exporters; each provider is bounded by [`SHUTDOWN_TIMEOUT`], so
    /// the call can wait up to twice it.
    pub fn shutdown(self) {
        let _ = self.metrics.shutdown_with_timeout(SHUTDOWN_TIMEOUT);
        let _ = self.provider.shutdown_with_timeout(SHUTDOWN_TIMEOUT);
    }
}

/// The layer [`Otel::layer`] returns: adam's spans and no events.
pub type OtelLayer<S> = Filtered<OpenTelemetryLayer<S, SdkTracer>, SpansOnly, S>;

/// A layer filter that passes spans and nothing else.
///
/// An event is never a span event ([ADR 0075] decision 3: no span events), and the process's
/// events stay with `tracing_subscriber::fmt()`.
///
/// [ADR 0075]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0075-metrics-and-traces-are-off-by-default-and-say-nothing-about-a-person.md
#[derive(Clone, Copy, Debug, Default)]
pub struct SpansOnly;

impl<S: Subscriber> Filter<S> for SpansOnly {
    fn enabled(&self, metadata: &Metadata<'_>, _: &Context<'_, S>) -> bool {
        metadata.is_span()
    }
}

/// The endpoint OTLP/HTTP traces are posted to: the configured base URL plus `/v1/traces`.
///
/// `OTEL_EXPORTER_OTLP_ENDPOINT` is the base of the OTLP/HTTP protocol, not a full signal URL
/// (`opentelemetry-otlp` appends the path only when it reads the variable itself, not when an
/// endpoint is given to the builder). A base that already names `/v1/traces` is left alone.
fn otlp_spans(endpoint: &str) -> Result<opentelemetry_otlp::SpanExporter, BoxError> {
    opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
        .with_endpoint(endpoint)
        .with_timeout(EXPORT_TIMEOUT)
        .build()
        .map_err(|error| Box::new(error) as BoxError)
}

fn otlp_metrics(endpoint: &str) -> Result<opentelemetry_otlp::MetricExporter, BoxError> {
    opentelemetry_otlp::MetricExporter::builder()
        .with_http()
        .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
        .with_endpoint(endpoint)
        .with_timeout(EXPORT_TIMEOUT)
        .build()
        .map_err(|error| Box::new(error) as BoxError)
}

fn traces_endpoint(base: &str) -> String {
    signal_endpoint(base, "/v1/traces")
}

/// The endpoint OTLP/HTTP metrics are posted to: the configured base URL plus `/v1/metrics`.
fn metrics_endpoint(base: &str) -> String {
    signal_endpoint(base, "/v1/metrics")
}

fn signal_endpoint(base: &str, signal: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.ends_with(signal) {
        base.to_owned()
    } else {
        format!("{base}{signal}")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use opentelemetry_sdk::metrics::InMemoryMetricExporter;
    use opentelemetry_sdk::trace::InMemorySpanExporter;
    use tracing_subscriber::Registry;
    use tracing_subscriber::layer::SubscriberExt as _;

    use super::*;

    #[test]
    fn the_two_variables_are_read_and_the_rest_ignored() {
        let settings = Settings::from_pairs([
            (ENDPOINT_VAR, "http://collector:4318"),
            (SERVICE_NAME_VAR, "  adam-test  "),
            ("OTEL_SOMETHING_ELSE", "ignored"),
        ])
        .expect("settings");
        assert_eq!(settings.endpoint(), Some("http://collector:4318/"));
        assert_eq!(settings.service_name(), "adam-test");
        assert!(settings.is_enabled());
    }

    #[test]
    fn a_blank_setting_is_unset() {
        let settings =
            Settings::from_pairs([(ENDPOINT_VAR, "  "), (SERVICE_NAME_VAR, "")]).expect("settings");
        assert!(!settings.is_enabled());
        assert_eq!(settings.service_name(), DEFAULT_SERVICE_NAME);
    }

    #[test]
    fn a_refused_endpoint_is_invalid_and_echoes_no_credential() {
        for bad in [
            "collector:4318",
            "ftp://collector:4318",
            "http://user:secret@collector:4318",
            "http://collector:4318/#fragment",
            "http://collector:4318/?query=1",
        ] {
            let error = Settings::from_pairs([(ENDPOINT_VAR, bad)]).expect_err(bad);
            assert_eq!(error.class(), ErrorClass::Invalid, "{bad}");
            let shown = format!("{error} {error:?}");
            assert!(
                !shown.contains("secret"),
                "{bad} echoed a credential: {shown}"
            );
        }
    }

    #[test]
    fn the_signal_path_is_appended_once() {
        assert_eq!(
            traces_endpoint("http://collector:4318/"),
            "http://collector:4318/v1/traces"
        );
        assert_eq!(
            traces_endpoint("http://collector:4318/otlp"),
            "http://collector:4318/otlp/v1/traces"
        );
        assert_eq!(
            traces_endpoint("http://collector:4318/v1/traces"),
            "http://collector:4318/v1/traces"
        );
        assert_eq!(
            metrics_endpoint("http://collector:4318/"),
            "http://collector:4318/v1/metrics"
        );
        assert_eq!(
            metrics_endpoint("http://collector:4318/otlp"),
            "http://collector:4318/otlp/v1/metrics"
        );
        assert_eq!(
            metrics_endpoint("http://collector:4318/v1/metrics"),
            "http://collector:4318/v1/metrics"
        );
    }

    #[test]
    fn no_endpoint_starts_no_exporter_and_no_thread() {
        let started = Arc::new(AtomicBool::new(false));
        let span_marker = Arc::clone(&started);
        let metric_marker = Arc::clone(&started);
        let otel = Otel::start_with(
            &Settings::disabled(),
            move |_| {
                span_marker.store(true, Ordering::SeqCst);
                Ok::<_, BoxError>(InMemorySpanExporter::default())
            },
            move |_| {
                metric_marker.store(true, Ordering::SeqCst);
                Ok::<_, BoxError>(InMemoryMetricExporter::default())
            },
        )
        .expect("start");
        assert!(otel.is_none(), "a provider was built without an endpoint");
        assert!(
            !started.load(Ordering::SeqCst),
            "an exporter was started without an endpoint"
        );
    }

    #[test]
    fn start_outside_a_tokio_runtime_is_refused_without_panicking() {
        let error =
            Otel::start(&Settings::enabled("http://collector.invalid:4318").expect("settings"))
                .expect_err("no runtime");
        assert!(matches!(error, StartError::NoRuntime), "{error:?}");
        assert_eq!(error.class(), ErrorClass::Internal);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_run_span_reaches_the_exporter_with_only_its_state() {
        let exporter = InMemorySpanExporter::default();
        let sink = exporter.clone();
        let otel = Otel::start_with(
            &Settings::enabled("http://collector.invalid:4318").expect("settings"),
            move |_| Ok::<_, BoxError>(sink),
            |_| Ok::<_, BoxError>(InMemoryMetricExporter::default()),
        )
        .expect("start")
        .expect("an endpoint is set");

        let subscriber = Registry::default().with(otel.layer());
        tracing::subscriber::with_default(subscriber, || {
            tracing::info_span!("adam.run", state = "runnable").in_scope(|| {});
        });
        otel.provider.force_flush().expect("flush");

        let spans = exporter.get_finished_spans().expect("spans");
        assert_eq!(spans.len(), 1, "{spans:?}");
        let span = &spans[0];
        assert_eq!(span.name.as_ref(), "adam.run");
        let attributes: Vec<(&str, String)> = span
            .attributes
            .iter()
            .map(|kv| (kv.key.as_str(), kv.value.as_str().into_owned()))
            .collect();
        assert_eq!(
            attributes,
            vec![("state", "runnable".to_owned())],
            "the closed set of ANR-1345 is `state` and nothing else: {attributes:?}"
        );
        otel.shutdown();
    }

    /// The metrics half: a measurement reaches the exporter with exactly the label the closed set
    /// allows, and nothing else.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_measurement_reaches_the_metrics_exporter_with_only_its_label() {
        let exporter = InMemoryMetricExporter::default();
        let sink = exporter.clone();
        let otel = Otel::start_with(
            &Settings::enabled("http://collector.invalid:4318").expect("settings"),
            |_| Ok::<_, BoxError>(InMemorySpanExporter::default()),
            move |_| Ok::<_, BoxError>(sink),
        )
        .expect("start")
        .expect("an endpoint is set");

        let meter = otel.meter();
        let state = crate::labels::RunState::Parked;
        let counter = meter.u64_counter("adam.runs").build();
        counter.add(1, &[opentelemetry::KeyValue::new("state", state.as_str())]);
        otel.metrics.force_flush().expect("flush");

        let batches = exporter.get_finished_metrics().expect("metrics");
        let resource_metrics = batches.first().expect("one resource");
        let scope = resource_metrics.scope_metrics().next().expect("one scope");
        let metric = scope.metrics().next().expect("one metric");
        assert_eq!(metric.name(), "adam.runs");
        let data = match metric.data() {
            opentelemetry_sdk::metrics::data::AggregatedMetrics::U64(
                opentelemetry_sdk::metrics::data::MetricData::Sum(sum),
            ) => sum,
            other => panic!("not a u64 sum: {other:?}"),
        };
        let point = data.data_points().next().expect("one data point");
        let attributes: Vec<(&str, String)> = point
            .attributes()
            .map(|kv| (kv.key.as_str(), kv.value.as_str().into_owned()))
            .collect();
        assert_eq!(
            attributes,
            vec![("state", "parked".to_owned())],
            "the label set is `state` and nothing else: {attributes:?}"
        );
        otel.shutdown();
    }
}
