# 0001. Library first: host roles in `adam-host`

Status: **Accepted** (2026-09-29). The owner answered the open questions the same day. Two
questions stay open: cross-process events, and the exact workspace placement enum.

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

The name "control plane" also names the Next.js chat UI in `another-agentic-system`. The owner
decided that the UI is renamed "web chat surface" there, and that "control plane" means the
role. The rename is outside this repo.

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

   *Amended 2026-09-29 (implemented in `adam-coder`):* config is checked per role, but the
   control plane still needs the model and GitHub settings. `Runtime::start` looks the agent up
   by name and calls its `init`, so the control plane registers the complete `CoderAgent`
   (its model client, workspaces and GitHub client are built, never called, and the workspace
   root is not created). Only `A2A_BEARER_TOKENS` and `PUBLIC_URL` are role-specific, and the
   worker does not need them. Removing the requirement needs agent starters (a control plane
   that can start a run without holding the agent), which is a later change. *Verified
   2026-09-29: read `Runtime::start` and `new_run` in `crates/adam-runtime/src/runtime.rs`.*
7. **The seam stays the store.** The two roles talk only through the Postgres store: the run
   record with its version compare-and-swap, and leases. No new protocol between them.
8. **Hosts may run adam agents in-process, tools and sandboxes included.** The owner decided
   this for the `another-agentic-system` orchestrator: a worker there may host an adam agent
   in its own process, behind its own agent port and behind a Cargo feature of its own. That
   includes agents that run tools and sandboxes. It is the host's decision and the host's
   risk. For adam-rs it means the agent, runtime and store crates stay usable as libraries in
   another process. The host, not adam-rs, adds the adapter. Plain A2A stays the way to reach
   an agent in another process.
9. **Hosts consume adam-rs by git rev.** The repository is public. A host pins a full commit
   sha and bumps it by pull request. adam-rs does not publish to crates.io for this. It keeps
   the workspace `repository` field and the pinned dependency versions correct, so a host can
   build from the sha alone.
10. **Workspace placement is chosen by the deployer** (decided in principle). An agent with a
    filesystem, such as the coder, needs a workspace per run. The owner said every strategy
    must be possible: "A worker can do the work, it can own a folder, it can have an isolated
    pvc, it can use only a2a." adam-rs will offer a closed, deployer-selected placement policy.
    Candidate names:

    | Policy | Meaning |
    |---|---|
    | `Shared` | an RWX volume; any worker may resume any run |
    | `Affinity` | a run is leased only by the worker that owns its folder |
    | `Isolated` | a PVC per worker; implies affinity |
    | none | A2A-only deployments need no workspace |

    The exact enum and the mechanism are follow-up design work. **The runtime has no
    run-to-worker affinity today:** any worker may lease any run. Until affinity exists, a coder
    deployment runs one worker, or uses a shared volume.

## Consequences

* A host is a short composition: read the role, register components, call `run`. The stop
  order stops being copied into each binary.
* The orchestrator and `adam-coder` can share one vocabulary, and one deployment can scale
  the two halves apart.
* `Role` is a public, closed enum. Adding a role is a breaking change for hosts that match on
  it. That is the point, and it costs a major version.
* `adam-coder` is not refactored here. Until it is, it keeps its own hand-written stop logic.
  *Amended 2026-09-29: `adam-coder` now reads `ROLE` and runs its components through `Host`;
  its hand-written stop logic and `StoppedUnexpectedly` are gone, and a component that ends
  early is a `HostError` (exit code 70).*
* Hosting agents in-process (decision 8) is only safe as far as the host limits it. An agent that
  runs builds can starve the process it lives in. The host must give it its own pods and
  limits. adam-rs does not sandbox the host.
* Placement (decision 10) will add a second closed enum that deployments depend on.
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

## Resolved questions

* **Local, in-process agents in hosts.** Resolved 2026-09-29: yes, including agents that run
  tools and sandboxes, behind the host's agent port and a Cargo feature (decision 8).
* **Where `Role` lives, and whether it is closed.** Resolved: `adam-host`, closed (decisions 2
  and 3).
* **How hosts depend on adam-rs.** Resolved: git rev, public repository (decision 9).

## Open questions

* **Cross-process events.** Live events and worker wake-up between a control plane and a
  worker in other processes. Proposed: an event sink and listener over Postgres `NOTIFY`
  (payload limit 8000 bytes, *unverified*: from memory), with polling as the fallback.
* **The exact placement enum and mechanism** (decision 10). Are `Shared`, `Affinity` and
  `Isolated` the right names and the right set? How does a worker own a folder and claim only
  its runs (a lease with a worker id, or a routing key)? What happens to a run when its worker
  is gone for good under `Affinity` or `Isolated`? Needs its own design and ADR.
