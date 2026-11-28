//! The agent's metrics (ANR-1454): a tool call's latency, labelled by the tool's name — the set the
//! agent registered, which is the closed set [`adam_otel::labels::Name`] exists for.
//!
//! Off with the rest: the instruments come from the global meter, which answers a no-op meter in a
//! process where `adam_otel::init` found no endpoint (ADR 0075). A name the label alphabet refuses
//! is not measured at all rather than measured under an invented label: a metric says nothing a
//! deployment did not declare.

use std::time::Duration;

use adam_otel::labels::Name;
use opentelemetry::KeyValue;
use opentelemetry::metrics::Meter;

/// The instrumentation scope of this crate's instruments.
pub const SCOPE: &str = "adam-llm-agent";

/// How long one tool call took, in seconds, one label: `tool`.
///
/// The name is the catalogue's ([`adam_otel::catalogue::TOOL_DURATION`]), so the list a dashboard is
/// held to and the instrument recorded here cannot drift apart.
pub const TOOL_DURATION: &str = adam_otel::catalogue::TOOL_DURATION;

/// Records one tool call's latency, under the tool's registered name.
///
/// The caller records only when a tool or a source answered the call, so the label set is the tools
/// the agent declares; a name the alphabet refuses is skipped (no measurement, no invented label).
pub fn record_tool_duration(meter: &Meter, tool: &str, elapsed: Duration) {
    let Ok(tool) = Name::new(tool) else {
        return;
    };
    meter
        .f64_histogram(TOOL_DURATION)
        .with_unit("s")
        .build()
        .record(
            elapsed.as_secs_f64(),
            &[KeyValue::new("tool", tool.as_str().to_owned())],
        );
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry_sdk::metrics::InMemoryMetricExporter;
    use opentelemetry_sdk::metrics::periodic_reader_with_async_runtime::PeriodicReader;
    use opentelemetry_sdk::metrics::{SdkMeterProvider, data};
    use opentelemetry_sdk::runtime;

    /// A call is measured under its tool's name and nothing else, and a name the alphabet refuses is
    /// not measured at all.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tool_call_is_measured_under_its_name_and_nothing_else() {
        let exporter = InMemoryMetricExporter::default();
        let reader = PeriodicReader::builder(exporter.clone(), runtime::Tokio).build();
        let provider = SdkMeterProvider::builder().with_reader(reader).build();
        let meter = provider.meter(SCOPE);

        record_tool_duration(&meter, "run_checks", Duration::from_millis(1500));
        record_tool_duration(&meter, "not a name", Duration::from_millis(10));
        provider.force_flush().expect("flush");

        let batches = exporter.get_finished_metrics().expect("metrics");
        let resource = batches.first().expect("one resource");
        let scope = resource.scope_metrics().next().expect("one scope");
        let metrics: Vec<_> = scope.metrics().collect();
        assert_eq!(metrics.len(), 1, "one instrument: {metrics:?}");
        let metric = metrics[0];
        assert_eq!(metric.name(), TOOL_DURATION);
        let data = match metric.data() {
            data::AggregatedMetrics::F64(data::MetricData::Histogram(histogram)) => histogram,
            other => panic!("not an f64 histogram: {other:?}"),
        };
        let points: Vec<_> = data.data_points().collect();
        assert_eq!(points.len(), 1, "the refused name was measured: {points:?}");
        let attributes: Vec<(String, String)> = points[0]
            .attributes()
            .map(|kv| (kv.key.to_string(), kv.value.as_str().into_owned()))
            .collect();
        assert_eq!(
            attributes,
            vec![("tool".to_owned(), "run_checks".to_owned())]
        );
        assert_eq!(points[0].count(), 1);
    }
}
