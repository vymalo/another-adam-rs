//! The dashboards of `deploy/dashboards/` cannot query a metric the service does not emit, nor a
//! label the instrument does not declare (ANR-1349).
//!
//! The generated JSON is the source of truth in the repository: `tools/dashboards/*.py` writes it,
//! CI regenerates it and fails on a difference (`tools/dashboards/regenerate.sh`), and this test
//! holds every query to [`adam_otel::catalogue::EMITTED`] — the instruments the workspace creates
//! from an OTel meter — and to the closed label sets of [`adam_otel::labels`]. A panel that asks for
//! a metric nobody emits, or groups by a label no instrument carries, fails here rather than drawing
//! a blank graph nobody notices.
//!
//! A panel target declares what it queries in two keys Grafana ignores:
//!
//! * `otelMetrics`: the OTel names of the instruments its expression reads;
//! * `otelLabels`: the labels it groups or matches on.
//!
//! The test checks both against the catalogue, and checks the expression agrees with them: the
//! `adam_`-prefixed names it contains are exactly the declared instruments' Prometheus names, and
//! every label it names is declared. A label is found in every form PromQL has for one — `by (…)`,
//! `without (…)`, `on (…)`, `ignoring (…)`, `group_left (…)`, `group_right (…)`, and the
//! `name="…"`, `name!="…"`, `name=~"…"`, `name!~"…"` matchers, with or without a space before the
//! parenthesis. A form this test does not read (`label_replace`, `label_join`) fails it outright
//! rather than passing unchecked.
//!
//! A templating variable is checked too: a `label_values(…)` query names a metric and a label in
//! the one place a dashboard can do it outside a panel target, so both are held to the catalogue.
//!
//! A metric that is emitted but unreachable from a dashboard is not a failure here — this is the
//! direction that keeps a graph honest.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::fs;
use std::path::{Path, PathBuf};

use adam_otel::catalogue::{self, Instrument};
use adam_otel::labels::RunState;
use serde_json::Value;

/// The PromQL keywords whose parenthesised list is a set of labels.
const GROUPING: [&str; 6] = [
    "by",
    "without",
    "on",
    "ignoring",
    "group_left",
    "group_right",
];

/// The forms that mint a label and that this test does not read. A panel that uses one fails rather
/// than escaping the check; extending the parser is the fix.
const UNPARSED: [&str; 2] = ["label_replace(", "label_join("];

/// The dashboards directory, from this crate's manifest: `crates/adam-otel` to the repository root.
fn directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/dashboards")
}

/// The scripts directory, beside the dashboards.
fn scripts() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/dashboards")
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

/// The label names an expression groups by or matches on, deduplicated and sorted.
fn label_names(expr: &str) -> Vec<String> {
    let mut names = Vec::new();
    grouping_labels(expr, &mut names);
    matcher_labels(expr, &mut names);
    names.sort();
    names.dedup();
    names
}

/// `by (a, b)`, `without (…)`, `on (…)`, `ignoring (…)`, `group_left (…)`, `group_right (…)` — a
/// keyword at a word boundary, any whitespace, then a parenthesised list.
fn grouping_labels(expr: &str, names: &mut Vec<String>) {
    for keyword in GROUPING {
        let mut from = 0;
        while let Some(offset) = expr[from..].find(keyword) {
            let start = from + offset;
            from = start + keyword.len();
            // A keyword inside a longer word (`region`, `group_leftx`) is not one.
            if expr[..start]
                .chars()
                .next_back()
                .is_some_and(|before| before.is_ascii_alphanumeric() || before == '_')
            {
                continue;
            }
            let Some(rest) = expr[from..].trim_start().strip_prefix('(') else {
                continue;
            };
            let Some(end) = rest.find(')') else { continue };
            for name in rest[..end].split(',') {
                let name = name.trim();
                if !name.is_empty() {
                    names.push(name.to_owned());
                }
            }
        }
    }
}

/// `name="…"`, `name!="…"`, `name=~"…"`, `name!~"…"` — the word before the operator.
fn matcher_labels(expr: &str, names: &mut Vec<String>) {
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
}

/// The instrument a Prometheus name belongs to, when the service emits it.
fn emitted_prometheus(name: &str) -> Option<&'static Instrument> {
    catalogue::EMITTED
        .iter()
        .find(|instrument| instrument.prometheus_name() == name)
}

/// `label_values(<selector>, <label>)` or `label_values(<label>)`, the two forms a Grafana
/// templating query uses to name a Prometheus label.
fn label_values(query: &str) -> Option<(Option<String>, String)> {
    let rest = query.trim().strip_prefix("label_values")?;
    let inner = rest.trim_start().strip_prefix('(')?;
    let inner = inner.strip_suffix(')')?.trim();
    match split_selector(inner) {
        (selector, Some(label)) => Some((Some(selector_metric(&selector).to_owned()), label)),
        (label, None) => Some((None, label)),
    }
}

/// Split a `label_values` argument list at its first top-level comma.
fn split_selector(inner: &str) -> (String, Option<String>) {
    let mut depth = 0usize;
    let mut quoted = false;
    for (index, character) in inner.char_indices() {
        match character {
            '"' => quoted = !quoted,
            '{' if !quoted => depth += 1,
            '}' if !quoted => depth = depth.saturating_sub(1),
            ',' if !quoted && depth == 0 => {
                return (
                    inner[..index].trim().to_owned(),
                    Some(inner[index + 1..].trim().to_owned()),
                );
            }
            _ => {}
        }
    }
    (inner.trim().to_owned(), None)
}

/// The metric a selector names, without its label matchers.
fn selector_metric(selector: &str) -> &str {
    match selector.find('{') {
        Some(index) => selector[..index].trim(),
        None => selector.trim(),
    }
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
                for form in UNPARSED {
                    assert!(
                        !expr.contains(form),
                        "{}: the expr uses `{form}`, which this test does not read; extend the parser before using it: {expr}",
                        path.display()
                    );
                }

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
                            instrument
                                .labels
                                .iter()
                                .any(|carried| *carried == label.as_str()),
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

/// A templating variable names a metric and a label too: `label_values(adam_runs_total, state)`,
/// `label_values(adam_runs_total{state="done"}, state)` or `label_values(state)`.
#[test]
fn every_metric_and_label_a_template_variable_names_is_emitted() {
    let mut inspected = 0;
    for (path, dashboard) in dashboards() {
        let Some(variables) = dashboard["templating"]["list"].as_array() else {
            continue;
        };
        for variable in variables {
            let Some(query) = variable["query"].as_str() else {
                continue;
            };
            inspected += 1;
            // A custom variable's query is its option list, not a PromQL selector; the `state` test
            // holds that one to the closed set.
            if variable["type"].as_str() == Some("custom") {
                continue;
            }
            for name in metric_names(query) {
                assert!(
                    emitted_prometheus(&name).is_some(),
                    "{}: a template variable queries `{name}`, which the service does not emit: {query}",
                    path.display()
                );
            }
            match label_values(query) {
                Some((Some(metric), label)) => {
                    let instrument = emitted_prometheus(&metric).unwrap_or_else(|| {
                        panic!(
                            "{}: a template variable reads the label `{label}` of `{metric}`, which the service does not emit: {query}",
                            path.display()
                        )
                    });
                    assert!(
                        instrument
                            .labels
                            .iter()
                            .any(|carried| *carried == label.as_str()),
                        "{}: a template variable reads the label `{label}` of `{metric}`, which the instrument does not carry ({:?})",
                        path.display(),
                        instrument.labels
                    );
                }
                Some((None, label)) => assert!(
                    catalogue::EMITTED.iter().any(|instrument| instrument
                        .labels
                        .iter()
                        .any(|carried| *carried == label.as_str())),
                    "{}: a template variable reads the label `{label}`, which no emitted instrument carries: {query}",
                    path.display()
                ),
                None => assert!(
                    !query.trim_start().starts_with("label_values"),
                    "{}: a `label_values` query was not parsed, so nothing held it to the catalogue: {query}",
                    path.display()
                ),
            }
        }
    }
    assert!(
        inspected > 0,
        "no template variable with a query was inspected: a green test over nothing proves nothing"
    );
}

/// The `state` variable of a dashboard offers the closed set and nothing else, in both places
/// Grafana reads: the query it parses on import, and the option list it shows.
#[test]
fn the_state_variable_holds_only_the_closed_set() {
    let closed: Vec<&str> = RunState::ALL.iter().map(|state| state.as_str()).collect();
    let mut checked = 0;
    for (path, dashboard) in dashboards() {
        let Some(variables) = dashboard["templating"]["list"].as_array() else {
            continue;
        };
        for variable in variables {
            if variable["name"].as_str() != Some("state") {
                continue;
            }
            let query = variable["query"].as_str().unwrap_or_else(|| {
                panic!(
                    "{}: the state variable has no query: {variable}",
                    path.display()
                )
            });
            let listed: Vec<&str> = query.split(',').map(str::trim).collect();
            assert_eq!(
                listed,
                closed,
                "{}: the state variable's query is not the closed set, in its order: {query}",
                path.display()
            );

            let options = variable["options"].as_array().unwrap_or_else(|| {
                panic!(
                    "{}: the state variable has no options: {variable}",
                    path.display()
                )
            });
            assert_eq!(
                options.len(),
                RunState::ALL.len(),
                "{}: the state variable offers {} options, not the closed set's {}: {query}",
                path.display(),
                options.len(),
                RunState::ALL.len()
            );
            for option in options {
                let value = option["value"].as_str().unwrap_or_default();
                assert!(
                    RunState::parse(value).is_some(),
                    "{}: the state variable offers `{value}`, which is not a run state ({closed:?})",
                    path.display()
                );
            }
            checked += 1;
        }
    }
    assert!(
        checked > 0,
        "no `state` variable was checked: a green test over nothing proves nothing"
    );
}

/// A dashboard no script writes is one regeneration can never repair: the CI step would be green
/// over it, because there is nothing to regenerate.
#[test]
fn every_dashboard_has_a_script() {
    let scripts = scripts();
    for (path, _) in dashboards() {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let stem = name.strip_suffix(".json").unwrap_or(name);
        let script = scripts.join(format!("{stem}.py"));
        assert!(
            script.is_file(),
            "{} is generated by nothing: tools/dashboards/{stem}.py does not exist",
            path.display()
        );
    }
}

/// The parser finds a label in every form PromQL has for one, so a panel cannot slip an undeclared
/// label past the check by spelling it another way.
#[test]
fn the_label_parser_reads_every_form() {
    let cases: [(&str, &[&str]); 8] = [
        ("sum by (state) (rate(adam_runs_total[5m]))", &["state"]),
        ("sum by(state) (rate(adam_runs_total[5m]))", &["state"]),
        ("sum without (instance) (x)", &["instance"]),
        (
            "sum by (state) (rate(a)) / on(run_id) group_left(owner) sum by (state) (rate(b))",
            &["owner", "run_id", "state"],
        ),
        (
            "sum(a) / ignoring (instance) group_right sum(b)",
            &["instance"],
        ),
        (
            "adam_runs_total{state=\"failed\",run_id=~\"x\"}",
            &["run_id", "state"],
        ),
        ("adam_runs_total{state!=\"failed\"}", &["state"]),
        ("rate(adam_runs_total[5m])", &[]),
    ];
    for (expr, expected) in cases {
        assert_eq!(label_names(expr), expected.to_vec(), "{expr}");
    }

    assert_eq!(
        label_values("label_values(adam_runs_total, state)"),
        Some((Some("adam_runs_total".to_owned()), "state".to_owned()))
    );
    assert_eq!(
        label_values("label_values( adam_runs_total{state=\"done\"} , state )"),
        Some((Some("adam_runs_total".to_owned()), "state".to_owned()))
    );
    assert_eq!(
        label_values("label_values(state)"),
        Some((None, "state".to_owned()))
    );
    assert_eq!(label_values("prometheus"), None);
}
