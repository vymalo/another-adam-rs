//! The runtime's metrics (ANR-1449): runs by state, with the state a value of the closed set
//! [`adam_otel::labels::RunState`], so no other label can reach the exporter.
//!
//! Off with the rest: the instruments are built from the global meter, which answers a no-op meter
//! in a process where `adam_otel::init` found no endpoint (ADR 0075), so a deployment without one
//! pays nothing and a measurement is never a reason a run fails.

use adam_core::store::RunStatus;
use adam_otel::labels::RunState;
use opentelemetry::KeyValue;
use opentelemetry::metrics::Meter;

/// The instrumentation scope of this crate's instruments.
pub const SCOPE: &str = "adam-runtime";

/// The counter of committed transitions, one per state: `state` is its only label.
///
/// The name is the catalogue's ([`adam_otel::catalogue::RUNS`]), so the dashboards' test and the
/// instrument cannot drift apart.
pub use adam_otel::catalogue::RUNS;

/// [`RunStatus`] as a label. Exhaustive on purpose: a state added to the store without a line here
/// does not compile, which is what keeps the instrument's set closed.
pub fn state_of(status: RunStatus) -> RunState {
    match status {
        RunStatus::Runnable => RunState::Runnable,
        RunStatus::Parked => RunState::Parked,
        RunStatus::Done => RunState::Done,
        RunStatus::Failed => RunState::Failed,
    }
}

/// Records one committed transition, under the state it committed.
///
/// The meter is a parameter so that a test can hand in one from an in-memory exporter; the caller
/// passes the process's ([`opentelemetry::global::meter`]).
pub fn record_run_status(meter: &Meter, status: RunStatus) {
    meter
        .u64_counter(RUNS)
        .build()
        .add(1, &[KeyValue::new("state", state_of(status).as_str())]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry_sdk::metrics::InMemoryMetricExporter;
    use opentelemetry_sdk::metrics::periodic_reader_with_async_runtime::PeriodicReader;
    use opentelemetry_sdk::metrics::{SdkMeterProvider, data};
    use opentelemetry_sdk::runtime;

    /// Every committed state reports itself, and the label set is `state` and nothing else — the
    /// assertion that fails the moment a run id, a conversation or an amount is added beside it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_committed_run_reports_its_state_and_nothing_else() {
        let exporter = InMemoryMetricExporter::default();
        let reader = PeriodicReader::builder(exporter.clone(), runtime::Tokio).build();
        let provider = SdkMeterProvider::builder().with_reader(reader).build();
        let meter = provider.meter(SCOPE);

        for status in RunStatus::ALL {
            record_run_status(&meter, status);
        }
        provider.force_flush().expect("flush");

        let batches = exporter.get_finished_metrics().expect("metrics");
        let resource = batches.first().expect("one resource");
        let scope = resource.scope_metrics().next().expect("one scope");
        let metric = scope.metrics().next().expect("one metric");
        assert_eq!(metric.name(), RUNS);

        let data = match metric.data() {
            data::AggregatedMetrics::U64(data::MetricData::Sum(sum)) => sum,
            other => panic!("not a u64 sum: {other:?}"),
        };
        let mut seen: Vec<(String, u64)> = data
            .data_points()
            .map(|point| {
                let attributes: Vec<(String, String)> = point
                    .attributes()
                    .map(|kv| (kv.key.to_string(), kv.value.as_str().into_owned()))
                    .collect();
                assert_eq!(
                    attributes.len(),
                    1,
                    "a label beyond `state` reached the exporter: {attributes:?}"
                );
                assert_eq!(attributes[0].0, "state", "{attributes:?}");
                (attributes[0].1.clone(), point.value())
            })
            .collect();
        seen.sort();
        let mut expected: Vec<(String, u64)> = RunStatus::ALL
            .iter()
            .map(|status| (state_of(*status).as_str().to_owned(), 1))
            .collect();
        expected.sort();
        assert_eq!(seen, expected);
    }

    /// A state the store adds cannot silently become a free-form label: `state_of` is exhaustive,
    /// so this test's value is that the mapping is total and injective.
    #[test]
    fn every_state_maps_to_its_own_label() {
        let mut labels: Vec<&str> = RunStatus::ALL
            .iter()
            .map(|status| state_of(*status).as_str())
            .collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), RunStatus::ALL.len());
        assert_eq!(labels, vec!["done", "failed", "parked", "runnable"]);
    }
}
