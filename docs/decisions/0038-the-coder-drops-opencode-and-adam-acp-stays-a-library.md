# 0038. The coder drops OpenCode, and adam-acp stays a library

Status: **Accepted** (2026-10-11), the owner's decision after the OpenCode study ([ADR 0037](0037-parallel-subagent-calls-and-an-editing-worker.md),
*Alternatives*): *"Let's remove OpenCode. But keep the ACP, we'll reuse it in the future."* Corrected the same day: *the
OpenCode CLI stays in the images, and so do Claude Code and Codex.* Nothing of it is built yet: the slices at the end are the
order of the removal.
Amends [ADR 0007](0007-progress-as-steps-and-streamed-text.md) (decision 7), [ADR 0010](0010-a-run-works-in-its-repositorys-devcontainer.md)
(decision 6), [ADR 0018](0018-extra-mcp-servers-are-a-file-merged-over-the-agents-own.md) (decision 4),
[ADR 0019](0019-a-runs-processes-in-a-pod-of-their-own.md) (decision 4, OD-K5), [ADR 0020](0020-reasoning-is-streamed-beside-the-answer-and-never-stored.md)
(consequences), [ADR 0021](0021-the-coder-is-adam-a-general-agent-that-can-code.md) (decision 6),
[ADR 0027](0027-every-tool-has-a-title-for-its-step.md) (decision 6), [ADR 0032](0032-usage-per-model-call.md) (consequences),
[ADR 0036](0036-the-coder-keeps-to-the-access-tier-the-person-picks.md) (decision 5) and
[ADR 0037](0037-parallel-subagent-calls-and-an-editing-worker.md) (the owner's answer 4).

## Context

OpenCode came before the decision records: `adam-acp` in #13 (`477f47d`, 2026-09-29) and the coder that delegates to it in #17
(`1856fe3`, the same day). ADRs 0007, 0010, 0019, 0021 and 0027 built on it. The study of 2026-10-11 (OpenCode v1.18.33 and
1.19.0, recorded in ADR 0037 by `5ee2ddf`) found the following:

* A delegation is one journaled step: a crash runs all of it again.
* Over ACP, OpenCode's subagents and their tokens are invisible.
* OpenCode loads the repository's own `.opencode/` plugins and config.
* OpenCode serves an unauthenticated loopback HTTP server.
* OpenCode skips a plugin that fails to load.
* Only the environment's `exec` reaches OpenCode.

[ADR 0037](0037-parallel-subagent-calls-and-an-editing-worker.md)'s workers do the same work in the open: journaled, a step per
call, tokens counted, the tier checked at every call. The owner chose to stop using OpenCode rather than to harden it or bridge
it.

## Decision

1. **The coder no longer uses OpenCode.** These go:
   * `delegate_to_opencode`;
   * the version check;
   * OpenCode's launch and inline configuration;
   * the variables `OPENCODE_MODEL`, `OPENCODE_COMMAND` and `OPENCODE_BINARY`;
   * the adam-side plumbing that brings an `opencode` binary into run environments (`/opt/adam/bin` in run pods and
     devcontainers);
   * the model key that environments received only for OpenCode.

   A removed variable that is still set is not an error. Nothing is less safe without it. The coder logs once, at startup,
   that it ignores the variable.
2. **What replaces each use:**

   | Use | Now |
   |---|---|
   | Broad, multi-file changes | the coder's own file tools (`read_file`, `write_file`, `edit_file`, `apply_patch`); once built, [ADR 0037](0037-parallel-subagent-calls-and-an-editing-worker.md)'s `worker` subagents, side by side |
   | Installing a project's dependencies (`checks.rs` advises it for a missing tool) | `run`, which keeps what a command makes |
   | Fixing a red check | the file tools, after reading the check's output |
   | "Split a change across OpenCode's subagents" (the prompt) | nothing until the workers exist; `explorer` and `reviewer` stay as they are |
   | The step tree under `Hand to OpenCode` | the coder's own tool steps, one per call, as for every other tool |
   | The `opencode` step icon | no longer emitted (decision 3) |
   | `OPENCODE_MODEL`, the second model alias | none: the coder's calls use `MODEL` and subagents' `model:` aliases |
3. **The `opencode` icon stays in the vocabulary.** It keeps its place in `StepIcon::OpenCode` (`crates/adam-runtime/src/step.rs:149-151`),
   in adam-ui's icon list (`crates/adam-ui/src/ask.rs:73`, `catalog-v5.json`) and in `#[tool(icon = ...)]`. `steps/v1` and the
   catalog are shared with the orchestration layer, and a word dropped from a closed vocabulary breaks the consumers that list
   it. The coder just stops sending it.
4. **No secret and no adam-provided agent binary enters a run's environment.**
   * Devcontainers lose the file `/run/adam/secrets/model-key`, and the tools directory keeps only `adam-exec`.
   * Run pods lose `MODEL_API_KEY` and the init container's copy of `opencode`.
   * The run pods' admission policy, which let a run pod read exactly one Secret for that key, then lets it read none.
   * `runPods.enabled` no longer needs `externalSecrets.enabled` for this reason (`deploy/coder/templates/_validate.tpl:193`).
   * `EnvSession::tool_path` and `EnvSession::secret_ref` leave the `adam-workspace` environment port. Their only use was
     OpenCode. This is a breaking change of a trait (Rule 2): the PR and the crate README say so. The three environments and
     the test doubles drop them. An ACP agent run inside an environment again would bring them back by an ADR.
5. **`MODEL_API_KEY` leaves `NEVER_HIDDEN`** (`bin/adam-coder/src/mcp_secrets.rs:20-24`). It was there because "the coder's
   OpenCode reads it". A `${MODEL_API_KEY}` in an `mcp.json` is now hidden from the run's processes and redacted, like every
   other secret a file names. Commands already never see it (`bin/adam-coder/src/tools/shell.rs:28`).
6. **`adam-acp` stays, as a library with its tests.** After the removal no crate depends on it: `bin/adam-coder` is its only
   dependent today (`bin/adam-coder/Cargo.toml:25`). It stays built and tested anyway:
   * It is a member of the workspace (`Cargo.toml:5`, `members = ["crates/*", "bin/*"]`) and of `[workspace.dependencies]`
     (`Cargo.toml:22`). So `cargo clippy --workspace`, the MSRV job and `cargo nextest run --workspace` build and test it.
   * `tests/fake_agent.rs` always runs against its scripted `adam-acp-fake-agent`.
   * `tests/live_opencode.rs` stays opt-in (`ADAM_TEST_OPENCODE=1`), and the nightly job `live-opencode` keeps running it
     (`.github/workflows/nightly.yml:60-94`).

   `AcpCommand::opencode` and `with_config_content` stay: they are the library's, not the coder's. The README's note on
   OpenCode's behaviour cites `bin/adam-coder/src/opencode.rs`, which goes. It is rewritten from the study's verified facts,
   which come from OpenCode's source.
7. **The CLIs stay in the images.** `vymalo/another-agentic-images` keeps OpenCode, Claude Code and Codex in both `workspace`
   and `agent-canvas`, and nothing changes there. The coder's image is built on `workspace` (`docker/coder/Dockerfile:68`), so
   the three CLIs stay on `PATH` in the coder's container, in the default devcontainer and in run pods. They are there for
   people and for repositories that want them. The coder starts none of them, configures none and gives none a key.

## What the removal changes

Paths as of `5ee2ddf` (`git grep -il opencode` over `crates`, `bin`, `docker`, `deploy`, `dev`, `docs`, `.agents`, `tools`,
`.github`, `compose.yaml`, `README.md`).

| Area | Goes | Changes |
|---|---|---|
| The coder's code | `bin/adam-coder/src/opencode.rs`, `src/tools/delegate.rs`, the dependency on `adam-acp` (`Cargo.toml:25`) | `src/tools/mod.rs` (`CoderSettings::opencode`, the tool list, the note on `DEVCONTAINER_NETWORK=none`); `src/tools/notes.rs:165-180` (`EnvironmentNotes::opencode`, `OpenCodeCheck`: old notes still load, the struct has no `deny_unknown_fields`); `src/tools/environment.rs:56`; `src/tier.rs:106,271`; `src/config.rs` (the three variables, the `OPENCODE_BINARY` check at 536-593, the settings' `model_key`); `src/serve.rs`; `src/mcp_secrets.rs`; the wording in `src/tools/{checks,files,inspect,make,publish,scratch}.rs`, `src/{lib,app,agent,redact}.rs` |
| The prompt | the passages of `bin/adam-coder/agent/instructions.md` at lines 79, 183, 221-222, 266-268, 319-322, 329-330, 385-386, 403-404, 425 and 451 | the card's skill description (line 62); the advice on installs points to `run` |
| The coder's tests | `tests/fixtures/tool-specs/delegate_to_opencode.json`; the delegation cases of `tests/{binary,e2e,environment,tools}.rs`, which drive `adam-acp-fake-agent` through `OPENCODE_COMMAND` | `tests/{agent_files,tiers,tool_specs}.rs`, `tests/common/mod.rs`, `tests/fixtures/agent/{prompt.txt,card.json}`, the other `tool-specs/*.json` whose descriptions name OpenCode |
| The environment port | `EnvSession::tool_path`, `secret_ref`, `SecretRef` (`crates/adam-workspace/src/environment.rs:372` and its tests) | `crates/adam-workspace` README and comments (`git_env.rs`, `workspace.rs`) |
| Devcontainers | `Settings::opencode`, `Settings::model_key`, the copy of `opencode` and the version it names (`crates/adam-devcontainer/src/tools.rs`), `session.rs:227-230`, the model-key file (`environment.rs`) | `lib.rs`, `state.rs`, `tests/environment.rs`, README |
| Run pods | `Settings::opencode`, `model_key`, `session.rs:205-207`, the fake's case (`fake.rs:1016-1035`) | `settings.rs`, `lib.rs`, `environment.rs`, README |
| The chart | the init container's copy of `opencode` (`deploy/coder/templates/_runpods.tpl:130-132`), `MODEL_API_KEY` in run pods (`_runpods.tpl:161-166`), `OPENCODE_MODEL` (`statefulset.yaml:159-160`), `config.opencodeModel` (`values.yaml:68-70`) | the admission policy (no Secret), `_validate.tpl:193`, `values.yaml` comments, the goldens (`tests/golden/{a2a-split,combined,run-pods}.yaml`), `tests/render-check.sh`, `deploy/coder/README.md` |
| The image | the link into `/opt/adam/bin` and its ELF checks (`docker/coder/Dockerfile:94-101,136-149`) | `OPENCODE_DISABLE_AUTOUPDATE` stays where the base image sets it; `docker/coder/test/{container-smoke.sh,run-pod.Dockerfile}` |
| The operator | `AgentConfig` `spec.coder.opencodeModel` (`crates/adam-operator-api/src/config.rs:197-199`), `OPENCODE_MODEL` in the contract (`crates/adam-operator-domain/src/contract.rs:38-39,103`) and in `resolve.rs:553-554` | the domain and runtime goldens, `deploy/crds/agents.vymalo.com.yaml` and its copy in `deploy/operator-crds/files/` (regenerated), `deploy/operator/examples/coder.yaml`, `deploy/operator/tests/e2e/coder.yaml`, `tools/adam-operator-parity/cases/*.yaml` and the parity goldens (`regen.sh`), `bin/adam-operator/tests/cluster.rs` |
| The dev stack | `dev/wiremock/mock-openai/mappings/opencode-script.json`, `__files/opencode-{bash,devbox-bash,devbox-done,done}.sse` | the `delegate_to_opencode` turns and `[mock:no-opencode]` of `coder-script.json` and `coder-script-stream.json`; `compose.yaml` (`OPENCODE_MODEL`, `mock-opencode`); `dev/compose.devcontainer.yaml` (the model a devcontainer reached for OpenCode); `dev/coder-e2e.sh` (`NO_OPENCODE` and the delegation scenario); `dev/coder-choices-e2e.sh`, `dev/git-server/seed/local/devbox/README.md`; `crates/adam-model-openai/tests/wiremock_compose.rs:360-410,545-600` |
| CI | | `.github/workflows/coder.yml` (`NO_OPENCODE=1` and the comments); `nightly.yml` keeps `live-opencode` (decision 6) |
| Docs | | `README.md`, `docs/architecture.md`, `docs/reference/{coder-agent,environment,workspace-and-environments,dev-stack,a2a-server}.md`, `docs/guides/{deploy-the-coder,run-locally,testing}.md`, the READMEs of `bin/adam-coder`, `bin/adam-agent`, `adam-acp`, `adam-devcontainer`, `adam-env-kubernetes`, `adam-workspace` |
| Skills | | `adam-coder-deploy` (the init container) and `adam-a2a-extensions` (OpenCode's unreported tokens); the vendored skills that mention OpenCode are not ours to edit |
| Examples only | | the label "OpenCode" in examples and tests of shared crates becomes a neutral one: `crates/adam-llm-agent` (README, `src/tool.rs:77`, `tests/llm_agent.rs`), `crates/adam` (README, `tests/tool_macro.rs`, `tests/ui/pass/options.rs`), `crates/adam-macros/src/{lib,expand}.rs`, `crates/adam-a2a-runtime/tests/steps.rs` |

Unchanged on purpose:
* `docs/reference/agent-files.md` and `crates/adam-agent-fs`, which read OpenCode's agent-file *format* (a different matter).
* The `opencode` icon (decision 3).
* `adam-acp` (decision 6).

## Alternatives considered

* **Keep OpenCode behind a setting, decided after a trial** (ADR 0037, the owner's answer 4). The owner decided now. The trial
  would have measured OpenCode against workers that do not exist yet.
* **Harden the present path and bridge it with a one-way plugin** (the study's recommendation). This is effort spent on a path
  the owner is closing.
* **Remove `adam-acp` too.** The owner wants ACP for later. A tested client is cheaper to keep than to write again.
* **Drop the `opencode` icon.** It would break a shared, closed vocabulary to remove a word nobody is obliged to send.
* **Keep `tool_path` and `secret_ref` as unused trait methods.** That would be dead API whose only purpose was to put a key
  and a binary into environments. A removal now, said under Rule 2, is cheaper than an unused path that nobody tests.

## Consequences

* **The prompt.** The rendered prompt is 29,820 characters (`bin/adam-coder/tests/fixtures/agent/prompt.txt`). The loader warns
  above 30,000 (`crates/adam-agent-fs/src/load/agent.rs:15,153-163`). The passages above free about 1,000 characters, after
  the `run` wording. This is an estimate from the lines listed. The prompt must stay under 30,000, and ADR 0037's worker
  section can use the room.
* **Broad changes** are made call by call with the file tools until the workers land. Each edit is a journaled step, and a
  crash resumes after the last recorded call.
* **The coder's token totals are complete.** The gap of [ADR 0032](0032-usage-per-model-call.md) (OpenCode's calls in no
  total) closes.
* **Less reaches a run's environment:**
  * no model key in devcontainers or run pods;
  * no Secret a run pod may read;
  * no `.opencode/` of a repository executed by the coder;
  * no OpenCode loopback server.

  With `DEVCONTAINER_NETWORK=none`, the coder loses no tool: it was OpenCode that needed the model inside.
* **The CLIs remain on the environment's `PATH`** for people and for repositories that want them (decision 7). Without a key or
  a configuration from the coder, a model's command that starts one cannot reach the gateway with the coder's credentials.
* **A run in flight at the upgrade.**
  * A history with `delegate_to_opencode` calls replays as data.
  * A call still owed gets the loop's answer for an unknown tool, which lists the tools there are
    (`crates/adam-llm-agent/src/agent.rs:1508-1511`). The model goes on with the file tools.
  * Old run notes with `environment.opencode` load, and the key is dropped on the next save.
* **The operator's CRD loses a field** of `v1alpha1`. Whether the API server prunes `opencodeModel` from a stored object, and
  warns on apply, is *unverified* here. The operator's kind e2e is what proves an `AgentConfig` that still sets it.
* **The orchestration layer vendors these mocks.** `vymalo/another-agentic-system` copies them into `dev/coder/` (the
  `opencode-*.sse` files, `opencode-script.json`, the coder scripts) and asserts the OpenCode step in `dev/coder-e2e.sh` and
  `dev/devcontainer-e2e.sh`. *Verified* by `git grep` there, 2026-10-11. Its next `bump-adam` drops them.
* **Rule 2:** `EnvSession` loses two methods (decision 4). A consumer that implements it stops compiling until it removes
  them.

## Slices

One pull request, one commit per slice, each green alone, in this order (a later slice removes what an earlier one stopped
using).

1. **The coder.** The tool, the module, the variables (with the startup warning), the notes, the tiers, the prompt (under
   30,000, with a test of the count), the fixtures and tests, the README. `adam-acp` stays in the workspace and its tests run.
2. **The environments.** `tool_path` and `secret_ref` go from the port, the three environments and the doubles. The
   devcontainer loses the tools copy and the key file. Run pods lose the key and the copy, the admission policy allows no
   Secret, `_validate.tpl` drops the requirement. Then the image's link and smoke checks, the goldens, and the crate and chart
   READMEs.
3. **The operator.** The CRD field, the contract and `resolve`, the regenerated CRD in both places, the examples, the e2e, the
   parity cases and goldens.
4. **The dev stack and CI.** The mocks and scripts, `compose.yaml`, `dev/compose.devcontainer.yaml`, `coder.yml`, and the
   wiremock test of `adam-model-openai`. Every `dev/*-e2e.sh` passes without OpenCode.
5. **Docs and skills.** The pages in the table, `adam-coder-deploy`, `adam-a2a-extensions`, `adam-acp`'s README note, and the
   neutral labels in the shared crates' examples. `node tools/docs-check/check-docs.mjs` passes.
