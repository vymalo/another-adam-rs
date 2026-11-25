# 0075. Metrics and traces are off by default and say nothing about a person

Status: **Proposed** (2026-11-24, task ANR-268; sys#407, ROADMAP M11, second gap; sys#405, M9, fifth gap). Nothing is
built. The number is the next free one at the time of writing (0074 is on an open branch); take the next free at merge.

## Context

*Verified 2026-11-24 at adam-rs `main` (`91a052d`):*

- **No metrics endpoint.** `crates/adam-service/src/metrics.rs` renders the admission pools as Prometheus text
  (`adam_admission_in_use`, `adam_admission_limit`, `adam_admission_waiting`, the token bucket), gauges only, and
  `adam-service` "runs no metrics listener of its own" (module doc, lines 1 to 12). No composition serves it
  (`docs/reference/parity.md` gap 27).
- **No OpenTelemetry exporter.** No `opentelemetry`, OTLP or `traceparent` appears in any `*.rs` or `Cargo.toml`; the
  workspace has `tracing` 0.1 and `tracing-subscriber` with `fmt`, `json` and `env-filter` (`Cargo.toml:137`).
- What does exist: logs, and the usage the agent reports over `usage/v1` (`docs/reference/a2a-server.md:655`, ADR 0032).

On the platform side the operator and the Platform API have traces (plat K5, `crates/trace`, `crates/trace-otlp`, *verified
2026-11-24* in its READMEs): a **closed span API** (an attribute is a variant of one enum, so no code can set a key outside
the list; a text value is a closed enum or a checked label), a **bounded pipeline** whose `end` is a `try_send` on a queue
(a full queue drops and counts, never blocks), OTLP over HTTP JSON by hand with no SDK, **off unless an endpoint is set**,
`OTEL_*` as a strict subset, and `aap_trace_*` counters on `/metrics`. The system's side is on the board and not decided
here: capacity metrics (sys#292, ANR-100) and the trace context across the outbox (sys#170, ANR-61, a spike, open question
27 of the system's `docs/open-questions.md`). **ANR-61 has not decided yet.**

## Decision

1. **Off by default.** No exporter, no listener and no task exists until configured. Metrics: `METRICS_LISTEN=<addr:port>`
   (unset: none). Traces: `OTEL_EXPORTER_OTLP_ENDPOINT` (unset: none). A bad value (an unparsable address, a protocol
   other than `http/json`, headers on a plain `http` endpoint, an unknown `OTEL_*` key we claim to read) is **exit 78**.
   A tracing variable that a build tool injects into a build step must never stop a running agent: at run time a refused
   tracing setting warns once and runs with tracing off, as plat K5 does; `--check` refuses it.
2. **Metrics.** Prometheus text on the listener. The names, all `adam_`, and their labels, from fixed closed sets:

   | Metric | Type | Labels |
   |---|---|---|
   | `adam_runs` | gauge | `state` (the closed `RunStatus` set) |
   | `adam_leases_held` | gauge | none |
   | `adam_queue_oldest_age_seconds` | gauge | none (the age of the oldest due, unclaimed run) |
   | `adam_step_duration_seconds` | histogram | `kind` (`model`, `tool`, `child`), `outcome` (`ok`, `error`, `retry`) |
   | `adam_model_calls_total`, `adam_tool_calls_total` | counter | `outcome`; tools by `class` (`builtin`, `mcp`, `command`), **never a tool's name** |
   | `adam_admission_*` | gauge | `pool` (today's series, unchanged) |

   **A label is a variant of an enum in the code, never a string from a run.** A person, a thread, a conversation, a run
   id, a token, a model alias chosen by a user, a tool name from an MCP server and a secret are never labels, and the test
   below proves it.
3. **Spans.** One trace per run round: a `run` root; a child per step (`step`), per model call (`model_call`), per tool call
   (`tool_call`), and a `child_run` for a child session (ADR 0045; the child's root is a child of the parent's span). Attributes
   come from a closed set, as in plat K5: `adam.run.state`, `adam.step.kind`, `adam.outcome`, `adam.tool.class`,
   `adam.model.alias_class`, token counts as integers, `error.type` as a class (`adam_error::Classify`). **Never** in a span:
   a prompt, a completion, a tool argument or result, a path, a URL, a person, a conversation id, a run id (the trace id is
   the correlation), an instruction, an environment value, a token. No span events, no links except the replay's (below).
4. **Trace context.** adam reads a W3C `traceparent` (and `tracestate`, ignored) from the A2A request's metadata and makes the
   run's root a child of it; it writes the `traceparent` of the current span on every A2A call it makes (a remote agent, a child
   session). This is the context ANR-61 will decide for the orchestrator side; **until ANR-61 decides, adam accepts and
   forwards it and starts its own trace when none comes**, which is compatible with any outcome (an orchestrator that sends
   none loses only the join). It is a card extension read live and fail closed (ADR 0008), `trace-context/v1`, optional.
   An inbound context is never trusted for sampling beyond its `sampled` flag, and the root's own ratio sampler decides first.
5. **Replay and duplicate spans.** A run is durable, so a round that wakes again must not repeat a span for work already
   exported. A span is **ended once, at the commit that records its step**: the exporter is fed from the journal commit, not
   from the code path. A wake that replays journalled steps (resume after a lost lease, or a future replay-from-step) emits
   **no span for a journalled step**; it emits one `resume` child with `adam.replayed_steps` as a number, and a replay that
   forks a run links the new root to the old trace with one link. A step re-executed because its journal entry was not
   committed is a new attempt and says `adam.attempt`.
6. **Pipeline.** As plat K5: a bounded queue (2048 spans), `end` never blocks, a full queue drops and counts
   (`adam_trace_spans_dropped_total`), a failing exporter logs once per episode, shutdown flushes for a deadline. OTLP over
   HTTP JSON by hand on the workspace's `reqwest`, no SDK and no new third-party package; headers from a file, a CA bundle that is
   the only trust, no proxy, no redirect. A shared crate with the platform is **not** assumed: the conventions are shared, the
   code is adam's own until both sides want one (open question 1).
7. **Chart values** (`deploy/coder`): `metrics.enabled` (port, a `ServiceMonitor` off by default, a NetworkPolicy ingress only
   from `metrics.allowFrom`, which a `ServiceMonitor` requires), `otel.endpoint`, `otel.sampler`, `otel.headersSecret`,
   `otel.caConfigMap`, `otel.networkPolicy.to` (egress only to the named collector). All off by default and **then not a trace in
   any render**: the existing goldens do not move. Parity matrix rows for both surfaces (`docs/reference/parity.md`).

## Slices

One row per PR.

| # | Slice | Decisive test |
|---|---|---|
| M1 | `adam-service` serves `/metrics` on `METRICS_LISTEN`; the existing pool gauges move there | unset means no socket is bound; a bad address is exit 78; the series names are unchanged |
| M2 | `adam_runs`, `adam_leases_held`, `adam_queue_oldest_age_seconds` from the store | against the testkit store, the gauges equal `count_runs` per state; a run of person A and of person B give one series, no label names either |
| M3 | Step, model-call and tool-call counters and histogram | a planted secret, a person's e-mail and an MCP tool name are in no label and no line of the output (a scan of the whole body) |
| T1 | `adam-trace`: the closed span API, the bounded pipeline, the `SpanExporter` seam and its testkit | a key outside the list does not compile; a full queue drops and counts without blocking (a hanging exporter) |
| T2 | `adam-trace-otlp`: OTLP/JSON exporter and the `OTEL_*` subset | `grpc`, `http/protobuf`, headers on `http` and a bad ratio are exit 78 at `--check` and a warning at run time |
| T3 | Spans on the run, steps, model and tool calls, child runs; `traceparent` in and out | a test collector receives one trace per round with the parent link from the inbound `traceparent`; no prompt, argument, path or id in any attribute (`assert_clean`) |
| T4 | Resume emits no span for a journalled step | a run killed and resumed three times exports each step's span exactly once |
| D1 | Chart values and goldens, `render-check`, a kind scenario with a collector | with the values off the goldens are byte for byte unchanged; with them on the policy admits only the collector |

## Open questions

1. A crate shared with the platform's `aap-trace`, or two crates with one convention? Recommended: two, until a third user.
2. Is a person's id ever acceptable as a trace attribute for debugging? Recommended: no, here or in the orchestrator; the trace
   id and the run id in logs are the join.

## Consequences

adam gains an observability surface that cannot name a person by construction (closed enums), at the cost of a second tracing
crate to keep aligned with the platform's. The `traceparent` forwarding is harmless before ANR-61 decides and is the only part that
needs both repositories to agree.

## Alternatives

- **The `opentelemetry` SDK and `tracing-opentelemetry`.** A large dependency tree, and a `tracing` bridge would let any `info!`
  field become an attribute, which is the leak the closed API prevents.
- **Labels from run data (agent, model alias, tool name).** One user's string becomes a time series; unbounded and, for a tool
  name, a way to put content in a label.
