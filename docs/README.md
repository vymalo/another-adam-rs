# adam-rs docs

Start with the [root README](../README.md), then the guide for what you are doing. Every crate also has a
`README.md` next to its `Cargo.toml`.

## Guides: do a thing

| Guide | When |
|---|---|
| [Run it locally](guides/run-locally.md) | you want the coder and the general agent running on your machine in minutes |
| [Write an agent](guides/write-an-agent.md) | you want an agent that is a folder of Markdown, or one with `#[tool]` functions |
| [Deploy the coder](guides/deploy-the-coder.md) | you are installing the image and the Helm chart on Kubernetes |
| [Run agents with the operator](guides/run-agents-with-the-operator.md) | you run the coder or a folder agent on Kubernetes from `AgentService` and `AgentConfig` resources |
| [Embed adam](guides/embed-adam.md) | you host adam agents inside your own Rust program |
| [Testing](guides/testing.md) | you want to run or add tests, the end-to-end scripts, or check the chart |

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
| [0029](decisions/0029-adam-rs-has-an-operator.md) | adam-rs ships the Kubernetes operator of `AgentService` and `AgentConfig`, moved from the platform repository |
| [0030](decisions/0030-a2a-push-notifications-list-tasks-extended-card-signatures.md) | A2A push notifications, `ListTasks`, the extended card and card signatures |
| [0031](decisions/0031-swagger-ui-and-the-a2a-rest-binding.md) | the A2A HTTP+JSON binding beside JSON-RPC, and Swagger UI at `/docs` |

## Keeping these docs short and true

* Humans read the root `README.md` and `docs/`. Keep them short: tables and diagrams over prose, link instead of copying.
* A process gets a Mermaid pair: a `sequenceDiagram` for the interaction and a `stateDiagram-v2` for the lifecycle.
  Every node, edge, state and call must exist in the code; cite the file.
* A change that alters behaviour updates its guide, reference or architecture section in the same pull request.
* Mark facts about third parties *verified* (date and source) or *unverified*.
* `node tools/docs-check/check-docs.mjs` parses every diagram, resolves every relative link and `#heading`,
  requires a `README.md` per crate and checks the first-party skills (CI's `docs` job;
  `npm --prefix tools/docs-check ci` once).
