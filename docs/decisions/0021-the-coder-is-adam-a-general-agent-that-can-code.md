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
   `delegate_to_opencode` step (its children keep the icons of their ACP kinds). The label stays "OpenCode".

## Consequences

* The card's skills grow from one to four; the first, `coding-task`, keeps its id.
* `opencode` is a new word of the `steps/v1` icon vocabulary. A consumer that does not know it drops the icon and shows the
  step without one (the contract: a screen ignores an icon it does not know). Consumers must add the word to their
  vocabulary, and `another-agentic-system` will (that it sanitises icons to its own list is *unverified*: another
  repository). `StepIcon` is `#[non_exhaustive]`, so the new variant breaks no match outside this repository.
* The prompt snapshot (`bin/adam-coder/tests/fixtures/agent/prompt.txt`) and the card golden change with the files; the
  greeting mock (`dev/wiremock/mock-openai`) answers "What can I help with?".
* **Deferred: two read-only subagents, `explorer` and `reviewer`.** The owner asked for them as files of the agent folder.
  They are not shipped: the coder's tools find the worktree by the run's id (`ToolEnv::slot` and `ToolEnv::session` use
  `ToolCtx::run_id()`, and `Workspaces::run` is `<root>/workspaces/<run>`), and a subagent is a child run with an id of its own
  (`child_run_id`), so `read_file` and `run_command` in a subagent would answer "there is no workspace yet" for a worktree that
  exists. Nothing in a `ToolCtx` names the parent run. The two subagents need the tools to resolve the root run first
  (`ToolCtx` exposing it, or the workspace being shared by the child's parent), a change to `adam-runtime`.
