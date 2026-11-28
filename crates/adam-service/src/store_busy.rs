//! The store's busy count as an instrument (ANR-1506).
//!
//! Off with the rest: the gauge is built from the global meter, which answers a no-op meter in a
//! process where `adam_otel::init` found no endpoint (ADR 0075), so a deployment without one pays
//! nothing. It carries **no labels** — the store's activity is one number for the service, never a
//! run id or an agent name.
//!
//! This is not [`crate::metrics`]: that renders the admission pools as Prometheus text for a
//! listener to serve, while this is an OTel instrument the exporter carries.

use opentelemetry::metrics::Meter;

/// The instrumentation scope of this crate's instruments.
pub const SCOPE: &str = "adam-service";

/// The gauge of the runs a worker is stepping right now.
///
/// The name is the catalogue's ([`adam_otel::catalogue::STORE_BUSY`]), so the dashboards' test and
/// the instrument cannot drift apart.
pub use adam_otel::catalogue::STORE_BUSY;

/// Records the store's busy count.
///
/// The meter is a parameter so that a test can hand in one from an in-memory exporter; the caller
/// passes the process's ([`opentelemetry::global::meter`]).
pub fn record_store_busy(meter: &Meter, busy: u64) {
    meter.u64_gauge(STORE_BUSY).build().record(busy, &[]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry_sdk::metrics::InMemoryMetricExporter;
    use opentelemetry_sdk::metrics::periodic_reader_with_async_runtime::PeriodicReader;
    use opentelemetry_sdk::metrics::{SdkMeterProvider, data};
    use opentelemetry_sdk::runtime;

    /// The gauge reports the count and carries **no attributes** — the assertion that fails the
    /// moment a run id, an agent name or a repository is added beside it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_store_busy_gauge_carries_no_attributes() {
        let exporter = InMemoryMetricExporter::default();
        let reader = PeriodicReader::builder(exporter.clone(), runtime::Tokio).build();
        let provider = SdkMeterProvider::builder().with_reader(reader).build();
        let meter = provider.meter(SCOPE);

        record_store_busy(&meter, 3);
        provider.force_flush().expect("flush");

        let batches = exporter.get_finished_metrics().expect("metrics");
        let resource = batches.first().expect("one resource");
        let scope = resource.scope_metrics().next().expect("one scope");
        let metric = scope.metrics().next().expect("one metric");
        assert_eq!(metric.name(), STORE_BUSY);

        let data = match metric.data() {
            data::AggregatedMetrics::U64(data::MetricData::Gauge(gauge)) => gauge,
            other => panic!("not a u64 gauge: {other:?}"),
        };
        let points: Vec<_> = data.data_points().collect();
        assert_eq!(points.len(), 1, "one series, no labels");
        let point = points[0];
        assert_eq!(point.value(), 3);
        let attributes: Vec<_> = point.attributes().map(|a| a.key.as_str()).collect();
        assert!(
            attributes.is_empty(),
            "the store's busy gauge carries no labels, got {attributes:?}"
        );
    }
}
