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

/// The counter of a model call's tokens, by direction and model.
pub const LLM_TOKENS: &str = adam_otel::catalogue::LLM_TOKENS;

/// The histogram of one model call's latency, by model.
pub const LLM_DURATION: &str = adam_otel::catalogue::LLM_DURATION;

/// Records one model call: its tokens by direction, and its latency, under the alias it was made to.
///
/// The alias is the one the agent was configured with (the deployment's `MODEL_ALIASES_FILE`, or its
/// `MODEL`), so the label set is closed by construction; a name the alphabet refuses is not measured
/// at all, and neither are its tokens — a series the deployment did not declare is never invented.
pub fn record_model_call(
    meter: &Meter,
    alias: &str,
    input_tokens: u64,
    output_tokens: u64,
    elapsed: Duration,
) {
    let Ok(model) = Name::new(alias) else {
        return;
    };
    let tokens = meter.u64_counter(LLM_TOKENS).build();
    for (direction, count) in [
        (adam_otel::labels::Direction::Input, input_tokens),
        (adam_otel::labels::Direction::Output, output_tokens),
    ] {
        tokens.add(
            count,
            &[
                KeyValue::new("direction", direction.as_str()),
                KeyValue::new("model", model.as_str().to_owned()),
            ],
        );
    }
    meter
        .f64_histogram(LLM_DURATION)
        .with_unit("s")
        .build()
        .record(
            elapsed.as_secs_f64(),
            &[KeyValue::new("model", model.as_str().to_owned())],
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

    /// A model call is measured as two token counters (input, output) and one latency, each labelled
    /// by the alias and nothing else — and an alias the alphabet refuses is not measured at all.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_model_call_is_measured_by_direction_and_alias_and_nothing_else() {
        let exporter = InMemoryMetricExporter::default();
        let reader = PeriodicReader::builder(exporter.clone(), runtime::Tokio).build();
        let provider = SdkMeterProvider::builder().with_reader(reader).build();
        let meter = provider.meter(SCOPE);

        record_model_call(
            &meter,
            "claude-sonnet-5",
            120,
            40,
            Duration::from_millis(900),
        );
        record_model_call(&meter, "not an alias", 10, 10, Duration::from_millis(1));
        provider.force_flush().expect("flush");

        let batches = exporter.get_finished_metrics().expect("metrics");
        let resource = batches.first().expect("one resource");
        let scope = resource.scope_metrics().next().expect("one scope");
        let mut names: Vec<&str> = scope.metrics().map(|m| m.name()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec![LLM_DURATION, LLM_TOKENS],
            "the two instruments, once each"
        );

        for metric in scope.metrics() {
            match metric.data() {
                data::AggregatedMetrics::U64(data::MetricData::Sum(sum)) => {
                    let points: Vec<_> = sum.data_points().collect();
                    assert_eq!(points.len(), 2, "one point per direction: {points:?}");
                    // The attributes are sorted by key, not in the order they were written, so
                    // each point is compared as a map: `direction` and `model`, nothing else.
                    let mut seen: Vec<(Vec<(String, String)>, u64)> = points
                        .iter()
                        .map(|point| {
                            let mut attributes: Vec<(String, String)> = point
                                .attributes()
                                .map(|kv| (kv.key.to_string(), kv.value.as_str().into_owned()))
                                .collect();
                            attributes.sort();
                            (attributes, point.value())
                        })
                        .collect();
                    seen.sort();
                    assert_eq!(
                        seen,
                        vec![
                            (
                                vec![
                                    ("direction".to_owned(), "input".to_owned()),
                                    ("model".to_owned(), "claude-sonnet-5".to_owned()),
                                ],
                                120
                            ),
                            (
                                vec![
                                    ("direction".to_owned(), "output".to_owned()),
                                    ("model".to_owned(), "claude-sonnet-5".to_owned()),
                                ],
                                40
                            ),
                        ]
                    );
                }
                data::AggregatedMetrics::F64(data::MetricData::Histogram(histogram)) => {
                    let points: Vec<_> = histogram.data_points().collect();
                    assert_eq!(
                        points.len(),
                        1,
                        "the refused alias was measured: {points:?}"
                    );
                    let attributes: Vec<(String, String)> = points[0]
                        .attributes()
                        .map(|kv| (kv.key.to_string(), kv.value.as_str().into_owned()))
                        .collect();
                    assert_eq!(
                        attributes,
                        vec![("model".to_owned(), "claude-sonnet-5".to_owned())]
                    );
                }
                other => panic!("unexpected instrument data: {other:?}"),
            }
        }
    }

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
