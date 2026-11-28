# Observability: the emitted instruments and the dashboards

For people: [the `adam-otel` crate README](../../../crates/adam-otel/README.md) (the layer and its
metrics) and [deploy/dashboards](../../../deploy/dashboards/README.md) (what each dashboard shows).
This page has no page for people at its own path: the reference for a metric is its instrument, and
the reference for a dashboard is its JSON.

## The invariants to check

1. **A metric a dashboard queries is a metric the workspace creates from an OTel meter.** The list is
   `crates/adam-otel/src/catalogue.rs` (`EMITTED`); `crates/adam-otel/tests/dashboards.rs` reads
   `deploy/dashboards/*.json` and fails on a name outside it. The list holds what a meter records, not
   what ADR 0075's table plans: an instrument enters it in the change that records it. `adam-service`'s
   `adam_admission_*` gauges are rendered as Prometheus text from no meter and are not in it.
2. **A label is a closed set.** `adam_otel::labels` holds `RunState` (held to
   `adam_core::store::RunStatus` by a test in `crates/adam-otel/src/labels.rs`) and `Name`; the
   catalogue repeats an instrument's labels, and the dashboards' test fails on a label the
   instrument does not carry. A run id, a conversation, a token, an amount or a person's address
   cannot pass that check.
3. **The JSON is generated.** `tools/dashboards/<name>.py` (standard library only, one script per
   dashboard) writes `deploy/dashboards/<name>.json`; `tools/dashboards/regenerate.sh` runs them
   all. A hand edit is a difference and CI's `fmt` job fails on it (`ci.yml`, step "The generated
   dashboards are up to date": regenerate, then `git diff --exit-code -- deploy/dashboards`). The
   script also fails on an orphan `*.json` with no `<name>.py`, and on a run where no script ran at
   all.
4. **A target declares what it queries.** Two keys Grafana ignores — `otelMetrics` (dotted OTel
   names) and `otelLabels` — are what the test reads, and the test also holds the expression to
   them: the `adam_`-prefixed names it spells must be exactly the declared instruments' Prometheus
   names (`.`, `_`, `_total`), and every label it names must be declared. A label is found in every
   form PromQL has for one: `by`, `without`, `on`, `ignoring`, `group_left`, `group_right` (with or
   without a space before the parenthesis) and the four matcher operators. The forms the test does
   **not** read — `label_replace` and `label_join`, which mint a label — fail it outright, so a panel
   cannot use one unchecked.
5. **A templating variable is checked too.** `label_values(<metric>, <label>)` and
   `label_values(<label>)` in `templating.list[].query` are held to the same catalogue, and the
   `state` variable's query must be exactly `RunState::ALL` — the closed set, in order — with the
   same four values in its option list.

## Adding a metric

1. Record it in the crate that owns the seam, from `opentelemetry::global::meter(scope)` — a no-op
   meter when `adam_otel::init` found no endpoint, so nothing has to be threaded through.
2. Label it with a type from `adam_otel::labels`, never a string from a run.
3. Add its line to `catalogue::EMITTED` (`name`, `kind`, `unit`, `labels`, `emitted_by`) in the same
   change. A test in `crates/adam-runtime/src/metrics.rs` asserts the emitted attribute set is
   exactly the declared labels — see "a label beyond `state` reached the exporter".
4. Update the crate README of the emitter and `docs/reference/environment.md` when a name or a
   switch changes.

## Adding a dashboard

1. Write `tools/dashboards/<name>.py`, standard library only, that writes `deploy/dashboards/<name>.json`
   through `json.dumps(..., indent=2, ensure_ascii=False) + "\n"` — the bytes must be stable.
2. Query only `catalogue::EMITTED` instruments and their declared labels; declare them in each
   target's `otelMetrics` and `otelLabels`. A label the parser does not read (`label_replace`,
   `label_join`) fails the test: extend it first.
3. Run `python3 tools/dashboards/<name>.py` twice and confirm the file is byte-identical, then run
   the test (`cargo nextest run -p adam-otel --test dashboards`).
4. Say in the pull request what a deployment does with the JSON. Nothing in `deploy/coder` reads it
   today: the chart has no Grafana and no `otel.*` values (ADR 0075 decision 7, D1).

## Seen red

* The dashboards' test: change a panel's `otelMetrics` to a metric nobody emits, add a label to
  `otelLabels` that the instrument does not carry, or spell a label one of the unread ways
  (`sum by(run_id)`, `on(run_id)`, `label_values(adam_runs_total, run_id)`), and a test names the
  file and the name. `the_label_parser_reads_every_form` is the test that keeps the parser itself
  honest.
* The CI step: hand-edit `deploy/dashboards/runs.json` and `git diff --exit-code` fails with the
  diff; leave a regenerated `*.json` uncommitted and the untracked check fails; delete
  `tools/dashboards/runs.py` and the orphan check fails; check the tree out under a path holding
  `/_` and the run still regenerates (the helper skip matches the file's name, not its path).
