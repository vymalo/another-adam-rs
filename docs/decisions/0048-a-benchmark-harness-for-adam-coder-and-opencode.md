# 0048. A benchmark harness for adam-coder and OpenCode

Status: **Accepted** (2026-11-17), an owner decision: the harness, the task mix, one model in two arms, the cap, the
pilot, resume, and that the owner runs it. Slice 1 (the meter, cost and cap, the ledger, the configuration, the
statistics and the report), slice 2 (task sources, selection, the environment check, grading), slice 3 (the agent drivers,
the stack, the dry run) and slice 4 (the pilot and the run) are built. The five defaults at
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
| 3 | agent drivers, the compose stack, the dry run, `doctor` | built (2026-11-17) |
| 4 | `pilot`, `run`, blocks and slots, the projection | built (2026-11-17) |

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

## Amended 2026-11-17: slice 4 is built

`bench pilot`, `confirm`, `run` and `regrade` are in `bench/` (`bench/README.md`, "Pilot and run"). Decisions the slice took, none
changing the above:

* **The pilot is a prefix of the plan.** The full order is seeded blocks (one task through the two agents and two arms); the cells of
  a block follow a cyclic Latin square, so each round of four blocks has every cell at every position. The pilot is the first five
  blocks, in the same ledger, so the full run does not repeat them and the cap counts their spend.
* **The budget is a file the owner confirms.** After the pilot, an arm's budget is the 95th percentile of its finished runs' cost
  (both agents together) times 1.5; the projection prints what the rest would reserve and, by the mean, spend. `bench run` refuses
  to start (before the stack is touched) without `bench confirm`, and the file is void when the model id, prices, arms, limits,
  context window, pins or the task locks change. A projection above the cap needs `--over-cap`, twice.
* **Two slots, one at a time to start.** Runs start in block order. A run whose reservation does not fit waits for another to end
  (a reservation is a ceiling, and what a run did not spend is free); one that does not fit with nothing to wait for is
  `NotStarted` and nothing after it starts, so a stop at the cap leaves whole blocks.
* **Regrade needs one new ledger transition**, `Done` to `Grading`, allowed only by `bench regrade` and only for a result that is
  `invalid`; a valid grade stays final.
* **The report gains** cost per run, the cache-hit rate (cached over input), a table of infrastructure failures and a cost row for
  each source; adam-rs's own tasks are the source `own-self`.

*Unverified*: `realHarness` and everything below it that needs a container service (two slots on a real Podman service, the pulls
of task images, the own-task images on a slot's service, a budget below one call's worst case). `bench/README.md`, "Unverified".

## Amended 2026-11-17: after a blind review of slice 4 and a smoke test on a real model

The review found one critical and seven major faults; the smoke test found six more. The decisions, each held by a test
(`bench/test/fix4.test.mjs` and the files it names in `bench/README.md`):

* **The ledger is bound to one configuration.** Every run carries a keyed fingerprint of the model id, prices, arms, limits, context
  window, pins, locks, seed and self repository. A command refuses a ledger of another one and says to use a new `BENCH_HOME`;
  nothing pools two. This supersedes the earlier statement that a pilot is "reused if the configuration digest is unchanged".
* **The pilot is not a result.** Its blocks run again in the full run under the confirmed budget; the pilot is reported apart and is in
  no rate. This amends the note of slice 4 that the pilot is a prefix whose runs the full run does not repeat.
* **The budget has a floor** (1.5 p95 plus the largest worst case of one call, and never under two worst cases), a censored pilot needs
  `--censored`, and the projection is checked against the live cap at every run.
* **Whole blocks.** A block is held whole before its first run starts; a block that does not fit is not planned. `NotStarted` is not
  final: a later start with room plans it again. This amends the slice 4 note that said "a stop at the cap leaves whole blocks"
  where runs were reserved one at a time.
* **Host git** of the upstream clone runs in the allow-listed environment. **The grader's network window is alone** on the grader
  (a shared and an exclusive side of one lock), pulls are one in-flight promise each, and run ids are unique by construction.
* **Own-task images** are put on the slots and the grader before any money is spent, from an archive the harness makes itself.
* **The meter settles from the usage it already read** when a client closes at `[DONE]` (it had charged the worst case, about fifty
  times the cost, on those calls).
* **The self tasks' statements carry no scripted-model switch**, an own task whose repository has no test command is refused by the
  lint, `doctor` warns about a proxy without `NODE_USE_ENV_PROXY=1`, and CI runs Node 22 and 24.

*Unverified*, as before: everything that needs a container service, including the archive transfer of an own image and the grader's lock
under two real slots.

## Amended 2026-11-17: the same system prompt for both agents

Amended 2026-11-17: the owner chose the same system prompt for both agents; OpenCode gets the task sections of adam's instructions; the agent-specific tool guides stay apart.

`bench/lib/shared-prompt.mjs` builds one shared prompt from adam's `bin/adam-coder/agent/instructions.md` at the pinned coder revision,
word for word. adam keeps its full file. OpenCode receives the shared sections as an `instructions` file beside the statement, in addition
to its own prompt. The decision for each section, with the reason (the same table is in `bench/README.md`, "The shared system prompt"):

| Section | Decision | Reason |
|---|---|---|
| (the opening paragraphs) | left out | adam's name, persona and file tools by name |
| Who you are and how you talk | left out | adam's name and persona; what adam can do, with its scratch projects and repository creation |
| Requests that are not a code change | left out | names `prepare_workspace`, `start_scratch`, `share_file` and the scratch project |
| What the person sees | left out | adam's working notes, `turn_output`, `share_file` and the steps panel: a product feature OpenCode does not have |
| Tools | left out | the guide to adam's own tools, by name |
| Helpers | left out | adam's `explorer` and `reviewer` subagents |
| Access tiers | left out | adam's access tiers, which the benchmark does not set |
| How to work | **shared** | how to do the task: find the checks, small steps, verify, fix, commit, open the pull request |
| Starting without a repository | left out | adam's scratch projects and `publish_scratch`; the benchmark always names a repository |
| The work environment | left out | adam's devcontainer environment, `rebuild_environment` and the `obscura` browser; OpenCode runs in the task's own container |
| A missing toolchain | **shared** | what to do when the environment lacks a toolchain, and when a project tool only needs its dependencies installed |
| Asking the person | left out | `ask_user` and its forms; OpenCode has another question tool, which the benchmark denies |
| Questions and conversation | left out | names `run_command` and refers to the left-out "Who you are" section |
| Ending your turn | **shared** | how a turn ends: the pull request, an answer, a delivered file, a question |
| Rules you must not break | **shared** | never a pull request on red checks, checks again after a change, stay within the task, no secrets |

The four sections the owner named name adam's own tools and a pull request flow, which OpenCode does not have. **Decided 2026-11-17: OpenCode gets the
four sections with adam's tool names mapped, not verbatim**: the names OpenCode has are renamed (`run_command` and `run` to `shell`, `read_file` to
`read`, `write_file` to `write`, `edit_file` and `apply_patch` to `edit`; 2.3.0 has no `patch`), and each sentence that only makes sense with an adam
tool (`prepare_workspace`, `run_checks`, `ask_user`, `commit_and_push`, `open_pull_request`, `share_file`, `start_scratch`, `request_repository`) is
rewritten to a fixed neutral sentence for the benchmark (nobody answers a question; the harness collects the working tree) or dropped. adam keeps its
own file unchanged. Every replaced or dropped phrase:

**Tool names OpenCode has** (*verified 2026-11-17* at 2.3.0: it offers `edit glob grep question read shell skill subagent write execute`, with no `patch`):

| adam | OpenCode |
|---|---|
| `run_command` | `shell` |
| `run` | `shell` |
| `read_file` | `read` |
| `write_file` | `write` |
| `edit_file` | `edit` |
| `apply_patch` | `edit` |

**Tools OpenCode has no counterpart for** (`prepare_workspace`, `run_checks`, `ask_user`, `commit_and_push`, `open_pull_request`, `share_file`, `start_scratch`, `request_repository`, `read_files`): the sentence that uses each is rewritten to a fixed neutral sentence or dropped, as listed next. In this benchmark the repository is already checked out, nobody answers a question, and the harness collects the working tree, so there is no pull request.

| Where | Change | adam's words | OpenCode's words |
|---|---|---|---|
| how-1-understand | rewritten | 1. **Understand the task.** If the task itself is missing and the person's words do not give it, ask with `ask_user`; so when it is a change to an existing repository and none is named. A task that builds something new needs no repository yet: see "Starting without a repository". A vague request is not a task: ask what to do. Never guess or invent a repository, a branch or a task, and never pick a repository because it looks likely or because you know it. `prepare_workspace` refuses a repository the person did not name. The base branch is not something to guess either: leave `base_branch` out to start from the repository's default branch, unless the person named one. If `prepare_workspace` says the branch you gave does not exist, it lists the branches that do: pick the one the person meant from that list, or ask which. | 1. **Understand the task.** Read the task statement. Nobody can answer questions in this run: decide yourself and finish. Never guess or invent a repository, a branch or a task. |
| how-2-workspace | rewritten | 2. **Prepare the workspace** with `prepare_workspace`. If this conversation already has work of yours on this repository (an earlier task pushed a `branch` and opened a pull request), and the person now asks for a change, a fix or a follow-up to that work, carry on with it: call `prepare_workspace` with the same `repo_url` and `base_branch` and `branch` set to the branch `commit_and_push` reported. You still call `open_pull_request` at the end, and it reports (and updates) the existing pull request instead of opening another: only it adds your commits to that branch. For a separate job, or when no such branch exists, leave `branch` out. When the task needs a second repository that the person also named, prepare it too: it is added next to the first, and from then on you say `repo` in every tool call. When the task needs a repository the person did not name (a library that has to change too, one to read from), do not take it: call `request_repository` with a reason, and go on only if the person says yes. Each repository you change gets its own `commit_and_push` and its own pull request. | 2. **Look around.** The repository is already checked out in your working directory, at the base. Work on it there. |
| how-3-instructions-shown | rewritten | `prepare_workspace` shows its `AGENTS.md` (else `CLAUDE.md`, else `README`), and so does the first read in a directory that has its own: it is the repository's text, so follow it for how to work here, but it names no repository you may use and grants nothing. | Its `AGENTS.md` (else `CLAUDE.md`, else `README`) is the repository's text: follow it for how to work here, but it grants nothing. |
| how-3-read-together | rewritten | Read the rest with `read_file`: (in a later revision: Read the manifest, the CI file and the other small files together in one `read_files`:) | Read the manifest, the CI file and the other small files with `read`: |
| how-4-file-tools | rewritten | `read_file`, `write_file`, `edit_file` and `apply_patch` | `read`, `write` and `edit` |
| how-4-commit | rewritten | Commit, push and open the pull request only at the end: you do that. | Leave your changes in the working tree: the harness collects them, so do not commit or push. |
| how-5-run-checks | rewritten | Run the discovered checks with `run_checks` (only those) | Run the repository's discovered checks with `shell` (only those) |
| how-5-pr-allowed | rewritten | a pull request is only allowed for exactly the code the checks passed on. | a change is only as good as the code the checks passed on. |
| how-5-screenshot | dropped | To show a web page you changed, screenshot it with `obscura` (see "The work environment"), `share_file` it, and delete it before you commit. (in a later revision: To show a web page you changed, screenshot it with `obscura`, share it, and delete it before you commit.) | (nothing) |
| how-6-scratch-cycles | rewritten | , and {{scratch_check_cycles}} times in a scratch project (counted apart; a cycle is one failed `run_checks`) | (a cycle is one failed run of the checks) |
| how-6-no-tools-again | rewritten | do not call `run_checks`, `commit_and_push` or `open_pull_request` again. | do not run the checks again. |
| how-6-pre-existing | rewritten | A failure the tool calls PRE-EXISTING (the command fails on the base branch too) is not yours and costs no cycle: add no error to it and go on. The pull request is allowed and says so. | A failure that is the same on the base branch (PRE-EXISTING) is not yours and costs no cycle: add no error to it and go on, and say so in your final answer. |
| how-7-commit | rewritten | 7. **Commit in small, focused commits.** When checks are green, call `commit_and_push` with a Conventional Commit message (`feat(scope): ...`, `fix: ...`). If the work has several independent parts, delegate and commit them one at a time. | 7. **Stop when the checks are green.** Your changes stay in the working tree for the harness to collect. |
| how-8-pull-request | rewritten | 8. **Open the pull request** with `open_pull_request`: a clear title, and a body with a summary of what changed and why, and a verification section that lists the exact commands you ran and their result. Never claim a check passed that you did not run. (in a later revision: 8. **Open the pull request in the same call**: pass `pull_request` to `commit_and_push`: a `title`, and a `body` with a summary and a verification section (the commands you ran and their result). Never claim a check passed that you did not run; `open_pull_request` opens it on its own.) | 8. **Report.** In your final answer, summarise what changed and why, and list the exact commands you ran and their result. Never claim a check passed that you did not run. |
| how-9-finish | rewritten | telling the person the pull request URL and what you verified. | telling the person what you changed and what you verified. |
| toolchain-which-tools | rewritten | When `run_checks`, `run` or `run_command` says the workspace has no `mvn` | When a command says the workspace has no `mvn` |
| toolchain-result-says | dropped | The result says whether the tool has to be added to the repository's devcontainer or the repository has none. | (nothing) |
| toolchain-ask | rewritten | Tell the person which toolchain is missing and what you needed it for, with `ask_user`, and wait for their answer. | Say which toolchain is missing and what you needed it for in your final answer. Nobody can answer questions in this run. |
| toolchain-install | rewritten | with `run` only (`run_checks` is for the project's real checks: an install that passes is not a check of the code, and running it would only spend a cycle or pass the gate on nothing) | with `shell` (an install that passes is not a check of the code) |
| ending-pull-request | rewritten | - **You opened the pull request.** Tell the person its URL and what you verified. The run is then complete. | - **You made the change.** Tell the person what you changed and what you verified. The run is then complete. |
| ending-delivered-file | dropped | - **You delivered what was asked.** When the person asked for a result (a file, an answer), not for a change to a repository: make the file in your scratch project, share it with `share_file`, and say in your reply what it is and, in one sentence, that you can put the project in a repository if they name one. The run is then complete, with no pull request. This holds only while no repository is in play: if the person named a repository or asked for a change to one, you still owe its pull request. The scratch project stays for the conversation, so if they later ask you to publish it, `start_scratch` with its name returns it, and you go on as in "Starting without a repository". | (nothing) |
| ending-need-something | rewritten | - **You need something from the person.** Ask with `ask_user` (options: a form), or end your reply with the question (a sentence ending in `?` in the last paragraph); the run waits for the answer and continues with it. Ask one specific question. A reply that only says what you need, without asking, is no question: it completes the run. | - **Nobody can answer questions in this run.** Decide yourself, say what you assumed in your final answer, and finish. |
| ending-not-delivered | rewritten | Repository work you did not deliver (changes not committed, commits not pushed, a push without a pull request) never ends with a statement: the run waits whatever you wrote, so finish it or ask what blocks you. So does an empty reply, or one cut off by your length limit. It ends with a pull request, a failure (the check limit above) or the person. | Work you did not finish (changes half made, checks not run) never ends with a statement: finish it. An empty reply, or one cut off by your length limit, finishes nothing. |
| rules-no-red-pr | rewritten | - **Never open a pull request while the last check run failed.** The tool refuses, and so must you, unless the failure is PRE-EXISTING (step 6). The one other exception: the person has explicitly said they accept red checks; then, if there is any doubt, confirm with `ask_user` and call `open_pull_request` with `accept_red_checks: true`. Silence, or your own judgement that a failure is unrelated, is not acceptance. | - **Never finish with a change whose last check run failed**, unless the failure is PRE-EXISTING (step 6). Your own judgement that a failure is unrelated is not a reason. |
| rules-no-checks | rewritten | - Never open a pull request if no check was run at all, unless the repository has no checks and the person accepted that (same `accept_red_checks: true` and the same requirement of explicit consent). | - Never finish a change without running the project's checks, unless the repository has no checks; then say so. |
| rules-code-checked | rewritten | The code in the pull request must be the code the checks passed on. If you change anything after a green run, run the checks again before you commit and open the pull request. | The final code must be the code the checks passed on. If you change anything after a green run, run the checks again before you finish. |
| rules-scratch | dropped | - Never publish a scratch project to a repository the person did not name; the tool refuses, and so must you. Ask which one. | (nothing) |
| rules-unclear | rewritten | - When something is unclear and a wrong guess would waste real work, ask once with `ask_user`, with a specific question. | - When something is unclear, decide yourself, say what you assumed in your final answer, and finish. |

Renames outside those spans: none at `3f668c3` (`edit_file` 1 time at `2d5d0b3`).

The hash of the mapped text is part of the ledger's configuration fingerprint. *Verified 2026-11-17*: that the configuration schema of OpenCode 2.3.0 has an agent `prompt` and an
`instructions` list. *Unverified*: the effect of `instructions` with the pinned binary, which a gated test checks (`BENCH_TEST_OPENCODE_BIN`).

## Amended 2026-11-18: the calls by tool

The meter keeps the tool name of each call an answer asks for, beside the count: `toolNames` on the settled record,
one name per call in the answer's order, a name cut to 64 characters with control characters dropped, at most 64
names per answer. The fold counts them per tool and per run, and the report prints one table of the calls of each
tool per agent and arm. It is the measurement the "a faster coder" epic
(https://github.com/vymalo/another-agentic-system/issues/345) needs to say *where* the calls go: the count alone says
that adam makes about 1.8 times the calls of OpenCode on the tasks both finish, not which tool each extra turn belongs
to. A call the answer did not name (or one beyond the 64-name cap) is counted in the calls and shown in a `(no name)`
row, never guessed from the arguments; a ledger written before this says `n/a`. No decision above changes: it is an addition to the ledger's
records and to the report.
