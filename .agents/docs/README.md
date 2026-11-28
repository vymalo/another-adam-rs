# Docs for agents

For people: [`docs/`](../../docs/README.md). This tree holds what a coding agent needs to change adam-rs and check
the change: procedures, invariants to verify, evidence, and how to run the gates. It is not published. The convention
is [ADR 0079](../../docs/decisions/0079-docs-for-people-and-docs-for-agents-live-apart.md); the entry point is
[`CLAUDE.md`](../../CLAUDE.md).

A page here that has a page for people sits at the same relative path, and the two link to each other.

| Page | For people | What it holds |
|---|---|---|
| [Writing docs](writing-docs.md) | [Writing these docs](../../docs/README.md#writing-these-docs) | the split, which page a change updates, how to write a page for people, what `docs-check` checks |
| [Skills](skills.md) | none | the skills this repository provides, how consumers install them, how to write and keep one |
| [Concepts: the evidence](concepts.md) | [Concepts](../../docs/concepts.md) | where the code defines each term of the glossary |
| [Architecture: the evidence](architecture.md) | [Architecture](../../docs/architecture.md) | what was verified, when and how, and what is still unverified |
| [Releases: the procedure and the evidence](guides/releases.md) | [Releases](../../docs/guides/releases.md) | the files of the release flow, the invariants to check, how to read a release's digests and rehearse a release, and what stays unverified until a tag |
| [Upgrade an adam deployment: evidence](guides/upgrade-an-adam-deployment.md) | [Upgrade an adam deployment](../../docs/guides/upgrade-an-adam-deployment.md) | the commands to name A and B, the pin by digest, the migration code and the schema versions |
| [The egress proxy and run-pod network: evidence](guides/egress-and-run-pod-network.md) | [The egress proxy and the network of run pods](../../docs/guides/egress-and-run-pod-network.md) | the commands to read the proxy and its policies, and where the chart renders each rule |
| [Read a stuck run: queries](guides/read-a-stuck-run.md) | [Read a run's state when it is stuck](../../docs/guides/read-a-stuck-run.md) | the read-only queries on the run store and the code each claim stands on |
| [Embed adam in an application: the evidence](guides/embed-in-an-app.md) | [Embed adam in an application](../../docs/guides/embed-in-an-app.md) | where the engine's approvals are in the code, the invariants to check, and each proof with the change it was seen failing against |
| [Troubleshoot by error class: evidence](guides/troubleshoot-by-error-class.md) | [Troubleshoot by error class](../../docs/guides/troubleshoot-by-error-class.md) | where each class's retry, A2A code and exit code is decided |
| [Errors: rules to check](reference/errors.md) | [Errors](../../docs/reference/errors.md) | the rules every error enum keeps |
| [Agent files: the decisions to keep](reference/agent-files.md) | [Agent files](../../docs/reference/agent-files.md) | the owner's decisions behind the folder format |
| [Child runs: the tests](reference/child-runs.md) | [Child runs](../../docs/reference/child-runs.md) | the test of every failure interleaving, and how the tests make each happen |
| [Hook programs: the invariants](reference/hooks-protocol.md) | [Hook programs](../../docs/reference/hooks-protocol.md) | the invariants of the stdio adapter, the test behind each claim, and what is not built (P2, H1d) |
| [Observability: the emitted instruments and the dashboards](reference/observability.md) | none | the catalogue a dashboard panel is held to, the closed labels, how a metric or a dashboard is added, and the gates that fail on drift |
| [ADR 0081: the evidence](decisions/0081-an-agent-keeps-a-self-improvement-memory-near-it-and-what-holds-becomes-the-deployments-memory.md) | [ADR 0081](../../docs/decisions/0081-an-agent-keeps-a-self-improvement-memory-near-it-and-what-holds-becomes-the-deployments-memory.md) | the paper, the code the memory decision was checked against, the draft's values and the proof of each slice |
| [Roadmap: the evidence](roadmap.md) | [Roadmap](../../docs/roadmap.md) | how to re-check each row of the roadmap, and what it was checked against |
| [0082: the evidence](decisions/0082-the-sql-adapters-keep-their-rules-in-the-database.md) | [ADR 0082](../../docs/decisions/0082-the-sql-adapters-keep-their-rules-in-the-database.md) | the survey of every rule the SQL adapters enforce in Rust, with `path:line`, the sources of the facts, and each slice's decisive cases and mutations |

## Still mixed

Pages for people that still carry agent evidence, to split one at a time:

| Page | What is still in it |
|---|---|
| [`docs/reference/parity.md`](../../docs/reference/parity.md) | the `repo:path:line` citation of every cell, the pins and *How to keep parity*; it is listed by name in `docs-check` until split |
| the crate READMEs | file citations and test lists, for people who change or embed the crate; long ones (`bin/adam-coder`, `adam-llm-agent`, `adam-assembly`) mix guide, reference and evidence |
