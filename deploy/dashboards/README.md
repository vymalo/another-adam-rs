# Dashboards

Grafana dashboards for the metrics `adam-otel` exports, as JSON. **Generated — do not edit.**
`tools/dashboards/<name>.py` writes `deploy/dashboards/<name>.json`, and CI regenerates every
dashboard and fails on a difference, so a hand edit is a change that cannot merge. Edit the script,
never the JSON — and do not export a dashboard from Grafana back into this directory: an export
drops the `otelMetrics`/`otelLabels` keys the test reads, and the next regeneration would undo it
anyway.

```sh
sh tools/dashboards/regenerate.sh     # every dashboard
python3 tools/dashboards/runs.py      # one, from any directory
```

The scripts are Python's standard library only: a dashboard is data, and it belongs in a script a
reviewer can read rather than in a Grafana export a person dragged.

## What is here

| Dashboard | Panels | What it answers |
|---|---|---|
| `runs.json` | transitions per second by state, transitions in range, failed per second, failure share, the state mix | how fast runs move between states, and how much of that is failure |

One dashboard, because the service emits one instrument today: `adam.runs`, a counter of committed
run transitions whose only label is `state` (`runnable`, `parked`, `done`, `failed`). It is a
**counter**, not a gauge, so every panel reads a `rate` or an `increase` — it says how fast runs
move, never how many are parked right now. A second dashboard waits for a second instrument family
(the queue, the step durations and the call counters of ADR 0075); a panel that queried one today
would be held back by the test below.

## How a panel is checked

`crates/adam-otel/tests/dashboards.rs` reads these files and holds every panel to
`adam_otel::catalogue::EMITTED` — the instruments the workspace actually records — and to the closed
label sets of `adam_otel::labels`. A panel that queries a metric nobody emits, or groups by a label
the instrument does not carry, fails a test rather than drawing a blank graph nobody notices.

To make that possible, each target carries two keys Grafana ignores:

| Key | What it is |
|---|---|
| `otelMetrics` | the OTel names the expression reads, dotted (`adam.runs`) |
| `otelLabels` | the labels it groups or matches on (`state`) |

A query spells the name a Prometheus-compatible store keeps: `adam.runs` is scraped as
`adam_runs_total` (`.` becomes `_`, a counter takes `_total` — *verified 2026-11-28* against
OpenMetrics 1.0, where a `# TYPE foo counter` family exposes `foo_total`), and the test checks that
the expression agrees with the name it declares.

## Using them

Nothing in `deploy/coder` reads these files: the chart has no Grafana and, today, no `otel.*` values
(ADR 0075 decision 7 is the chart slice). A deployment imports them itself — `grafana import`, or a
sidecar that loads a ConfigMap labelled `grafana_dashboard: "1"` — and needs two things first:

1. a Prometheus-compatible store receiving the process's OTLP metrics, which is
   `OTEL_EXPORTER_OTLP_ENDPOINT` set on the pod (see [the environment reference](../../docs/reference/environment.md));
2. a Prometheus datasource, chosen in the dashboard's `datasource` variable.

The `state` variable offers exactly the closed set the instrument can report, so no selector can ask
for a state the service never emits.
