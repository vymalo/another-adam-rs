# adam-rs docs

adam-rs is a Rust framework for **durable AI agents**: an agent saves its state after every step, so a worker that
dies loses nothing. Start with the [root README](../README.md), then pick the line below that matches what you want to
do. Every crate also has a `README.md` next to its `Cargo.toml`.

These pages are for people. The procedures and evidence that coding agents use live in
[`.agents/docs/`](../.agents/docs/README.md), and each page that has a counterpart there links to it
([ADR 0079](decisions/0079-docs-for-people-and-docs-for-agents-live-apart.md)).

## Get started

| Page | When |
|---|---|
| [Get started](getting-started.md) | you want one success in minutes: the stack up and a folder agent answering over A2A |
| [Try the agents](guides/try-the-agents.md) | you want to chat with the Assistant, give Adam a task and write your own agent, all on mocks, then on a real model |
| [Run it locally](guides/run-locally.md) | you want the coder and the general agent running on your machine in minutes |

## Guides

| Group | Page | When |
|---|---|---|
| Use | [Write an agent](guides/write-an-agent.md) | you want an agent that is a folder of Markdown, or one with `#[tool]` functions |
| Deploy | [Deploy the coder](guides/deploy-the-coder.md) | you are installing the image and the Helm chart on Kubernetes |
| Deploy | [Releases](guides/releases.md) | you want a fixed version for production: what a release is, how one is cut, which tag to pin, how to check its signature |
| Operate | [Upgrade an adam deployment](guides/upgrade-an-adam-deployment.md) | you move a running coder to a newer image: what to read, what needs your hand, what the database does |
| Operate | [The egress proxy and the network of run pods](guides/egress-and-run-pod-network.md) | a build in a run pod cannot reach a host, or you want to let it |
| Operate | [Read a run's state when it is stuck](guides/read-a-stuck-run.md) | a task does not finish: is it waiting, or is it stuck, and where to look in the store |
| Operate | [Troubleshoot by error class](guides/troubleshoot-by-error-class.md) | a pod exited or a task failed: from the class of the error to what to do |
| Operate | [Export runs for training](guides/export-runs-for-training.md) | you want adam's runs as training records (JSON Lines in the chat shape) to fine-tune a small model for one stack, and how to curate and evaluate it |
| Operate | [Troubleshooting a local run](guides/run-locally.md#troubleshooting), [the chart's known risks](guides/deploy-the-coder.md#known-risks) | something does not work on your machine, or you want to know what can go wrong in a cluster |
| Extend | [Embed adam](guides/embed-adam.md) | you host adam agents inside your own Rust program, a server |
| Extend | [Embed adam in an application](guides/embed-in-an-app.md) | adam runs inside your application (a desktop or a phone), with no server and nothing on the network of its own |
| Extend | [Store adapters](reference/store-adapters.md#adding-a-backend) | you add a database behind the `Store` trait |

## Reference

| Group | Page | What it answers |
|---|---|---|
| API | [The A2A server](reference/a2a-server.md) | the methods, follow-ups, continuing a finished task, push notifications, `ListTasks`, the extended card and its signature, the steps, text and reasoning streams, and the A2A extensions an agent serves |
| API | [Extensions](reference/extensions.md) | every A2A extension URI an adam card can list: when it is listed, whether it is served, and its contract on the system's site |
| Configuration | [Environment variables](reference/environment.md) | every variable of the binaries, the compose stack and the tests |
| Configuration | [Agent files and `#[tool]`](reference/agent-files.md) | the folder layout and formats, validation, skills, subagents, MCP, dev reload, the macro |
| Configuration | [The chart](../deploy/coder/README.md) | every value of the coder's Helm chart |
| Configuration | [Parity of the surfaces](reference/parity.md) | every setting and feature by surface (coder, folder agent, embedded engine, orchestrator, platform), and the gaps |
| Errors | [Errors](reference/errors.md) | error classes, the variant-to-class tree, A2A codes and exit codes |
| Rust API | [Rustdoc of the library crates](rust/adam/index.html){target="_self"} | the generated API of every library crate (published with the site); [the crate map](architecture.md#the-crate-map) says which crate does what, and each crate's README is its guide |
| Behaviour | [The coder agent](reference/coder-agent.md) | its flow and states, the rules its tools enforce, how a run ends |
| Behaviour | [Child runs and remote tasks](reference/child-runs.md) | how a subagent waits, and every failure interleaving |
| Behaviour | [Workspace and environments](reference/workspace-and-environments.md) | placement, a run's slots, devcontainers and run pods |
| Behaviour | [Store adapters](reference/store-adapters.md) | what each database adapter guarantees and how, data caveats |
| Local stack | [The local stack](reference/dev-stack.md) | compose services, mocks, scripted models |

## Architecture

| Page | What it answers |
|---|---|
| [Concepts](concepts.md) | the ideas in one screen (run, journal, lease, store, host role, agent folder, tool, A2A server) and a glossary of terms with their files |
| [Architecture](architecture.md) | the mental model, the crate map, the ports, the path of a task, the run lifecycle, the data schema, signals, errors |

## Decisions

[Decisions](decisions/README.md): every ADR, with its decision in one line. ADRs are history: a change to a past
decision is a dated *Amended* note, never a silent edit.

## Plans and reviews

| Page | What it is |
|---|---|
| [Roadmap](roadmap.md) | what the library has built and what it has not; the product roadmap is [`ROADMAP.md` in another-agentic-system](https://github.com/vymalo/another-agentic-system/blob/main/ROADMAP.md) |
| [Optimisation review, 2026-11-18](reviews/2026-11-18-optimisation.md) | where the framework's RAM, CPU and I/O go, and the ranked fixes |

## Contributing

| Page | When |
|---|---|
| [Contributing](../CONTRIBUTING.md) | you want to pick an issue, build, test and open a pull request |
| [Testing](guides/testing.md) | you want to run or add tests, the end-to-end scripts, or check the chart |
| [Security](../SECURITY.md) | you found a vulnerability: report it privately |
| [The benchmark harness](../bench/README.md) | you want to compare adam-coder and OpenCode on real tasks, with a hard cap on spend |

## Writing these docs

* Short pages, the answer first: tables and diagrams over prose, a link instead of a copy.
* A process gets a Mermaid pair: a `sequenceDiagram` for the interaction and a `stateDiagram-v2` for the lifecycle.
* A change that alters behaviour updates its page in the same pull request.
* Facts about third parties are marked *verified* (date and source) or *unverified*.
* Evidence, procedures and checks go on the agent page of the same path, not here.

Which page a change updates, and how the docs are checked: [writing docs](../.agents/docs/writing-docs.md).
