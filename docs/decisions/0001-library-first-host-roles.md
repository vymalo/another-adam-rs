# 0001. Library first: host roles in `adam-host`

Status: **Proposed** (2026-09-29). The owner decided the direction. The open questions at the
end are still pending.

## Context

adam-rs started as a framework with one binary, `adam-coder`. Its `serve.rs` runs two halves
by hand: an A2A server and a run worker. It starts both, waits for a signal, and stops the
server first and the worker second.

The owner decided on 2026-09-29:

* adam-rs is a **library** that any host app embeds. The comparison: eve.dev is a library,
  NestJS is the app.
* A host runs a **control plane** and **workers**, decoupled: one control plane and N workers,
  or more.
* The roles `control-plane`, `worker` and `all` are an enum **in adam-rs**. It is the standard
  for every host. Another host, such as the `another-agentic-system` orchestrator, reuses it
  and only says which role a process runs.

The orchestrator already stops its two halves by hand in `orchestrator/bin/orchestrator/src/boot.rs`.
The two hand-written versions are alike: the first half to end takes the process down, the
front stops first, the worker stops second, and a bound decides when to give up (the
orchestrator bounds both halves, `adam-coder` only the front). *Verified 2026-09-29: read
`crates/adam-coder/src/serve.rs` and `boot.rs`.*

The name "control plane" also names the Next.js chat UI in `another-agentic-system`. That
clash is outside this repo. It is tracked there.

## Decision

1. **adam-rs is library-first.** A binary is a host that uses the library. Anything a host
   needs from every process goes into a library crate, not into one binary.
2. **`Role` lives in a new crate, `adam-host`.** The crate has std, `adam-error` and
   `thiserror` by default, and it has no HTTP framework. It never reads the environment.
3. **`Role` is closed.** `enum Role { All (default), ControlPlane, Worker }`, with no
   `#[non_exhaustive]`. A new role must be a compile error in every host that matches on it
   (the same idea as ADR 0004 in `another-agentic-system`: closed enums for protocols).
   `runs_control_plane()` and `runs_workers()` let a host avoid matching at all.
4. **The host supplies the variable name.** `adam-host` gives `Role::from_optional(Option<&str>)`
   and, with feature `clap`, `ValueEnum`. `adam-coder` will read `ROLE`. The orchestrator
   can read `ORCH_ROLE`. `None` or blank means `all`, which is today's behaviour.
5. **A small supervisor comes with it** (feature `supervisor`, on by default).
   `Host::new(role).control_plane(name, f).worker(name, f).run(shutdown)`. It runs only the
   components that match the role. It stops the control plane first (bounded by a drain
   time), then the workers (bounded by a grace time), and it returns the first failure by
   component name. It exposes `Health` as data only.
6. **What each role runs in `adam-coder`** (a later change, not part of this one):

   | Role | Runs |
   |---|---|
   | `control-plane` | the A2A front (`A2aServer` over `RuntimeTaskBackend`), and the runtime **without** `run_worker` |
   | `worker` | `run_worker`, and a health endpoint |
   | `all` | both (today) |

   The front then needs no model or GitHub secrets. The worker needs them. Config is checked
   per role.
7. **The seam stays the store.** The two roles talk only through the Postgres store: the run
   record with its version compare-and-swap, and leases. No new protocol between them.

## Consequences

* A host is a short composition: read the role, register components, call `run`. The stop
  order stops being copied into each binary.
* The orchestrator and `adam-coder` can share one vocabulary, and one deployment can scale
  the two halves apart.
* `Role` is a public, closed enum. Adding a role is a breaking change for hosts that match on
  it. That is the point, and it costs a major version.
* `adam-coder` is not refactored here. Until it is, it keeps its own hand-written stop logic.
* When the halves are separate processes, some things degrade until they are built: live
  events only reach subscribers in the same process, the worker wakes by polling instead of a
  wake signal, and a cancel reaches the worker by polling. Each has a planned fix (Postgres
  `NOTIFY`), not part of this change.
* The supervisor pulls `tokio`, `tokio-util` and `tracing` into `adam-host`. A host that
  only wants `Role` turns the default feature off.

## Alternatives considered

* **`Role` in `adam-runtime`.** The runtime is where workers live, so it looks natural. But
  a host with no runtime, such as the orchestrator today, would depend on the whole runtime
  for one enum. Rejected.
* **`Role` in `adam-core`.** The crate is about the store and run types. A role is about a
  process, not about state. Rejected: wrong layer, and it would pull the host's concern into
  every crate that uses the store.
* **Separate binaries** (`adam-coder-front`, `adam-coder-worker`). No enum, no supervisor.
  But every host repeats the split, the config, and the stop logic, and "all" (one process
  for development) needs a third binary. Rejected.
* **`#[non_exhaustive]` on `Role`.** Hosts would keep compiling when a role is added, and
  would silently treat it as a default. That is the failure this decision wants to
  prevent. Rejected for `Role`. `HostError` and the other error enums stay
  `#[non_exhaustive]`, following the repo rules, because a new error only needs a class.
* **A `runtime` feature in `adam-host` now.** A ready-made worker component over
  `adam-runtime`. Useful, but not needed to fix the contract. Left as future work.

## Open questions

* **Local, in-process agents in hosts.** May the orchestrator run an adam agent in its own
  process, behind its agent port? Options: later and feature-gated (only agents with no
  sandbox), never (A2A only), or yes including tools. The coder, workspace and ACP crates
  would not be linked in.
* **Cross-process events.** Live events and worker wake-up between a control plane and a
  worker in other processes. Proposed: an event sink and listener over Postgres `NOTIFY`
  (payload limit 8000 bytes, *unverified*: from memory), with polling as the fallback.
* **Coder scale-out with worktrees on per-replica volumes.** A run needs the git worktree it
  started with. N workers with a volume each means a run must return to the same worker.
  Options: N fronts and one worker, a shared read-write-many volume, or lease affinity.
