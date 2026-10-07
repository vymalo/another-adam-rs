# 0021. The coder is Adam, a general agent that can code

Status: **Accepted** (2026-10-06), on the owner's decision; the owner may revisit.
Builds on [ADR 0007](0007-progress-as-steps-and-streamed-text.md) (steps and their closed icon vocabulary) and
[ADR 0012](0012-files-as-a2a-artifacts.md) (files shared as artifacts).

## Context

The owner, 2026-10-06: "The system prompt of the actual coder looks too strict. It's as if the guy cannot do anything than
coding. It's coder, but just because it can code. That's why I want us to change that name to something else." The prompt
opened with "You are a coding agent" and asked every greeting for a repository, so a person who wanted an answer, a report
or a plan was steered into a pull request. The owner chose the name **Adam**.

## Decision

1. **The visible name is Adam.** `vars.display_name`, the card's `name` and `description`, the prompt and the docs that
   describe the agent say Adam. The agent id stays `coder` (`AGENT_NAME`, which the stored runs carry).
2. **Four abilities beside the code change**, each a card skill and a rule of the prompt:
   * **answer and explain** (`explain`): a question about code, a repository or a concept, with no change and no pull
     request; a repository the person named is read with `run_command` and `read_file` to ground the answer;
   * **research** (`research`): web search and reading pages through the MCP servers the deployment gives it; with none it
     says so and never claims to have searched;
   * **documents and files** (`documents`): a report, a plan, a chart or an export made in a scratch project and shared with
     `share_file`, with no repository;
   * **plan, then ask**: a vague or large request gets a short plan that the person confirms before Adam acts.
3. **Code to a pull request stays the strongest path** (`coding-task`) and keeps every gate: the check limits, "no pull
   request while the last check failed", "the code in the pull request is the code the checks passed on". Adam never
   opens a pull request the person did not ask for.
4. **A greeting asks what Adam can help with**, not which repository. The mock model's greeting and the scripts that check
   it follow.
5. **Technical names do not change now**: the binary `adam-coder`, the crate, the image
   `ghcr.io/vymalo/another-adam-rs/coder`, the chart `deploy/coder` and the environment variables move later, in one
   release, so that consumers change their pins once.
6. **OpenCode's step has its own icon.** `StepIcon::OpenCode`, on the wire `"opencode"`, is the icon of the
   `delegate_to_opencode` step (its children keep the icons of their ACP kinds). The label stays "OpenCode". *Amended 2026-10-07:* it is "Hand to OpenCode", like every other tool's step ([ADR 0027](0027-every-tool-has-a-title-for-its-step.md)).

## Consequences

* The card's skills grow from one to four; the first, `coding-task`, keeps its id.
* `opencode` is a new word of the `steps/v1` icon vocabulary. A consumer that does not know it drops the icon and shows the
  step without one (the contract: a screen ignores an icon it does not know). Consumers must add the word to their
  vocabulary, and `another-agentic-system` will (that it sanitises icons to its own list is *unverified*: another
  repository). `StepIcon` is `#[non_exhaustive]`, so the new variant breaks no match outside this repository.
* When the model stops with nothing to say before any workspace exists, the question the run asks is "What can I help with?", as the greeting.
* The prompt snapshot (`bin/adam-coder/tests/fixtures/agent/prompt.txt`) and the card golden change with the files; the
  greeting mock (`dev/wiremock/mock-openai`) answers "What can I help with?".
* **Two read-only subagents, `explorer` and `reviewer`**, are files of Adam's folder
  (`bin/adam-coder/agent/subagents/`), with the tools `read_file` and `run_command` and nothing else: no write, no
  checks, no delegation, no publishing, no asking. The prompt says when to use them (a large or unfamiliar repository,
  and once before `open_pull_request`); their findings are advice and the gates still decide.

## Amended 2026-10-06: a subagent works for its root run

The first version of this decision left the two subagents out: the coder's tools found the worktree by the id of the
run that called them (`ToolCtx::run_id()`), a subagent is a child run with an id of its own, and nothing in a
`ToolCtx` named the parent, so `read_file` in a subagent answered "there is no workspace yet" for a worktree that
existed. Now:

* **`ToolCtx::root_run_id()`** (`adam-llm-agent`) is the top of the chain of parents, and the run itself for a run that
  is nobody's child. `ToolCtx::start_child` puts the caller's root in the child's first message
  (`root_run`), and the child keeps it in its stored state (`Conversation::root_run`). A restart or another worker reads
  the same id, no store is read per call, a child of a child gets the top run and not its parent, and a run that
  continues another does not inherit it. Only code in the process writes the key: the A2A front builds payloads of
  `text` and `context`, so a client cannot name a run it does not own. The API is additive (no required trait method).
* **The coder keys by the root** the workspace and the environment (`ToolEnv::slot`, `ToolEnv::session`, `prepare_workspace`,
  `start_scratch`, `rebuild_environment`) and every run note that is a gate or a budget (the check cycles, the
  repositories the person named and agreed to, the created repositories, the credentials blocker, OpenCode's version
  check), so a subagent gets no fresh budget and cannot pass a gate its root has not passed. The two places that stay on
  the run itself are the steps the environment shows (`env:<run>:...`: a step belongs to the run that emits it) and
  `share_file`'s "delivered" note and 6 MiB budget, because a file shared by a subagent stays on the subagent's run and
  never reaches the person, so it delivers nothing for the root. Every call id kept in the root's notes (a missing tool,
  a check's record and the replay list of counted checks) is made unique per run for a subagent (`ToolEnv::call_key`):
  model call ids are unique only within the run that made them.
