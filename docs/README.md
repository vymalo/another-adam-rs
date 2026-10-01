# adam-rs docs

| Document | What it answers |
|---|---|
| [Architecture](architecture.md) | How the 22 crates fit together, which traits are the swappable boundaries, what happens to a task from A2A request to pull request, how a run moves through its states, how errors decide behaviour, and how the coder agent is built and deployed. Every process has a Mermaid diagram. |
| [The authoring layer](authoring.md) | The design of agents as Markdown files (eve's `agent/` layout, Claude Code / Copilot custom-agent files, Agent Skills, `mcp.json`) plus `#[tool]` for tools: formats, the tool contract, skills and subagents as durable child runs, `build.rs` versus dev mode, the crate layout, the accepted decisions D1 to D6 and the mapping eve to adam-rs to standard. Slices S1 to S8 are built (macro, parser, codegen, binding, skills at run time, child runs in the runtime); the subagent tool is designed and marked planned. Mermaid pairs for each process. |
| [Decisions](decisions/) | Architecture decision records. [0001](decisions/0001-library-first-host-roles.md): adam-rs is library-first; hosts run a control plane and workers through the closed `Role` enum and supervisor in `adam-host`. [0002](decisions/0002-workspace-placement.md): the closed `Placement` enum (`shared`, `affinity`, `isolated`, `a2a-only`), pinned claims over a run owner, and a cross-process mirror lock. [0003](decisions/0003-a-new-task-continues-the-task-it-references.md): a new A2A task that references a finished one (`referenceTaskIds`, same caller, same context) starts from its conversation, through `init_continuing`, with the carried history capped. [0004](decisions/0004-agent-folders-at-run-time.md): a binary reads its agent folder (`ADAM_AGENT_DIR`) once, at startup, and the coder falls back to the copy embedded in its binary; no hot reload in release images, a restart is a deploy. [0005](decisions/0005-one-binary-serves-any-agent-folder.md): `adam-agent` is one binary that serves any agent folder (no embedded default, one agent per process, `ask_user` and the folder's MCP tools only), over `adam-service`, shipped inside the coder image. [0006](decisions/0006-a2ui-and-the-vymalo-extensions-in-adam-rs.md): a question can carry an A2UI interface, a message's extensions are the run's inbound context (the screen's UI catalog, the thread-tools grant kept until it expires), tools can come from a `ToolSource` read at every model turn, and `adam-ui` turns the screen's catalog into `ask_user` with Choices, `show` and `ui_catalog`. [0007](decisions/0007-progress-as-steps-and-streamed-text.md): a request's activated extensions reach the backend, a tool call and the work it drives are `Step` events (the orchestration layer's `steps/v1`, a line of text for a client that did not activate it), and the model's answer is streamed as it is written (issue #51: the `text-stream/v1` extension, `RunEvent::TextDelta`, the streamed model step in the journal, only the final text stored). [0008](decisions/0008-a-workspace-holds-several-repositories.md): a run's workspace is a directory of slots, each a repository's worktree or a scratch project (a local repository with an empty root commit, kept only until the run ends and copied into a repository all or nothing), a repository has one slot per run, the workspace is deleted with the run, an empty remote gets an empty first commit, and which repository may enter is the coder's rule. [0009](decisions/0009-github-per-installation-read-through-mcp.md): the coder is authenticated per installation, by a token or a GitHub App (`GitHubApp` signs a JWT with the App's key, parsed at startup, and trades it for an installation token it keeps until five minutes before it expires, behind `HostScoped`; every token it mints is redacted), its own tools stay in process, GitHub is read through the official GitHub MCP server (read-only, built: the coder image carries a pinned `github-mcp-server`, the shipped `mcp.json` lists twelve read tools, and its flags, variables and behaviour are verified against v1.12.2) and a repository is created only for an allow-listed owner after the person agrees (planned). |

Other places to look:

* [Root README](../README.md): the durable model (runs, journal, leases), how
  each store adapter keeps its promises, the local Compose stack, the error
  table, testing.
* One `README.md` per crate, linked from
  [Where to go next](architecture.md#where-to-go-next).
* [`deploy/coder/README.md`](../deploy/coder/README.md): the Helm chart of the
  coder agent.

## Writing docs

* A process gets a Mermaid pair: a `sequenceDiagram` for the interaction and a
  `stateDiagram-v2` for the lifecycle. Prose says what the diagrams cannot.
* Every node, edge, state and call in a diagram must exist in the code. Cite
  the file, for example `crates/adam-runtime/src/runtime.rs`.
* Mark third-party claims *verified* (with date and source) or *unverified*.
* Do not copy a crate README. Link to it.
* `node tools/docs-check/check-docs.mjs` parses every diagram and resolves every
  relative link and `#heading`. CI runs it in the `docs` job. Install once with
  `npm --prefix tools/docs-check ci`.
