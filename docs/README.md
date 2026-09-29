# adam-rs docs

| Document | What it answers |
|---|---|
| [Architecture](architecture.md) | How the 16 crates fit together, which traits are the swappable boundaries, what happens to a task from A2A request to pull request, how a run moves through its states, how errors decide behaviour, and how the coder agent is built and deployed. Every process has a Mermaid diagram. |
| [Decisions](decisions/) | Architecture decision records. [0001](decisions/0001-library-first-host-roles.md): adam-rs is library-first; hosts run a control plane and workers through the closed `Role` enum and supervisor in `adam-host`. |

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
