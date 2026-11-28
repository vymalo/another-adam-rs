//! The dashboards of `deploy/dashboards/` cannot query a metric the service does not emit, nor a
//! label the instrument does not declare (ANR-1349).
//!
//! The generated JSON is the source of truth in the repository: `tools/dashboards/*.py` writes it,
//! CI regenerates it and fails on a difference (`tools/dashboards/regenerate.sh`), and this test
//! holds every query to [`adam_otel::catalogue::EMITTED`] — the instruments the workspace records —
//! and to the closed label sets of [`adam_otel::labels`]. A panel that asks for a metric nobody
//! emits, or groups by a label no instrument carries, fails here rather than drawing a blank graph
//! nobody notices.
//!
//! A target declares what it queries in two keys Grafana ignores:
//!
//! * `otelMetrics`: the OTel names of the instruments its expression reads;
//! * `otelLabels`: the labels it groups or matches on.
//!
//! The test checks both against the catalogue, and checks the expression agrees with them: the
//! `adam_`-prefixed names it contains are exactly the declared instruments' Prometheus names, and
//! every label it names is declared. A metric that is emitted but unreachable from a dashboard is
//! not a failure here — this is the direction that keeps a graph honest.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::fs;
use std::path::{Path, PathBuf};

use adam_otel::catalogue::{self, Instrument};
use adam_otel::labels::RunState;
use serde_json::Value;

/// The dashboards directory, from this crate's manifest: `crates/adam-otel` to the repository root.
fn directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/dashboards")
}

/// Every `*.json` dashboard, sorted by name so a failure names the same file twice.
fn dashboards() -> Vec<(PathBuf, Value)> {
    let dir = directory();
    let mut paths: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|error| panic!("{} is not a directory: {error}", dir.display()))
        .map(|entry| entry.expect("a directory entry").path())
        .filter(|path| path.extension().and_then(|extension| extension.to_str()) == Some("json"))
        .collect();
    paths.sort();
    assert!(
        !paths.is_empty(),
        "no dashboard in {}: a green test over nothing proves nothing",
        dir.display()
    );
    paths
        .into_iter()
        .map(|path| {
            let source = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            let value: Value = serde_json::from_str(&source)
                .unwrap_or_else(|error| panic!("{} is not JSON: {error}", path.display()));
            (path, value)
        })
        .collect()
}

/// The panels of a dashboard; an empty list when the key is absent or is not an array.
fn panels(dashboard: &Value) -> &[Value] {
    dashboard["panels"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// The `adam_`-prefixed names an expression contains, deduplicated and sorted.
///
/// The translation of the name is the catalogue's ([`Instrument::prometheus_name`]); this only
/// finds what the expression actually spells.
fn metric_names(expr: &str) -> Vec<String> {
    let mut names = Vec::new();
    for (start, _) in expr.match_indices("adam_") {
        let rest = &expr[start..];
        let end = rest
            .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
            .unwrap_or(rest.len());
        names.push(rest[..end].to_owned());
    }
    names.sort();
    names.dedup();
    names
}

/// The label names an expression groups by (`by (...)`, `without (...)`) or matches on
/// (`name="…"`, `name!="…"`, `name=~"…"`, `name!~"…"`), deduplicated and sorted.
fn label_names(expr: &str) -> Vec<String> {
    let mut names = Vec::new();
    for clause in ["by (", "without ("] {
        for (start, _) in expr.match_indices(clause) {
            let rest = &expr[start + clause.len()..];
            let Some(end) = rest.find(')') else { continue };
            for name in rest[..end].split(',') {
                let name = name.trim();
                if !name.is_empty() {
                    names.push(name.to_owned());
                }
            }
        }
    }
    for operator in ["=~\"", "!~\"", "!=\"", "=\""] {
        for (start, _) in expr.match_indices(operator) {
            let head = &expr[..start];
            let begin = head
                .rfind(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
                .map_or(0, |position| position + 1);
            let name = &head[begin..];
            if !name.is_empty() {
                names.push(name.to_owned());
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

fn strings(value: &Value, key: &str, path: &Path) -> Vec<String> {
    value[key]
        .as_array()
        .unwrap_or_else(|| panic!("{}: `{key}` is not an array: {value}", path.display()))
        .iter()
        .map(|item| {
            item.as_str()
                .unwrap_or_else(|| panic!("{}: `{key}` holds a non-string: {item}", path.display()))
                .to_owned()
        })
        .collect()
}

/// The decisive test: every instrument and label a panel declares is one the service emits.
#[test]
fn every_metric_a_panel_queries_is_one_the_service_emits() {
    let mut targets = 0;
    for (path, dashboard) in dashboards() {
        assert!(
            !dashboard["uid"].as_str().unwrap_or_default().is_empty(),
            "{}: a dashboard without a uid cannot be imported twice",
            path.display()
        );
        for panel in panels(&dashboard) {
            let Some(targets_of_panel) = panel["targets"].as_array() else {
                continue;
            };
            for target in targets_of_panel {
                let declared = strings(target, "otelMetrics", &path);
                assert!(
                    !declared.is_empty(),
                    "{}: a target declares no `otelMetrics`: {target}",
                    path.display()
                );
                let labels = strings(target, "otelLabels", &path);
                let expr = target["expr"].as_str().unwrap_or_else(|| {
                    panic!("{}: a target has no expr: {target}", path.display())
                });

                let mut instruments: Vec<&Instrument> = Vec::new();
                for name in &declared {
                    let instrument = catalogue::emitted(name).unwrap_or_else(|| {
                        panic!(
                            "{}: a panel queries `{name}`, which the service does not emit; the emitted list is {:?}",
                            path.display(),
                            catalogue::EMITTED
                                .iter()
                                .map(|instrument| instrument.name)
                                .collect::<Vec<_>>()
                        )
                    });
                    assert!(
                        expr.contains(instrument.prometheus_name().as_str()),
                        "{}: the expr of `{name}` does not query `{}`: {expr}",
                        path.display(),
                        instrument.prometheus_name()
                    );
                    for label in &labels {
                        assert!(
                            instrument.labels.contains(&label.as_str()),
                            "{}: `{name}` declares the label `{label}`, which the instrument does not carry ({:?})",
                            path.display(),
                            instrument.labels
                        );
                    }
                    instruments.push(instrument);
                }

                // The expression and its declaration agree, in both directions: no `adam_` name the
                // target did not declare, and no label it did not declare.
                let mut expected: Vec<String> = instruments
                    .iter()
                    .map(|instrument| instrument.prometheus_name())
                    .collect();
                expected.sort();
                expected.dedup();
                assert_eq!(
                    metric_names(expr),
                    expected,
                    "{}: the expr of `{declared:?}` queries a different set: {expr}",
                    path.display()
                );
                for label in label_names(expr) {
                    assert!(
                        labels.contains(&label),
                        "{}: the expr of `{declared:?}` names the label `{label}`, which `otelLabels` does not declare: {expr}",
                        path.display()
                    );
                }
                targets += 1;
            }
        }
    }
    assert!(
        targets > 0,
        "no panel target was checked: a green test over no query proves nothing"
    );
}

/// The `state` variable of a dashboard offers the closed set and nothing else, so a person cannot
/// build a selector for a state the instrument can never report.
#[test]
fn the_state_variable_holds_only_the_closed_set() {
    let mut checked = 0;
    for (path, dashboard) in dashboards() {
        let Some(variables) = dashboard["templating"]["list"].as_array() else {
            continue;
        };
        for variable in variables {
            if variable["name"].as_str() != Some("state") {
                continue;
            }
            let options = variable["options"].as_array().unwrap_or_else(|| {
                panic!(
                    "{}: the state variable has no options: {variable}",
                    path.display()
                )
            });
            for option in options {
                let value = option["value"].as_str().unwrap_or_default();
                assert!(
                    RunState::parse(value).is_some(),
                    "{}: the state variable offers `{value}`, which is not a run state ({:?})",
                    path.display(),
                    RunState::ALL.map(RunState::as_str)
                );
                checked += 1;
            }
        }
    }
    assert!(
        checked > 0,
        "no `state` variable was checked: a green test over nothing proves nothing"
    );
}
