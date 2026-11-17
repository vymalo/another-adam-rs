# 0048. A benchmark harness for adam-coder and OpenCode

Status: **Accepted** (2026-11-17), an owner decision: the harness, the task mix, one model in two arms, the cap, the
pilot, resume, and that the owner runs it. Slice 1 (the meter, cost and cap, the ledger, the configuration, the
statistics and the report) is built; the task sources, the agent drivers and the runs are not. The five defaults at
the end are the owner's to confirm.

## Context

The owner wants to know how `adam-coder` compares with OpenCode on real coding tasks, on one model, with one
reasoning setting on and one off, and what each costs. No harness exists here, and a comparison is only worth
reading if the two agents get the same tasks, the same environment and the same meter, and if the money spent has a
hard ceiling. The owner's endpoint, model, prices and key are private and must never enter the repository.

## Decision

1. **A harness in `bench/`**, Node ESM with no dependencies, outside the Rust workspace (not under `crates/` or
   `bin/`), changing no Rust code and nothing under `dev/`. It reuses what `dev/` already has (the git server, the
   Podman service, the GitHub mock) in later slices. Its CI is `.github/workflows/bench.yml`, on changes under
   `bench/`. How it works: [`bench/README.md`](../../bench/README.md).
2. **Tasks.** SWE-bench Pro V2, 80 tasks, seeded and stratified by repository, pinned by upstream commit and
   checksum; at most six of our own, mined from the public repositories and written up by the owner, reported apart.
   No task content is copied into this repository: the lock file holds ids, checksums and image digests only.
3. **One model, two arms.** An arm is a JSON fragment the owner writes in the local configuration and the meter
   merges into every request. No paid probe: the pilot shows whether the "off" arm cuts reasoning tokens.
4. **One meter.** Every model call of both agents goes through one proxy that injects the key, rewrites the model,
   merges the arm, clamps the output limit and counts tokens and cost. It is the truth for the cost and the cap. The
   agents' own counts are shown beside it and never used for the cap.
5. **A hard cap, as reservations.** A run reserves its budget before it starts and is never started if the
   reservation would cross the cap; a call whose worst case would cross its run's reservation is refused with 429.
   Money is integer micro-dollars. A call with no usable `usage` is charged its worst case and flagged. The cap is
   soft by one call per running slot: a call that reports more usage than its worst case takes its run, and the
   total, past the reservation by that call's excess, and the next call is refused. Every call also has a size and
   a time limit, one completion (`n` = 1), and the agent never sees an upstream error body; the numbers are in
   [`bench/README.md`](../../bench/README.md).
6. **A ledger and resume.** An append-only record of every state change of every run and every call. A finished run
   is never repeated; an interrupted one starts again and its spend stays as waste, but at most twice: the third
   interruption ends the run `Failed_infra` for good; a run being graded is graded again and never run again. The
   ledger has one writer, held by a lock file that is linked into place so it never exists without its pid.
7. **Grading** is by patch replay in a pristine image, three times, two of three to pass (slice 2).
8. **A pilot first** (slice 4): 20 runs set the per-run budget; the full run starts only after the owner confirms the
   projection.
9. **The owner runs it**, on the owner's machine and endpoint. Private values are in a file outside the repository,
   which the harness refuses to read from inside any work tree, and a key is only ever read from the environment.
   The report shows the model's label, never its id.

The report says what it does not claim: no general ranking, no comparison with public leaderboards, no significance
where the intervals overlap, no real billing, no per-language conclusion below 15 tasks.

| Slice | What | State |
|---|---|---|
| 1 | meter, cost and cap, ledger, configuration, statistics, report, this ADR, CI | built |
| 2 | task sources, selection, the environment check, grading | not built |
| 3 | agent drivers, the compose stack, the dry run, `doctor` | not built |
| 4 | `pilot`, `run`, blocks and slots, the projection | not built |

## For the owner to confirm

Defaults, each changeable in the local configuration:

1. Two slots (two Podman services of 4 CPU and 12 GB each).
2. 3 USD per run before the pilot; after it, the pilot's 95th percentile times 1.5 for each arm, printed as a
   projection that the owner confirms before the full run.
3. The two arms are JSON fragments the owner writes; there is no paid probe.
4. At most six own tasks from adam-rs, reported apart from the rest.
5. If nested Podman fails under rootless Docker: host rootless Podman first, k3d run pods second; `doctor` says
   which.

## Consequences

* A comparison that holds the environment, the model and the money fixed, and says plainly what it cannot show.
* A harness that costs nothing to test: the tests use a scripted OpenAI-compatible server and spend no tokens.
* The meter's worst case is an estimate (one token per three bytes of input, the clamped output); an endpoint that
  reports more than it makes the global stop, not the reservation, hold the cap, so the cap is soft by that one
  call's excess per running slot.

## Verified and unverified

* *Verified 2026-11-17* (`scaleapi/SWE-bench_Pro-os`, `README.md` and `v2/README.md` on `main`): V2 is 642 validated
  tasks in 11 repositories as Harbor task directories, with public images
  `ghcr.io/scaleapi/swe-bench_pro-v2:<instance_id>` (linux/amd64, anonymous pull) and a locked protocol: no network
  for the agent, grading by patch replay in a pristine image. The harness is MIT; the task content is under each
  upstream repository's licence.
* *Verified 2026-11-17* (OpenCode `packages/opencode/src/cli/cmd/run.ts` at v1.19.6): `opencode run` has
  `--format json`, `--model`, `--auto`, `--variant`, `--dir` and `--title`; `--auto` and `--variant` also exist in
  v1.18.33.
* *Unverified*: the size of the task images; whether `--title` stops OpenCode's extra title call; whether an endpoint
  honours `stream_options.include_usage` and reports cached and reasoning tokens as
  [ADR 0032](0032-usage-per-model-call.md) assumes; whether nested Podman works under rootless Docker.
