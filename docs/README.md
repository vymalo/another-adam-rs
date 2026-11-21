# adam-rs docs

Start with the [root README](../README.md), then the guide for what you are doing. Every crate also has a
`README.md` next to its `Cargo.toml`.

## Guides: do a thing

| Guide | When |
|---|---|
| [Try the agents](guides/try-the-agents.md) | you want to chat with the Assistant, give Adam a task, write your own agent, then do it all on a real model |
| [Run it locally](guides/run-locally.md) | you want the coder and the general agent running on your machine in minutes |
| [Write an agent](guides/write-an-agent.md) | you want an agent that is a folder of Markdown, or one with `#[tool]` functions |
| [Deploy the coder](guides/deploy-the-coder.md) | you are installing the image and the Helm chart on Kubernetes |
| [Embed adam](guides/embed-adam.md) | you host adam agents inside your own Rust program, a server |
| [Embed adam in an application](guides/embed-in-an-app.md) | adam runs inside your application (a desktop or a phone), with no server and nothing on the network of its own |
| [Testing](guides/testing.md) | you want to run or add tests, the end-to-end scripts, or check the chart |
| [The benchmark harness](../bench/README.md) | you want to compare adam-coder and OpenCode on real tasks, with a hard cap on spend |

## Architecture: understand it

| Doc | What it answers |
|---|---|
| [Architecture](architecture.md) | the mental model, the crate map, the ports, the path of a task, the run lifecycle, the data schema, signals, errors |

## Reference: look it up

| Doc | What it answers |
|---|---|
| [Environment variables](reference/environment.md) | every variable of the binaries, the compose stack and the tests |
| [Agent files and `#[tool]`](reference/agent-files.md) | the folder layout and formats, validation, skills, subagents, MCP, dev reload, the macro |
| [The A2A server](reference/a2a-server.md) | the methods, follow-ups, continuing a finished task, push notifications, `ListTasks`, the extended card, the card's signature, and the steps, text and reasoning streams |
| [Child runs and remote tasks](reference/child-runs.md) | how a subagent waits, every failure interleaving and its test |
| [Workspace and environments](reference/workspace-and-environments.md) | placement, a run's slots, devcontainers and run pods |
| [The coder agent](reference/coder-agent.md) | its flow and states, the rules its tools enforce, how a run ends |
| [Errors](reference/errors.md) | error classes, the variant-to-class tree, A2A codes and exit codes |
| [Store adapters](reference/store-adapters.md) | what each database adapter guarantees and how, data caveats |
| [The local stack](reference/dev-stack.md) | compose services, mocks, scripted models |
| [Roadmap](roadmap.md) | what is built and what is not |
| [The chart](../deploy/coder/README.md) | every value of the coder's Helm chart |

## Reviews

| Review | What it found |
|---|---|
| [Optimisation review, 2026-11-18](reviews/2026-11-18-optimisation.md) | where the framework's RAM, CPU and I/O go, and the ranked fixes |

## Decisions

ADRs are history: a change to a past decision is a dated *Amended* note, never a silent edit.

| ADR | Decision |
|---|---|
| [0001](decisions/0001-library-first-host-roles.md) | library first; hosts run a control plane and workers through the closed `Role` and a supervisor |
| [0002](decisions/0002-workspace-placement.md) | the closed `Placement` enum, pinned claims over a run owner, a mirror lock |
| [0003](decisions/0003-a-new-task-continues-the-task-it-references.md) | a new task that references a finished one starts from its conversation |
| [0004](decisions/0004-agent-folders-at-run-time.md) | a binary reads its agent folder once at startup; a restart is a deploy |
| [0005](decisions/0005-one-binary-serves-any-agent-folder.md) | `adam-agent` serves any agent folder, over `adam-service`, in the coder image |
| [0006](decisions/0006-a2ui-and-the-vymalo-extensions-in-adam-rs.md) | A2UI questions, a message's extensions as run context, tool sources, `adam-ui` |
| [0007](decisions/0007-progress-as-steps-and-streamed-text.md) | tool calls as `Step` events, the model's answer streamed as it is written |
| [0008](decisions/0008-a-workspace-holds-several-repositories.md) | a workspace is a directory of slots: repositories and scratch projects |
| [0009](decisions/0009-github-per-installation-read-through-mcp.md) | a token or a GitHub App per installation; GitHub read through the official MCP server |
| [0010](decisions/0010-a-run-works-in-its-repositorys-devcontainer.md) | a run's commands run in its repository's devcontainer, on rootless Podman |
| [0011](decisions/0011-a-tool-calls-step-carries-its-input-and-output.md) | a tool call's step carries its input and output, cut and redacted |
| [0012](decisions/0012-files-as-a2a-artifacts.md) | a file an agent made is an A2A artifact with a `raw` part |
| [0013](decisions/0013-run-keeps-changes-edit-file-and-scratch-completion.md) | three shell tools of one job each, `edit_file`, and scratch work that completes without a PR |
| [0014](decisions/0014-a-turn-output-answer-is-the-runs-answer.md) | a tool can announce the run's answer, and a turn can end with it |
| [0015](decisions/0015-tools-the-orchestrator-reports-long-calls-and-mentioned-agents.md) | thread tools with timeouts and reported steps; mentioned agents |
| [0016](decisions/0016-a-message-sent-to-a-working-task-is-steered-into-it.md) | `steer/v1`: a message to a working task is read at its next step |
| [0017](decisions/0017-a-github-app-works-on-every-account-it-is-installed-on.md) | a GitHub App finds the installation of each owner, for an allow-listed set |
| [0018](decisions/0018-extra-mcp-servers-are-a-file-merged-over-the-agents-own.md) | extra MCP servers are a file merged over the agent's own, optional servers |
| [0019](decisions/0019-a-runs-processes-in-a-pod-of-their-own.md) | a run's processes in a Kubernetes pod of its own |
| [0020](decisions/0020-reasoning-is-streamed-beside-the-answer-and-never-stored.md) | reasoning streamed beside the answer and never stored |
| [0021](decisions/0021-the-coder-is-adam-a-general-agent-that-can-code.md) | the coder is Adam, a general agent that can code |
| [0026](decisions/0026-a-failure-the-base-has-too-is-not-the-runs.md) | a failing check is also run on the base: a failure it has too is pre-existing and costs no cycle |
| [0027](decisions/0027-every-tool-has-a-title-for-its-step.md) | every tool has a title for its step |
| [0028](decisions/0028-the-card-says-which-build-answers.md) | the card says which build and which agent files answer |
| [0029](decisions/0029-adam-rs-has-an-operator.md) | adam-rs ships the Kubernetes operator of `AgentService` and `AgentConfig`, moved from the platform repository (superseded by 0046) |
| [0030](decisions/0030-a2a-push-notifications-list-tasks-extended-card-signatures.md) | A2A push notifications, `ListTasks`, the extended card and card signatures |
| [0031](decisions/0031-swagger-ui-and-the-a2a-rest-binding.md) | the A2A HTTP+JSON binding beside JSON-RPC, and Swagger UI at `/docs` |
| [0032](decisions/0032-usage-per-model-call.md) | `usage/v1`: the tokens of each model call, a subagent's apart, and a task's totals |
| [0033](decisions/0033-files-from-mcp-results-are-shared-files.md) | `files: true` on an MCP server or a remote subagent: the files of its answers are shared files of the calling run |
| [0034](decisions/0034-a-turn-that-asks-nothing-completes-and-scratch-lives-for-the-thread.md) | a turn that asks nothing completes the task, and a scratch project lives for the thread (`SCRATCH_IDLE_DAYS`) |
| [0035](decisions/0035-questions-carry-a-title-a-description-and-typed-options.md) | `ask_user` forms: a title, a description of why, typed questions (radio, checkboxes, list, picture deck, typed), options with a label and a description; the screen's catalog decides what is sent |
| [0036](decisions/0036-the-coder-keeps-to-the-access-tier-the-person-picks.md) | the person picks an access tier for each message (`plan`, `edits`, `auto`, `bypass`, capped by `MAX_ACCESS_TIER`, default `auto`); the coder keeps to it in code (not against a command that can read the App's key), and gets `merge_pull_request` (checked, and asked for itself when it is not the conversation's own), `rename_repository`, `delete_repository` and `create_repository` |
| [0037](decisions/0037-parallel-subagent-calls-and-an-editing-worker.md) | the calls of one turn that say `Tool::concurrent` run side by side, each journaled at a seq reserved in call order, one park for all their children; a cancelled parent stops its children; the coder gets `worker`, an editing subagent in a worktree and branch of its own that Adam merges, the gates staying on Adam (not built yet) |
| [0038](decisions/0038-the-coder-drops-opencode-and-adam-acp-stays-a-library.md) | the coder stops using OpenCode: no `delegate_to_opencode`, no binary or model key brought into run environments, broad changes with the file tools and then ADR 0037's workers; `adam-acp` stays a tested library, and the images keep the OpenCode, Claude Code and Codex CLIs (not built yet) |
| [0039](decisions/0039-the-agent-loop-takes-opencodes-structure-and-agents-get-plugins.md) | the loop keeps its journal and takes OpenCode's shape: a turn ledger with usage per call, rules per agent with patterns (the tiers become presets), `permission:` in agent files, a last-turn grace, compaction as a journaled step, the coder's snapshots and `revert_to`; a `Plugin` trait composed at build time whose decisions are journaled, and a plugin that does not load stops the agent (not built yet) |
| [0040](decisions/0040-a-shared-package-cache-at-cache.md) | one shared ReadWriteMany volume at `/cache` for the npm, pnpm and cargo caches: the chart, the platform repository's operator, run pods and devcontainers mount it; no credential in it; a pull-through proxy later |
| [0041](decisions/0041-a-run-pod-sees-only-its-own-run.md) | a run pod mounts only its own run's directory and, for each worktree, the mirror read-only and the worktree's own directory in it: nothing of other runs, `meta/` or other mirrors; a repository added later makes the pod again; the admission policy lets a run pod reference the model key and no other Secret key (proposed) |
| [0042](decisions/0042-run-pods-reach-the-internet-through-an-egress-proxy.md) | a run pod's network is deny by default: DNS, the egress proxy and `extraEgress` only; the one way out is `adam-egress`, an HTTPS-only `CONNECT` proxy of the coder image that the chart deploys with the run pods, allowing named hosts (package registries, git hosts) and public IPv4 addresses only; IPv6 denied; the old internet rule and `runPods.networkPolicy.enabled` are gone |
| [0043](decisions/0043-sandbox-time-as-usage.md) | the time a run's commands use in a run pod is reported as `usage/v1` `sandbox`, beside `totals`: command time (spawn to exit, a timeout or cancel kill included), weighted by the pod template's requests, a subagent's merged into its parent's, kept in the run's state and journaled with the call's result; `local` and `devcontainer` say where and no time (not measured, never zero); no new URI, no live reports |
| [0044](decisions/0044-paged-tools-a-stable-prompt-prefix-a-plan-skills-commands-and-project-context.md) | a request is the previous one plus what happened since, so the provider's cache holds; every read is paged (`read_file`, `list_files`, `find_files`, `search_files`, `read_output`); `plan_work`, skills, commands and project instructions are built in; web and image generation stay MCP servers |
| [0045](decisions/0045-a-subagent-can-run-in-the-background-as-a-session.md) | a subagent can run in the background as a session: `subagent_start` returns at once, the parent keeps working and reads reports, `subagent_send` continues it with its context, and "close when done" is chosen at creation; a kept-open session lives for the thread and is announced to clients by `child-sessions/v1` |
| [0046](decisions/0046-the-operator-lives-in-the-platform-repository.md) | the operator lives only in the platform repository: adam-rs's copy, its charts, CRDs, image and skill are removed; `deploy/coder` stays the contract |
| [0047](decisions/0047-adam-rs-can-be-embedded-in-an-application.md) | adam-rs can be embedded in an application: a library with no protocol (`adam-engine`), the embedded set as Cargo features, a SQLite store (amended 2026-11-19 by ADR 0053: `rusqlite` in the main workspace, not cratestack), the host's model client, and ports for secrets, approvals and the tools' workspace (accepted 2026-11-16; slices 1, 1b and 2 built, and slice 6, the tools as a library, complete; slice 4's PRs 1–3 built 2026-11-20 (the engine's API, its events, the example), and slice 4 is not done until the proof that a killed process resumes, which waits on the SQLite store) |
| [0048](decisions/0048-a-benchmark-harness-for-adam-coder-and-opencode.md) | a benchmark harness in `bench/` for adam-coder against OpenCode: one meter that is the truth for cost and the cap, reservations, a ledger with resume, grading by patch replay (accepted 2026-11-17; the meter, cost, cap, ledger, report, task sources, environment check, grading, the stack, the two agent drivers, the runner, doctor and the dry run are built; the second agent is OpenCode V2) |
| [0049](decisions/0049-the-tool-calls-of-a-turn-run-at-the-same-time.md) | the tool calls of a turn run at the same time, up to a limit (10 by default, 1 to 32): a tool declares what a call shares (`Tool::access`), conflicting calls keep the model's order, the journal is the sequential one (`Ctx::reserve_steps`), results are applied in call order (accepted 2026-11-17; the runtime primitive, `ToolAccess`, the scheduler, the loop, `MAX_PARALLEL_TOOL_CALLS`, `after_previous` and the library tools' classes are built, so the MCP calls of a folder agent now overlap; the coder's classes are not built) |
| [0050](decisions/0050-hooks-around-the-agent-loop-compiled-first-and-programs-second.md) | hooks around the agent loop: a compiled `Plugin` first, then programs over JSON lines on stdio named by the deployment only; observers of every loop event and of custom events; deciding points (`message.received`, `model.request`, `tool.before`, `tool.after`, `compaction.before`, `run.ending`) journaled in their step and never asked again on replay; hooks ask the loop for actions and never write a summary; fail closed (accepted 2026-11-18; nothing is built) |
| [0051](decisions/0051-a-nul-is-stored-as-the-replacement-character.md) | a NUL in a tool's output is stored as U+FFFD by every adapter (`adam_core::storable`), before the write, so a run neither fails on PostgreSQL's `JSONB` nor reads a different value on another backend; the testkit requires it (accepted 2026-11-18; built) |
| [0052](decisions/0052-no-podman-a-repositorys-own-environment-is-a-run-pod-of-its-image.md) | no Podman anywhere, a hard cutover: a run's commands run in the coder's own container (on a desktop, the person's work sandbox) or in a run pod; a repository's own environment is a run pod of the image its `devcontainer.json` names, through `use_repository_environment`, a tool offered only with run pods, an allowed image prefix and a configuration in a writable slot; a configuration that only builds is refused in words; the bench moves to the host's container runtime (accepted 2026-11-19; the reader, the port's image method, `use_repository_environment`, the chart's `runPods.repositoryImages`, the dev stack and the code are built, the code by slice 3; the bench is not) |
| [0053](decisions/0053-sqlite-a-store-service-a-database-per-agent-and-a-static-coder.md) | SQLite joins Postgres and MongoDB as a store (`rusqlite`, in the main workspace, since `adam-store-postgres` leaves `sqlx` for `tokio-postgres`) and every binary picks its store from `DATABASE_URL`; a store service serves `Store` over CrateStack RPC/CBOR, batched and pipelined, so the agent pod holds no database credential; the platform makes one CloudNativePG `Database` and `DatabaseRole` per agent in a shared cluster; the image's binaries are static (musl) (accepted 2026-11-19; nothing is built) |
| [0054](decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md) | a child session can be configured, edited and deleted: an optional `config` on `subagent_start` and on a person's start only chooses within, or narrows, what the subagent's folder and the deployment allow (limits, context window, tools, token budget, instructions, skills, tier, report schema, model from an allow-list); it is recorded in the session header; `subagent_edit`, `subagent_close {delete}` and a fixed section on the session tools in the system prompt (accepted 2026-11-19; nothing is built) |
| [0055](decisions/0055-a-runs-state-is-a-head-and-an-append-only-transcript.md) | a run's state is a head and an append-only transcript: one body row per message, file bytes in a content-addressed blob store, schema 5 (6 on PostgreSQL), defaulted store methods; expand then contract, two releases; follows the I/O-layer pull request (accepted 2026-11-19; S0–S3 are built on 2026-11-20 — the counting store and the measurement, blobs, the shadow, the view, and the contract: `state` is the tombstone, every read serves the head, the PostgreSQL half is in; the contract is a separate deploy) |
| [0056](decisions/0056-a2ui-v1-0-only-a-hard-cutover.md) | A2UI v1.0 only, a hard cutover in R1: envelopes say `"version": "1.0"`, the card lists `https://a2ui.org/a2a-extension/a2ui/v1.0` and `ui-catalog/v2`, the renderer's capabilities are read only under `a2uiRendererCapabilities["1.0"]` and a turn without them draws nothing; the v0.9.1 constants are removed, the media type stays; the official schemas, vendored at `a2ui-project/a2ui` `b4f91c2`, are the test oracle; two slices merged into `main` once, as pull request #251 (accepted 2026-11-19; A1 and A2 are built) |
| [0057](decisions/0057-time-is-jiff-with-the-bytes-chrono-wrote.md) | time is `jiff`, with the bytes chrono wrote: `adam_core::time::Timestamp` is the instant of every `Store`, runtime, agent and push value and serializes as chrono's `AutoSi` RFC 3339; the A2A types (`TaskStatus`, `TaskQuery`, `PageToken`) stay chrono, PostgreSQL maps through sqlx's chrono support and MongoDB through BSON dates, so no stored byte changes; a `[bans]` rule keeps chrono out of every other crate (accepted 2026-11-19; built) |
| [0058](decisions/0058-the-models-credentials-come-from-a-provider-asked-for-each-call.md) | the model's credentials come from a provider asked for every call (`ModelCredentials` in `adam-model`): the key is the default provider, a host mints, renews or proves a token, or tags the run for an injector; one retry on 401; nothing cached or journaled by the client (accepted 2026-11-19; built: the port, the OpenAI client, and the device grant of `adam-model-oauth` behind `MODEL_AUTH=device`) |
| [0060](decisions/0060-schedules-are-parked-runs.md) | schedules are parked runs of a built-in `adam.schedule` agent, one open per schedule's conversation, woken by the store's durable timer, with no new `Store` method: one firing per due time (a run of the root agent whose id is a hash of agent, schedule and due time), missed due times fire once with a `missed` count, exactly one of `cron`, `at` or `after`, reconciled when a worker starts, jiff's bundled time zones, an own five-field cron parser, and retention never purges a scheduler run; the new crate `adam-schedule` (accepted 2026-11-20; built: the format (`at`, `after`, `Cron::parse`, slice 2) and the evaluation and ids (`adam-schedule`, `RunStarter`, slice 3); the scheduler and `reconcile` (slice 4), and `adam-agent` (slice 5) and the coder (slice 6) run a folder's schedules) |
| [0061](decisions/0061-a-step-names-its-retry-policy.md) | a step names its retry policy: a policy lists the retryable classes it retries (`retry_on`), errors carry their class (`AgentError` and a new `ToolError::Retryable`), a step, a tool (`Tool::retry_policy`, `#[tool(retry(...))]`) or a folder's `retry:` per tool names a policy under a key (`tool:<name>`), failures are counted per key under a ceiling of 100 consecutive failures, and the policy of a failing key is recorded in the run's state (the head of a v5 run) at its first failure, never recomputed; the runtime's policy and a step that names none are unchanged (accepted 2026-11-20; built 2026-11-21 in four slices, #263 to #266) |
| [0062](decisions/0062-a-race-of-steps-records-its-winner.md) | a race of steps records its winner: `Ctx::step_race` runs its contenders at the same time as one journaled step, the first success wins, the losers' cancel tokens fire and their futures are dropped, and the step records the winner's index and value (or every error, in order), so a replay returns the same winner and runs no contender; the journal format is unchanged (accepted and built 2026-11-21) |

## Keeping these docs short and true

* Humans read the root `README.md` and `docs/`. Keep them short: tables and diagrams over prose, link instead of copying.
* A process gets a Mermaid pair: a `sequenceDiagram` for the interaction and a `stateDiagram-v2` for the lifecycle.
  Every node, edge, state and call must exist in the code; cite the file.
* A change that alters behaviour updates its guide, reference or architecture section in the same pull request.
* Mark facts about third parties *verified* (date and source) or *unverified*.
* `node tools/docs-check/check-docs.mjs` parses every diagram, resolves every relative link and `#heading`,
  requires a `README.md` per crate and checks the first-party skills (CI's `docs` job;
  `npm --prefix tools/docs-check ci` once).
