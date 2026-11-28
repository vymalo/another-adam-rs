# Roadmap

For agents: [the evidence and how to re-check it](../.agents/docs/roadmap.md)

This is the library's own roadmap: what is built, what is missing, what is half done and what is big. The product
roadmap, with its milestones, is
[`ROADMAP.md` of another-agentic-system](https://github.com/vymalo/another-agentic-system/blob/main/ROADMAP.md).
The feature table by area is in the [README](../README.md#features); every setting by surface is in
[Parity](reference/parity.md).

Each row names a card (`ANR-NNN`), a GitHub issue, or says "no card yet". Where a row says *proven*, it says what
proves it. Everything else is built and not yet proven.

## Where it stands (2026-11-28)

* The library is wide and green on mocks: the durable loop, three stores, A2A 1.0, the coder and the folder agent are built.
* Almost nothing here has run against a real model, a real GitHub or a real cluster, and no release has ever been published.
* `v0.7.0` is tagged and its release run is queued, so the first signed image, chart and `adam-agent` binary do not exist yet.

## Built and proven

Proven means CI runs it. The list of features is in the [README](../README.md#features); these are the parts the rest
stands on.

| What | Proof |
|---|---|
| The durable loop: journaled steps, leases, a run resumed by any worker, state as a head and an append-only transcript ([ADR 0055](decisions/0055-a-runs-state-is-a-head-and-an-append-only-transcript.md)) | the store conformance suite on PostgreSQL 18, MongoDB 5.0 and 8.0, and SQLite |
| The A2A 1.0 server and its extensions | the dev stack's end-to-end scripts, on a scripted model |
| Folder agents, `#[tool]`, subagents, MCP servers | CI, on mocks |
| The coder's tools: paged reads, plan, skills, commands that keep running, language servers ([ADR 0044](decisions/0044-paged-tools-a-stable-prompt-prefix-a-plan-skills-commands-and-project-context.md)) | CI, on mocks |
| Run pods, the egress proxy and the git sandbox: the chart's objects on kind | the `run-pods` CI job on kind |

## Missing: not started

| What | Why it matters | Card |
|---|---|---|
| The store service, so an agent's pod holds no database credential ([ADR 0053](decisions/0053-sqlite-a-store-service-a-database-per-agent-and-a-static-coder.md), slices 4 to 7) | no credential in an agent's pod | no card yet |
| Replicated SQLite and a run resumed on a new machine ([ADR 0078](decisions/0078-a-sqlite-store-is-replicated-per-commit-and-a-new-machine-resumes-from-the-copies.md), proposed) | a device that is lost keeps its runs | no card yet |
| A folder agent that sleeps and waits on a named queue ([ADR 0074](decisions/0074-a-folder-agent-can-sleep-and-wait-on-a-named-queue.md), proposed) | long waits without a held run | no card yet |
| Metrics and traces for the whole runtime ([ADR 0075](decisions/0075-metrics-and-traces-are-off-by-default-and-say-nothing-about-a-person.md), proposed; the OTLP layer is composed by the four binaries and two span kinds are built, no metrics listener yet) | an operator sees what the runtime does | ANR-268 |
| The memory's database adapters and its later slices ([ADR 0081](decisions/0081-an-agent-keeps-a-self-improvement-memory-near-it-and-what-holds-becomes-the-deployments-memory.md): PostgreSQL, MongoDB and Redis, then the promotion pass and the reviewer) | the memory survives a restart and is shared | ANR-1292, ANR-1293, ANR-1294 |
| A coder's snapshot plugin, `ExecHook` and the chart's volume for hook programs ([ADR 0050](decisions/0050-hooks-around-the-agent-loop-compiled-first-and-programs-second.md), H10, H11, P3) | a run that can be reverted; programs that can veto a command | no card yet |
| The dev TUI, host adapters (axum/tower) and sandboxes as adapters | none is planned yet | no card yet |
| A folder's own `compaction` subagent ([ADR 0044](decisions/0044-paged-tools-a-stable-prompt-prefix-a-plan-skills-commands-and-project-context.md), slice 9) | compaction is a single summary today | no card yet |
| Images out, made by the model | deferred by the owner until a provider is named | no card yet |
| Editing workers and sub-agent worktrees | on hold | no card yet |
| Encrypted webhook credentials and an extended card per caller | A2A push notifications ([ADR 0030](decisions/0030-a2a-push-notifications-list-tasks-extended-card-signatures.md)) | no card yet |

## Incomplete: started but not done, or built but unproven

| What | State | What would prove it | Card |
|---|---|---|---|
| The first release | `v0.6.0` was never published: its smoke test ran PostgreSQL 17, which the store refuses. `v0.7.0` is tagged; its release run is queued | the release run passes: an `X.Y.Z` image, a chart and signatures that resolve anonymously | no card yet |
| The Homebrew path for `adam-agent` (#495) | built; never run. Four builds, the signed `SHA256SUMS`, the Homebrew installs and the tap pull request wait for the first release. #495 and #530 were merged without CI having run on them | the first release run, then a `brew install` from the tap | ANR-1203 |
| The coder, chat to pull request | proven on a scripted model, a mock GitHub and a local git server; never on GitHub.com, never on a real model | a live run to a real pull request, and the bench on a real model ([bench](../bench/README.md)) | [#102](https://github.com/vymalo/another-adam-rs/issues/102) |
| Model client and credentials | proven on mocks; the nightly live test skips without its secrets; the device sign-in and the injector have never met a real provider | the nightly job with secrets, and the human test of usage and reasoning against a real gateway | [#102](https://github.com/vymalo/another-adam-rs/issues/102) |
| Run pods | the chart's objects run on kind; a run on a real model through a run pod is not proven | the same run on a real cluster | no card yet |
| A run pod holds no model key ([ADR 0038](decisions/0038-the-coder-drops-opencode-and-adam-acp-stays-a-library.md)) | not built: every run pod still gets the key | a run pod with no Secret, shown by a test | [#119](https://github.com/vymalo/another-adam-rs/issues/119) |
| The coder checks who the person is and their access ([ADR 0072](decisions/0072-the-coder-verifies-who-the-person-is-and-asks-github-before-it-touches-a-repository.md), proposed) | not built: one GitHub App acts for everyone | a person is refused a repository they cannot reach | [#121](https://github.com/vymalo/another-adam-rs/issues/121) |
| A git mirror per person | not built: two people on one repository can read each other's unpushed work | a run pod cannot read another person's refs | [#122](https://github.com/vymalo/another-adam-rs/issues/122) |
| The package cache | not built: any run can write what another's build reads | a run cannot change another's cache | [#123](https://github.com/vymalo/another-adam-rs/issues/123) |
| The Helm chart | renders and goldens only; never applied with more than one worker; no database backups; `flock` on network volumes unverified | a rollout of several workers on a real cluster | [#127](https://github.com/vymalo/another-adam-rs/issues/127) |
| The coder's database for scale and pruning of finished runs | not built: nothing deletes a finished run | a periodic sweep and chart values for several instances | [#127](https://github.com/vymalo/another-adam-rs/issues/127) |
| The SQL adapters keep their rules in the database ([ADR 0082](decisions/0082-the-sql-adapters-keep-their-rules-in-the-database.md)) | P0 to P2 built; P3 to P7 and the two SQLite slices to do | the store suite after each slice | ANR-1148 |
| The activity route for scaling ([ADR 0083](decisions/0083-the-activity-route-has-a-listener-and-tokens-of-its-own.md)) | the listener and its tokens are built; no scaler has used them on a cluster | the platform's operator reads it on a cluster | no card yet |
| A run's signed requests ([ADR 0068](decisions/0068-adam-signs-its-own-history.md), [0086](decisions/0086-a-run-signs-its-model-requests-with-a-certificate.md)) | turns and heads are signed; the run certificate has its ports, a fake issuer and a suite, and no real issuer. The request signature follows the contract as first written, not as amended; `signed-turns/v1` is not built | the amended contract's vectors pass | [#445](https://github.com/vymalo/another-adam-rs/issues/445) |
| A subagent in the background, as a session ([ADR 0045](decisions/0045-a-subagent-can-run-in-the-background-as-a-session.md)) | 7 of 8; the coder's kept-open helpers are built in part | the coder keeps a helper open across rounds | no card yet |
| Hooks ([ADR 0050](decisions/0050-hooks-around-the-agent-loop-compiled-first-and-programs-second.md)) | the library's hooks, `plugins:`, the `audit` plugin and the program adapter are built, and so are the deployment's hooks file and switch; no shipped binary registers a plugin | a plugin the coder registers, and a program that vetoes `git push` on the dev stack | no card yet |
| A repository's own environment ([ADR 0052](decisions/0052-no-podman-a-repositorys-own-environment-is-a-run-pod-of-its-image.md)) | lifecycle commands, `containerEnv` and the pulled digest are not built | the missing steps and their tests | no card yet |
| Images in | built, off by default; the provider's wire shape is unverified | a real provider's answer | no card yet |
| The memory ([ADR 0081](decisions/0081-an-agent-keeps-a-self-improvement-memory-near-it-and-what-holds-becomes-the-deployments-memory.md)) | the ports, the in-folder adapter, the in-process adapter and the two tools are built; no database adapter, no promotion, no defaults in the agents | the paired study in the bench | ANR-1292, ANR-1293, ANR-1294 |
| The engine (`adam-engine`) | a chat agent on SQLite; phones are type-checked only; no MCP servers, no remote subagents, no access tier | an application on a phone | [#275](https://github.com/vymalo/another-adam-rs/issues/275), [#277](https://github.com/vymalo/another-adam-rs/issues/277), [#278](https://github.com/vymalo/another-adam-rs/issues/278) |
| Parity gaps between surfaces | `uploads/v1` on no card, no access tier on the folder agent, reasoning effort not one setting everywhere | the [parity table](reference/parity.md) with no gap | [#274](https://github.com/vymalo/another-adam-rs/issues/274), [#282](https://github.com/vymalo/another-adam-rs/issues/282), [#284](https://github.com/vymalo/another-adam-rs/issues/284) |
| Known defects | the model client drops a tool call's `extra_content` (every second Gemini call is a 400); the extra body's thinking members are not merged with the typed level; the folder-agent skill omits the reasoning settings; a rustdoc link does not resolve | each issue closed | [#322](https://github.com/vymalo/another-adam-rs/issues/322), [#330](https://github.com/vymalo/another-adam-rs/issues/330), [#318](https://github.com/vymalo/another-adam-rs/issues/318), [#249](https://github.com/vymalo/another-adam-rs/issues/249) |
| Known gaps in the docs | a pinned run whose worker never returns is stranded; subagent calls of one turn run one after the other; a cancel does not reach a remote A2A task a run started | see [Child runs](reference/child-runs.md) | no card yet |

## Needs more time

| What | Why it is big |
|---|---|
| The first live run of the coder | It needs a real model, a real GitHub and a real cluster at once. Each has its own unknowns: the provider's wire shapes, a GitHub App on every account, the CNI that enforces the network policy. It also waits on the platform's operator and the orchestrator, which are other repositories. |
| A coder safe for many people (#119, #121, #122, #123) | Four linked pieces: no key in a run pod, the person's identity, a mirror per person and a cache that a run cannot poison. The identity piece depends on the orchestrator and on a decision still open ([ADR 0072](decisions/0072-the-coder-verifies-who-the-person-is-and-asks-github-before-it-touches-a-repository.md) is proposed). |
| Running the coder at scale (#127, the store service, replication) | It needs a database that is sized, backed up and pruned, and a load test that names the real limit before the store service is worth building. |
| Moving the SQL rules into the database (ADR 0082) | Seven slices over two stores, each with a schema version step that is a one-way door for a deployed store. |
| The memory (ADR 0081) | Eleven slices, three database adapters, a promotion rule and a paired study that needs a real model to say anything. |
| Signed runs with a real issuer (ADR 0068, 0086) | It needs a certificate authority behind the issuer port and an agreement with the verifier on the contract's exact form (#445). |
| The run's observability (ADR 0075) | A new pipeline that must say nothing about a person; the redaction rules are the work. |

## Next

1. Get the first release out: let the queued `v0.7.0` run finish, read its log, and fix what it stops on. Then check an anonymous pull of the image and the chart.
2. Run the Homebrew path once for real, and run CI on what merged without it (#495, #530).
3. Follow the amended request-signature contract ([#445](https://github.com/vymalo/another-adam-rs/issues/445)): a device's signature is refused today.
4. Run the coder once against a real model and a real GitHub ([#102](https://github.com/vymalo/another-adam-rs/issues/102)), and the bench with it.
5. Start the coder's safety for many people with [#119](https://github.com/vymalo/another-adam-rs/issues/119), then [#121](https://github.com/vymalo/another-adam-rs/issues/121).

## How this page is kept

It is updated in the same pull request as any change that moves a row: a slice built, a proof run, a gap found.
A row moves down only with its proof named. The README's feature table and this page must agree; when they do not,
this page is the one to fix first. The evidence behind each row, and how to check it, is on
[the agent page](../.agents/docs/roadmap.md).
