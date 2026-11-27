# ADR 0071 — A remote sandbox is one more `Environment`, and the seam is the port plus a conformance kit

- **Status:** **Accepted (2026-11-24)** for the seam and the conformance kit, on the owner's answer to
  ANR-Q63: *(a) "Write the ADR for the port seam and a conformance test kit only; adopt no outside service now;
  drop the Docker host (ADR 0052 stands)"*. **Adopting a service (E2B, Daytona or any other) is not decided here**
  and stays the owner's, with its own card. Recorded in the owner's private decision log (2026-11-24).
- **It closes gap 3 of ANR-12** ("Pluggable sandboxes: E2B, Daytona and a Docker host"). It does **not** amend
  [ADR 0052](0052-no-podman-a-repositorys-own-environment-is-a-run-pod-of-its-image.md): that decision stands.

## Context

A run's environment is where the agent's commands execute: a local directory, or a pod of the agent's image
(`KubeEnvironment`). The gap asks whether a *remote* sandbox — a service that runs commands somewhere else, with its own
account, terms and spend — can be a third place to run.

Two things were missing, and neither is a vendor choice. **The seam was not written down**: it exists as a trait, and it
is worth saying explicitly that a remote sandbox is *that* trait and not a second one. And **there is no conformance
kit**, so nobody could say what a new implementation must do before it is trusted with a run. Both are ours to settle
without adopting anything.

The Docker host the gap also names is refused by [ADR 0052](0052-no-podman-a-repositorys-own-environment-is-a-run-pod-of-its-image.md)
(a repository's own environment is a run pod of its image, never Podman), and the owner's answer keeps it refused: **no
Docker host is designed here.**

## Decision

1. **One port, not two.** A remote sandbox implements [`Environment`](../../crates/adam-workspace/src/environment.rs),
   the trait the run loop already drives: `ensure(run, workspace)` → `Arc<dyn EnvSession>`, `release(run)` (idempotent),
   `held_runs()` for the janitor's sweep of what a crash left, and the session's `describe() -> EnvDescription`. The
   precedent is on `main`: `Local` and `KubeEnvironment` are two implementations, and `HidingEnvironment` and the
   metered one are decorators of any implementation. A remote sandbox is a **third implementation in its own crate**,
   not a change to the port, not a flag inside `KubeEnvironment`, and no new trait.
2. **What the port already promises, and an adapter must keep.** `ensure` is **single-flight per run** (a second call
   for one run makes nothing twice; a cancelled caller leaves nothing half-made), `release` is idempotent, and
   `held_runs` answers for the sweep. A path inside the workspace means the same in every environment
   (`ExecSpec`'s contract), so the agent's commands, their working directories and the files they write do not change
   when the environment moves.
3. **How the workspace goes in and comes back.** The run's workspace is the unit: the mirror and the run's worktrees go
   **in** when `ensure` materialises the environment, and what comes **back** is what the pipeline already carries out of
   any environment — the agent's reported branch and files, as the run's artifacts, under the rules that path already
   has. **No new transport is introduced by this decision**: an adapter that has to copy a whole volume back is
   describing a problem the pipeline does not have, and it may refuse a run it cannot serve.
4. **Credentials never come through the run.** `ExecSpec`'s environment says *never a secret* in as many words, and that
   stays: the adapter's own credential (the vendor's API token) is the **deployment's**, from the deployment's secret
   source, read where every other deployment secret is read. The run's own credentials are unchanged: they reach the
   agent through the lease it already has, and nothing about a remote environment widens them.
5. **The network rule is stated, not assumed.** A remote sandbox is reached only over its API, so a run there has
   **no lane of ours**: the person's network tier cannot be enforced by us inside a vendor's network. An adapter must
   therefore **declare what it can and cannot enforce** in `EnvDescription` (already the session's own account of
   itself), and a deployment that enables one must say that a tier it cannot enforce is not claimed. Claiming `offline`
   for a run in a vendor's network would be the one dishonest answer available here, and it is refused.
6. **The conformance kit is the gate.** A new crate, `adam-env-testkit`, exports a conformance macro in the shape the
   repository already uses (`adam-confine-testkit`, `adam-admission-testkit`): a suite every `Environment` implementation
   must pass, and a **fake adapter** that passes it. Its cases are the port's promises: single-flight `ensure` (two
   concurrent calls, one environment), a cancelled `ensure` leaving nothing that a later call cannot use, idempotent
   `release`, `held_runs` telling the truth across a release, `describe` matching what the adapter enforces, `exec` with
   `cwd`/`env`/`hide` and the path contract, a background process that outlives its call, `kill`, and each `EnvError`
   class reached by the fake's failure injection. At least one case must be shown **red** by breaking the fake (a
   removed single-flight guard is the sharpest), because a kit nobody has seen fail proves nothing.
7. **No Docker host, and no vendor adapter here.** ADR 0052 stands; whether a service is adopted later is a separate
   decision with its own card, its own facts and its own spend.

## Vendor facts

E2B and Daytona are named **only as examples of the class**, and this ADR asserts nothing about them: what they offer,
their limits, their terms and their spend are **unverified** (no vendor documentation was read for this decision, and
none is needed for a seam). Whoever proposes adopting one verifies each fact it relies on, with a source and a date,
and puts it on that card.

## Slice

| Slice | What it delivers | Decisive test |
|---|---|---|
| S1 | `adam-env-testkit`: the conformance macro, its cases, and a fake `Environment` that passes it | the suite runs; the single-flight case seen **red** by removing the fake's guard, then green |

A later slice per vendor — one row each, never one row for "the vendors" — would name that vendor's decisive test on a
fake **of that vendor's service**. None is planned here.

## Consequences

- **The seam costs nothing today and is written down for free.** A future adapter has a port, a kit and the rules above;
  it does not get to invent a second seam or a new way to move a workspace.
- **The honest cost is the network.** A remote environment is the one place where the person's tier cannot be enforced,
  so the adapter has to say so and the deployment has to repeat it. That is worse than a pod, and it is the reason a
  remote sandbox is an *option* and not a default.
- **The kit becomes a gate for every implementation, including `Local` and `KubeEnvironment`.** They should pass it too:
  the trait's promises were documented before they had a suite, and the suite is where that stops being a hope.

## Alternatives rejected

- **A second port for remote sandboxes.** It would duplicate `ensure`/`release`/`held_runs`, and everything that drives
  an environment today (the run loop, the janitor, the decorators) would need a second path.
- **A `docker` backend or a Docker host.** Refused by ADR 0052 and by the owner: a repository's own environment is a run
  pod of its image.
- **Adopting a vendor in this ADR.** The owner's answer is explicit: the seam and the kit now, the vendor later.
