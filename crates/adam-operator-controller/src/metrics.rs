//! A few counters in the Prometheus text format. No client library: a counter is a number, and the
//! exposition format is lines of text.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

/// The counters the controllers keep. Cheap to clone: share it in an `Arc`.
#[derive(Debug, Default)]
pub struct Metrics {
    series: Mutex<BTreeMap<(&'static str, String), u64>>,
}

/// Names of the series, and what they count.
pub mod name {
    /// Passes of a reconciler, by `controller` and `result` (`ok` or `error`).
    pub const RECONCILES: &str = "adam_operator_reconcile_total";
    /// Failed passes, by `controller` and the `class` of the error.
    pub const ERRORS: &str = "adam_operator_reconcile_errors_total";
    /// Status patches written, by `controller`.
    pub const STATUS_PATCHES: &str = "adam_operator_status_patches_total";
    /// Services whose state changed, by the new `state`.
    pub const STATE_CHANGES: &str = "adam_operator_service_state_changes_total";
    /// Ids heard from `RuntimeProvider::watch()`.
    pub const RUNTIME_SIGNALS: &str = "adam_operator_runtime_signals_total";
    /// Services deleted: the finalizer ran `RuntimeProvider::delete` and the store's release.
    pub const SERVICES_DELETED: &str = "adam_operator_services_deleted_total";
}

fn help(series: &str) -> &'static str {
    match series {
        name::RECONCILES => "Reconcile passes.",
        name::ERRORS => "Reconcile passes that failed, by error class.",
        name::STATUS_PATCHES => "Status patches written.",
        name::STATE_CHANGES => "AgentService state changes, by the new state.",
        name::RUNTIME_SIGNALS => "Runtime ids reported by the provider's watch.",
        name::SERVICES_DELETED => "AgentServices whose runtime and store were released.",
        _ => "",
    }
}

fn labels(pairs: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(k);
        out.push_str("=\"");
        for c in v.chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '"' => out.push_str("\\\""),
                '\n' => out.push_str("\\n"),
                c => out.push(c),
            }
        }
        out.push('"');
    }
    out
}

impl Metrics {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one to the series `series{labels}`.
    pub fn inc(&self, series: &'static str, label_pairs: &[(&str, &str)]) {
        let mut map = self.series.lock().unwrap_or_else(PoisonError::into_inner);
        *map.entry((series, labels(label_pairs))).or_insert(0) += 1;
    }

    /// The value of a series, 0 when it was never touched.
    pub fn get(&self, series: &'static str, label_pairs: &[(&str, &str)]) -> u64 {
        let map = self.series.lock().unwrap_or_else(PoisonError::into_inner);
        map.get(&(series, labels(label_pairs)))
            .copied()
            .unwrap_or(0)
    }

    /// The text of a scrape.
    pub fn render(&self) -> String {
        let map = self.series.lock().unwrap_or_else(PoisonError::into_inner);
        let mut out = String::new();
        let mut last = "";
        for ((series, label_text), value) in map.iter() {
            if *series != last {
                out.push_str(&format!(
                    "# HELP {series} {}\n# TYPE {series} counter\n",
                    help(series)
                ));
                last = series;
            }
            if label_text.is_empty() {
                out.push_str(&format!("{series} {value}\n"));
            } else {
                out.push_str(&format!("{series}{{{label_text}}} {value}\n"));
            }
        }
        out
    }
}
