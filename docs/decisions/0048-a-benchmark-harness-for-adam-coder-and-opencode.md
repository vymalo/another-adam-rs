# 0048. A benchmark harness for adam-coder and OpenCode

Status: **Accepted** (2026-11-17), an owner decision: the harness, the task mix, one model in two arms, the cap, the
pilot, resume, and that the owner runs it. Slice 1 (the meter, cost and cap, the ledger, the configuration, the
statistics and the report) and slice 2 (task sources, selection, the environment check, grading) are built; the agent
drivers and the runs are not. The five defaults at
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
| 2 | task sources, selection, the environment check, grading | built (2026-11-17) |
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

## Amended 2026-11-17: slice 2 is built

The task sources, the statement lint, selection, the environment check and grading by patch replay are in
`bench/lib/*.mjs` (`bench/README.md`, "Tasks" and "Grading"). Decisions the slice took, none changing the above:

* One task format for both sources, the Harbor layout; the validator refuses a task with a part missing, an unreadable
  `task.toml`, an image not pinned by digest and an agent network other than none. Upstream names images by tag, so a
  digest is resolved by `bench tasks check` and kept in `$BENCH_HOME`, never in the repository.
* The upstream commit is the owner's, in the local configuration (`tasks.upstreamCommit`); there is no default.
* An own task is two files in the repository (the statement and `own.json`: commit ids, test command, toolchain pins).
  Its task directory, its hidden tests (read from the owner's clone) and its image are built under `$BENCH_HOME`.
* The environment check runs from the clone path the agents will use, with the top-level ignored entries linked to
  `/app`'s and `/app` rewritten in `test.sh`; it also records a `.devcontainer` at the base, and selection drops those tasks.
* Grading runs through a `Containers` interface; `PodmanRemote` builds `podman --remote` arguments with a pure function.
  Mount sources are paths as the Podman service sees them, so the service must share the harness's work directory.

*Unverified*: everything that needs a container service (the `podman --remote` options of the version in use, bind
mounts of harness paths, digests of pulled and locally built images, running a locally built image by digest, the
clone-path check on real images), where `SHA256SUMS` and the task directories sit under `v2/`, the form of `solution/`
in upstream tasks, and the image sizes. The Docker-gated test (`bench/test/docker.test.mjs`) has not been run.

### Amended 2026-11-17: after a blind review of slice 2

Grading runs the agent's code, so no grader can be made tamper-proof against code that runs inside it. The harness
therefore makes tampering hard and visible, and says so in the report:

* **Nothing but files from the agent's tree.** The patch is made in a clone the harness owns; the agent's `.git`
  (configuration, filters, hooks, attributes) is never read. The base's `.gitignore` decides what is left out.
* **Stripping and the suspect list.** Changes to the tests and to the machinery that runs them are removed from the patch
  and recorded; changes that configure tests, a reward that came with a failed exit status, and a patch that mentions the
  grading's files are graded as they are and listed in the report as **suspect**, with the patch path, for a person to read.
* **Every signal must agree.** A pass is exit status 0, a reward of exactly `1` written during the attempt, in a
  directory of that attempt, and no error.
* **An invalid grade is not a fail.** An attempt with an infrastructure error is tried twice more; still one, the grade is
  `invalid`, left out of the rates and listed.
* **The verifier's network is an allow-list** (`none`, `default`, `public` or a name the owner lists; never `host`).
* The upstream tree is verified whole (every file listed, no symlink, normal paths), a check result is tied to the task's
  checksum and image digest, the environment check also runs the gold through the real `/app` path, and the miner reads
  `cfg` predicates, more test paths, exact lockfile names and binary source files.

*Unverified* (added): that the stripping list covers the test machinery of every language in the task set, and everything
the container service must still prove (see above).

## Amended 2026-11-17: the owner chose OpenCode V2 (`@opencode/cli`) for the benchmark

Amended 2026-11-17: the owner chose OpenCode V2 (`@opencode/cli`) for the benchmark.

The benchmark compares adam-coder with OpenCode V2 (`@opencode/cli`, 2.x), not the 1.19.x line above. The pin is the
platform package `@opencode/cli-linux-x64` 2.3.0 (published 2026-11-13), checked against its npm integrity before it is
mounted read-only. Every fact about OpenCode in "Verified and unverified" above was about 1.19.x and is superseded by the
table "OpenCode V2" in `bench/README.md`, which was checked against the 2.3.0 binary and source on 2026-11-17. Where 2.x
differs the harness follows 2.x: `run` has no `--dir` and takes `--standalone`, the export is `opencode session export`,
the tools are `shell`, `subagent` and `path`-taking `write`, and `question` is denied. V2 runs subagents in the background
and `run` waits for them; the harness leaves them on and bounds the run by its time limit.

## Amended 2026-11-17: slice 3 is built

The stack, the two agent drivers, the runner, `bench doctor` and the dry run are in `bench/` (`bench/README.md`: "The
stack", "The agents", "The runner", "`bench doctor`", "The dry run"). Decisions the slice took, none changing the above:

* **One pin file.** `bench/pins.json` holds the coder image (tag, digest, revision) and OpenCode (package, version,
  integrity); `bench/compose.yaml` names the same coder image and a test holds them equal.
* **adam runs on settings that exist** (`ALLOW_LOCAL_REPOS`, the devcontainer settings, a WireMock mock of GitHub) and on
  the agent folder of the image's revision **without `mcp.json`**. Polling `GetTask` decides; a first `input-required` gets one
  fixed reply; the second, the time limit and a meter refusal cancel the task.
* **Two internal networks for each slot, relays on the only network with the internet.** A Podman service is on a network whose
  only other member is its slot's meter relay, so a task's commands reach the meter and nothing else; a coder is on its slot's
  network and reaches neither a Podman service nor the other slot; only five relays, each forwarding to one fixed address, are on
  `egress`. The harness reaches each Podman client through `docker compose exec`: no socket is published. Grading runs on a
  `grader` service that no agent can reach. A test over the rendered compose file holds every member list.
* **The base of a task** is its repository plus one commit that links the ignored top-level entries to the image's;
  both agents clone it, and the links are left out of the patch.
* **Refusals end a run.** A call the meter refuses ends the run `budget_hit` (or `stopped_by_cap`); for OpenCode, which
  retries a refused call, the harness stops the container.
* **The runner is sequential with one slot.** Blocks, slots at once and the pilot are the next pull request's.
* **The dry run** drives both agents through three self-test tasks on a scripted model and asserts, from the ledger, the
  grades 1, 1, 0, the cost, a run over budget and a kill with a resume. It is started by hand (`workflow_dispatch`).

*Unverified*: everything that needs a container service, and none was available where the slice was written (the compose
stack, nested Podman under rootless Docker, `docker compose exec` as the Podman client, the relay through the host gateway,
the coder with no `mcp.json`, OpenCode V2 in a task image, the dry run itself). The list is in `bench/README.md`, "Unverified".

## Amended 2026-11-17: after a blind review of slice 3

The host trusts no place a container could write. The review found that host git ran in a base repository a container had
written (a planted `url.insteadOf` and `core.sshCommand` would have run on the host with the model's key in its environment), and
several places where the harness read or deleted files a container made. The decisions:

* **One principle:** the host never runs git in, reads a file from, or deletes a file in a place a container could write. What a
  container wrote comes out as a tar stream parsed with limits (regular files and directories, links only in a work tree and never
  followed, names refused when they leave the target, caps); what a container left is removed by a container and checked.
  Host git runs in an allow-listed environment (no key) and `ls-remote` runs in an empty directory that is not a repository.
* **adam's git runs in a git sandbox** (the same coder image), the coders are on no network with internet, and every run has a
  repository of its own on the git server, so no run sees another's branches. The base is pushed by a one-shot container.
* **Narrow mounts and a grader.** An agent's service mounts its slot's volume and one read-only directory, never `$BENCH_HOME`;
  grading and the environment check run on a separate `grader` service.
* **The run proves it used the task image** (a container of its digest ran, else `Failed_infra`), **the slot is reaped before and
  after** every run, **the agent is stopped and that is confirmed before its tree is read**, and **the meter's record tells the
  infrastructure from the agent** (`lib/classify.mjs`).
* **Fairness:** one step limit, one context window (required) and one cancel grace in the configuration (`limits`), used the same way: a polite stop, the grace, a forced stop, for both agents; OpenCode's exit
  is the container runtime's; one nudge ever; every request has a timeout; the meter binds to the loopback by default.

*Unverified*, as before: everything that needs a container service. `bench/README.md`, "Unverified", lists what each fix could not
prove here.
