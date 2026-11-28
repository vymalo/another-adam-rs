#!/usr/bin/env python3
"""Write ``deploy/dashboards/runs.json``: the run-lifecycle dashboard (ANR-1349).

Standard library only, on purpose (the owner's ``otel-conventions`` of 2026-11-27): a dashboard is
data, and data belongs in a script a reviewer can read, not in a Grafana export a person dragged.
Run it, or ``sh tools/dashboards/regenerate.sh`` for all of them, and commit the result; CI
regenerates and fails on a difference.

Every panel queries ``adam.runs`` — the counter of committed run transitions — and touches only
``state``, the one label the instrument carries, whose values are the closed set
``adam_otel::labels::RunState``. ``adam.runs`` is a counter of transitions, not a gauge of runs: a
panel says how fast runs move, never how many are parked right now, so every expression is a
``rate`` or an ``increase``.

A target carries two keys Grafana ignores and ``crates/adam-otel/tests/dashboards.rs`` reads:

``otelMetrics``
    the OTel instruments the expression queries, by their dotted names, each of which must be in
    ``adam_otel::catalogue::EMITTED``;
``otelLabels``
    the labels the expression groups or matches on, each of which must be one the instrument
    declares — no id, token, amount or person label can pass that check.
"""

from __future__ import annotations

import json
import pathlib

# The instrument, in the two spellings that matter: the OTel name (the test holds it to the emitted
# list) and the name a Prometheus-compatible backend stores it under. The OTel Prometheus exporter
# and the Prometheus OTLP receiver both turn `.` into `_` and append `_total` to a counter.
METRIC = "adam.runs"
PROM = "adam_runs_total"

# The closed label set of the instrument, spelled as `RunState::as_str` spells it.
STATES = ["runnable", "parked", "done", "failed"]

DATASOURCE = {"type": "prometheus", "uid": "${datasource}"}

ALL_STATES = '{state=~"$state"}'
BY_STATE_PER_SECOND = f"sum by (state) (rate({PROM}{ALL_STATES}[$__rate_interval]))"
BY_STATE_IN_RANGE = f"sum by (state) (increase({PROM}{ALL_STATES}[$__range]))"
TOTAL_IN_RANGE = f"sum(increase({PROM}{ALL_STATES}[$__range]))"
FAILED_PER_SECOND = f'sum(rate({PROM}{{state="failed"}}[$__rate_interval]))'
FAILURE_SHARE = (
    f'sum(rate({PROM}{{state="failed"}}[$__rate_interval]))'
    f" / sum(rate({PROM}[$__rate_interval]))"
)

DESCRIPTION = (
    "Committed run transitions, counted where the runtime commits a run's state. "
    "adam.runs is a counter, so every panel reads a rate or an increase: it says how fast runs "
    "move between states, not how many are parked now."
)


def target(expr, legend, labels=("state",)):
    """One Prometheus query, with the OTel instrument and labels it uses declared for the test."""
    return {
        "datasource": DATASOURCE,
        "editorMode": "code",
        "expr": expr,
        "legendFormat": legend,
        "otelLabels": list(labels),
        "otelMetrics": [METRIC],
        "range": True,
        "refId": "A",
    }


def panels():
    return [
        {
            "id": 1,
            "type": "timeseries",
            "title": "Transitions per second, by state",
            "description": "How fast runs move, split by the state they committed. A flat line at "
            "zero with a busy fleet means no run is finishing.",
            "gridPos": {"h": 8, "w": 16, "x": 0, "y": 0},
            "datasource": DATASOURCE,
            "fieldConfig": {
                "defaults": {
                    "unit": "ops",
                    "min": 0,
                    "custom": {
                        "drawStyle": "line",
                        "fillOpacity": 10,
                        "lineInterpolation": "linear",
                        "lineWidth": 2,
                        "showPoints": "never",
                        "spanNulls": False,
                    },
                },
                "overrides": [],
            },
            "options": {
                "legend": {"calcs": [], "displayMode": "list", "placement": "bottom", "showLegend": True},
                "tooltip": {"mode": "multi", "sort": "desc"},
            },
            "targets": [target(BY_STATE_PER_SECOND, "{{state}}")],
        },
        {
            "id": 2,
            "type": "stat",
            "title": "Transitions in range",
            "description": "Every committed transition of the selected states over the dashboard's "
            "time range: the traffic the other panels are a rate of.",
            "gridPos": {"h": 8, "w": 8, "x": 16, "y": 0},
            "datasource": DATASOURCE,
            "fieldConfig": {
                "defaults": {
                    "color": {"mode": "thresholds"},
                    "unit": "short",
                    "thresholds": {"mode": "absolute", "steps": [{"color": "blue", "value": None}]},
                },
                "overrides": [],
            },
            "options": {
                "colorMode": "value",
                "graphMode": "area",
                "justifyMode": "auto",
                "orientation": "auto",
                "reduceOptions": {"calcs": ["lastNotNull"], "fields": "", "values": False},
                "textMode": "auto",
            },
            "targets": [target(TOTAL_IN_RANGE, "transitions")],
        },
        {
            "id": 3,
            "type": "timeseries",
            "title": "Failed transitions per second",
            "description": "The failure rate of the fleet, in transitions to `failed` per second. "
            "A cancel and an unreadable run state count here too: both commit `failed`.",
            "gridPos": {"h": 8, "w": 12, "x": 0, "y": 8},
            "datasource": DATASOURCE,
            "fieldConfig": {
                "defaults": {
                    "unit": "ops",
                    "min": 0,
                    "color": {"mode": "fixed", "fixedColor": "red"},
                    "custom": {
                        "drawStyle": "line",
                        "fillOpacity": 20,
                        "lineInterpolation": "linear",
                        "lineWidth": 2,
                        "showPoints": "never",
                        "spanNulls": False,
                    },
                },
                "overrides": [],
            },
            "options": {
                "legend": {"calcs": [], "displayMode": "list", "placement": "bottom", "showLegend": True},
                "tooltip": {"mode": "single", "sort": "none"},
            },
            "targets": [target(FAILED_PER_SECOND, "failed")],
        },
        {
            "id": 4,
            "type": "timeseries",
            "title": "Failure share",
            "description": "Failed transitions over all transitions, as a share. The dashed line "
            "is the red threshold; an empty graph means no transition in the window.",
            "gridPos": {"h": 8, "w": 12, "x": 12, "y": 8},
            "datasource": DATASOURCE,
            "fieldConfig": {
                "defaults": {
                    "unit": "percentunit",
                    "min": 0,
                    "max": 1,
                    "custom": {
                        "drawStyle": "line",
                        "fillOpacity": 20,
                        "lineInterpolation": "linear",
                        "lineWidth": 2,
                        "showPoints": "never",
                        "spanNulls": False,
                        "thresholdsStyle": {"mode": "dashed"},
                    },
                    "thresholds": {
                        "mode": "absolute",
                        "steps": [
                            {"color": "green", "value": None},
                            {"color": "red", "value": 0.25},
                        ],
                    },
                },
                "overrides": [],
            },
            "options": {
                "legend": {"calcs": [], "displayMode": "list", "placement": "bottom", "showLegend": True},
                "tooltip": {"mode": "single", "sort": "none"},
            },
            "targets": [target(FAILURE_SHARE, "failed / all")],
        },
        {
            "id": 5,
            "type": "bargauge",
            "title": "Transitions in range, by state",
            "description": "The state mix over the dashboard's time range: what the fleet spent "
            "its transitions on.",
            "gridPos": {"h": 8, "w": 24, "x": 0, "y": 16},
            "datasource": DATASOURCE,
            "fieldConfig": {
                "defaults": {
                    "color": {"mode": "continuous-GrYlRd"},
                    "unit": "short",
                    "min": 0,
                    "thresholds": {"mode": "absolute", "steps": [{"color": "green", "value": None}]},
                },
                "overrides": [],
            },
            "options": {
                "displayMode": "gradient",
                "minVizHeight": 16,
                "minVizWidth": 8,
                "orientation": "horizontal",
                "reduceOptions": {"calcs": ["lastNotNull"], "fields": "", "values": False},
                "showUnfilled": True,
                "valueMode": "color",
            },
            "targets": [target(BY_STATE_IN_RANGE, "{{state}}")],
        },
    ]


def dashboard():
    return {
        "annotations": {"list": []},
        "description": DESCRIPTION,
        "editable": True,
        "graphTooltip": 1,
        "links": [],
        "panels": panels(),
        "refresh": "30s",
        "schemaVersion": 39,
        "tags": ["adam", "otel", "runs"],
        "templating": {
            "list": [
                {
                    "current": {},
                    "hide": 0,
                    "includeAll": False,
                    "label": "Data source",
                    "multi": False,
                    "name": "datasource",
                    "options": [],
                    "query": "prometheus",
                    "refresh": 1,
                    "regex": "",
                    "skipUrlSync": False,
                    "type": "datasource",
                },
                {
                    "allValue": ".*",
                    "current": {"selected": True, "text": ["All"], "value": ["$__all"]},
                    "hide": 0,
                    "includeAll": True,
                    "label": "State",
                    "multi": True,
                    "name": "state",
                    "options": [
                        {"selected": index == 0, "text": state, "value": state}
                        for index, state in enumerate(STATES)
                    ],
                    "query": ",".join(STATES),
                    "refresh": 0,
                    "skipUrlSync": False,
                    "type": "custom",
                },
            ]
        },
        "time": {"from": "now-6h", "to": "now"},
        "timepicker": {},
        "timezone": "browser",
        "title": "adam — runs",
        "uid": "adam-runs",
        "version": 1,
    }


def main():
    root = pathlib.Path(__file__).resolve().parents[2]
    out = root / "deploy" / "dashboards" / "runs.json"
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(dashboard(), indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print(f"wrote {out.relative_to(root)}")


if __name__ == "__main__":
    main()
