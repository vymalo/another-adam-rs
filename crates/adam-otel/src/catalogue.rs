//! The catalogue of instruments the service emits: the list a dashboard panel is held to
//! (ANR-1349, [ADR 0075]).
//!
//! A panel that queries a metric nobody emits is a blank graph nobody notices, and a panel that
//! groups by a label the instrument does not carry is a series the deployment never planned for.
//! So the emitted list is data here, beside the closed label sets of [`crate::labels`], and the
//! dashboards' test (`crates/adam-otel/tests/dashboards.rs`) reads it: every instrument a target of
//! `deploy/dashboards/*.json` declares must be one of [`EMITTED`], and every label it declares must
//! be one the instrument carries.
//!
//! An instrument is listed once it is **created from an OTel meter**, not when ADR 0075 names it:
//! the decision's table is the plan, this list is what a deployment can scrape from the exporter.
//! Not every metric the process can print is here — `adam-service`'s `adam_admission_*` gauges are
//! rendered as Prometheus text by `adam-service`'s `metrics::render`, from no meter at all, and no
//! listener serves them yet; they are the next family to move here. Adding a metric means adding its
//! line here and in the crate that records it, in the same change.
//!
//! [ADR 0075]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0075-metrics-and-traces-are-off-by-default-and-say-nothing-about-a-person.md

/// The counter of committed run transitions, labelled by the run's state (ANR-1449).
///
/// Recorded by `adam-runtime` where a transition commits: the worker's step commit, a cancel, a
/// wake, and a run failed for an unreadable state.
pub const RUNS: &str = "adam.runs";

/// The kind of an instrument, the three [ADR 0075] uses.
///
/// [ADR 0075]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0075-metrics-and-traces-are-off-by-default-and-say-nothing-about-a-person.md
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A monotonic sum of events: only its rate or its increase over a window means anything.
    Counter,
    /// A value read at a moment.
    Gauge,
    /// A distribution of durations or sizes.
    Histogram,
}

/// One instrument the service emits: its name, its kind, its unit and the labels it may carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Instrument {
    /// The OTel name, dotted: `adam.runs`.
    pub name: &'static str,
    /// What kind of instrument it is.
    pub kind: Kind,
    /// The OTel unit, empty when the instrument declares none.
    pub unit: &'static str,
    /// The labels it may carry, from the closed sets in [`crate::labels`] — never an id, a token,
    /// an amount or a person.
    pub labels: &'static [&'static str],
    /// The crate that records it, so a reader knows where the instrument is created.
    pub emitted_by: &'static str,
}

impl Instrument {
    /// The name a Prometheus-compatible backend stores the instrument under.
    ///
    /// The OTel Prometheus exporter and the Prometheus OTLP receiver both turn the dotted name into
    /// an underscored one and append `_total` to a counter, so `adam.runs` is scraped as
    /// `adam_runs_total` (*verified 2026-11-28*: OpenMetrics 1.0, a `# TYPE foo counter` family
    /// exposes `foo_total`). A unit suffix is added only when the instrument declares a unit, and
    /// none does today.
    pub fn prometheus_name(&self) -> String {
        let base = self.name.replace('.', "_");
        match self.kind {
            Kind::Counter if !base.ends_with("_total") => format!("{base}_total"),
            _ => base,
        }
    }
}

/// Every instrument the service emits, in the order a dashboard should list them.
pub const EMITTED: &[Instrument] = &[Instrument {
    name: RUNS,
    kind: Kind::Counter,
    unit: "",
    labels: &["state"],
    emitted_by: "adam-runtime",
}];

/// The instrument `name` names, or `None` when the service does not emit it.
pub fn emitted(name: &str) -> Option<&'static Instrument> {
    EMITTED.iter().find(|instrument| instrument.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::labels::RunState;

    /// A duplicate name would make [`emitted`] answer one of two instruments, and a name outside
    /// the `adam.` prefix would be a series nobody can find beside the rest.
    #[test]
    fn the_emitted_names_are_unique_and_under_one_prefix() {
        let mut names: Vec<&str> = EMITTED.iter().map(|instrument| instrument.name).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(
            names.len(),
            count,
            "two instruments share a name: {names:?}"
        );
        for instrument in EMITTED {
            assert!(
                instrument.name.starts_with("adam."),
                "{} is not under the adam. prefix",
                instrument.name
            );
            assert!(
                !instrument.emitted_by.is_empty(),
                "{} does not say which crate records it",
                instrument.name
            );
        }
    }

    /// The list is not empty, so the dashboards' test cannot pass over nothing.
    #[test]
    fn something_is_emitted() {
        assert!(!EMITTED.is_empty());
        assert!(emitted(RUNS).is_some());
        assert!(emitted("adam.nothing").is_none());
    }

    /// The label of the one instrument is the closed set's, spelled the way the instrument spells
    /// it: a dashboard that groups by `state` groups by [`RunState`]'s values.
    #[test]
    fn the_run_label_is_the_closed_sets() {
        let runs = emitted(RUNS).expect("the runs counter is emitted");
        assert_eq!(runs.labels, &["state"]);
        let declared: Vec<&str> = RunState::ALL.iter().map(|state| state.as_str()).collect();
        assert_eq!(declared, vec!["runnable", "parked", "done", "failed"]);
    }

    #[test]
    fn a_counter_is_scraped_with_its_total_suffix() {
        let runs = emitted(RUNS).expect("the runs counter is emitted");
        assert_eq!(runs.prometheus_name(), "adam_runs_total");
        let gauge = Instrument {
            name: "adam.leases_held",
            kind: Kind::Gauge,
            unit: "",
            labels: &[],
            emitted_by: "adam-runtime",
        };
        assert_eq!(gauge.prometheus_name(), "adam_leases_held");
        let histogram = Instrument {
            name: "adam.step_duration_seconds",
            kind: Kind::Histogram,
            unit: "s",
            labels: &["kind", "outcome"],
            emitted_by: "adam-runtime",
        };
        assert_eq!(
            histogram.prometheus_name(),
            "adam_step_duration_seconds",
            "a histogram keeps its unit in its name and takes no _total"
        );
    }
}
