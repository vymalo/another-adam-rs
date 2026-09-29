# adam-assembly

Bind an agent manifest to [`LlmAgent`](../adam-llm-agent/README.md)s. An agent written as files
(`agent/instructions.md`, subagents, skills, `mcp.json`; see [`docs/authoring.md`](../../docs/authoring.md))
is read into a manifest by [`adam-agent-fs`](../adam-agent-fs/README.md). This crate gives the
manifest its meaning at run time: the tools it names, the `{{placeholders}}` of its prompt, the model
it talks to and the state its tools need. Every mistake in that is found **when the process starts**,
with the agent and the file in the message, and never in the middle of a run.

## Where it sits

Slice S6 of the authoring layer, the meeting point of the "macro" track (`#[tool]`, `ToolSet`) and the
"files" track (`adam-agent-fs`).

```text
adam-agent-fs  (files -> manifest) ─┐
adam-llm-agent (ToolSet, LlmAgent) ─┼─> adam-assembly ─> adam (facade)
adam-model, adam-runtime ───────────┘        ▲
adam-a2a  (feature a2a) ─────────────────────┘
```

It depends on `adam-agent-fs`, `adam-llm-agent`, `adam-model`, `adam-runtime`, `adam-error` and
`thiserror`, and on `adam-a2a` and `url` behind the feature `a2a`. It has no `unsafe`, no I/O of its
own (the manifest is already in memory) and no async.

## Use

```rust
use adam::prelude::*; // AgentDef and friends are in the facade prelude

adam::include_agent!(); // AGENT: the agent build.rs embedded (a `&'static EmbeddedAgent`)

let assembly = AgentDef::from_manifest(AGENT)?    // or an `AgentManifest` a `Dir` loaded
    .var("repo", "acme/widgets")                   // a value for a `{{repo}}` placeholder
    .bind(tools![PrepareWorkspace, RunChecks])?    // tools: names checked, prompts rendered
    .state(Arc::new(env))                          // what the tools read with State<T>
    .model(model, "coder-large")?;                 // one client, and the default gateway alias

let runtime = assembly.register(Runtime::builder(store)).build(); // root and subagents
let card = assembly.card(public_url, env!("CARGO_PKG_VERSION"))?;   // feature a2a
```

The embedded agent and the directory read at run time are the same `AgentManifest`, so they take the
same code path here and bind to equal agents (`Assembly::info()` compares equal). The facade calls the
first `AgentDef::from_manifest(AGENT)`; note that `AGENT` is already a reference, so no `&`.

## API at a glance

| Item | What |
|---|---|
| `AgentDef` | `from_manifest(impl IntoManifest)`, `from_source(&impl ManifestSource, Strictness)` (one per agent of a package), `var(name, value)`, `agent_var(agent, name, value)`, `name()`, `manifest()`, `bind(ToolSet)` |
| `IntoManifest` | `AgentManifest`, `&AgentManifest`, `EmbeddedAgent` and `&EmbeddedAgent` (what `include_agent!` gives) |
| `BoundDef` | `state(Arc<T>)`, `model_aliases(..)`, `model(DynModel, alias)` |
| `Assembly` | `agents()`, `root()`, `register(RuntimeBuilder)`, `info()`, `remotes()`, `manifest()`, `card(url, version)` (feature `a2a`) |
| `AgentInfo` | `name` (`coder`, `coder/reviewer`), `parent`, `description`, `file`, `model_alias`, `prompt` (rendered), `tools`, `limits`: what an `LlmAgent` was made from, comparable |
| `RemoteInfo` | a remote (`a2a:`) subagent, as data |
| `Error`, `Origin` | the closed error enum, and the agent and file every file-related variant carries |
| `AliasProblem`, `TemplateProblem` | closed enums inside `Error::ModelAlias` and `Error::Template` |

## The stages

Each step is a type, so a step cannot be skipped: the state comes before the model because the model
step is the one that builds the agents and finds a missing state.

```mermaid
sequenceDiagram
  participant M as main.rs
  participant D as AgentDef
  participant B as BoundDef
  participant A as Assembly
  participant R as Runtime
  M->>D: from_manifest(AGENT)
  M->>D: var(name, value)
  M->>D: bind(tools)
  D->>D: resolve tools, render prompts, check vars
  D-->>M: Err(UnknownTool, UnknownVar, UnusedVar, ...) or BoundDef
  M->>B: state(env)
  M->>B: model(client, alias)
  B->>B: resolve model aliases, LlmAgent try_build for each agent
  B-->>M: Err(ModelAlias, Build) or Assembly
  M->>A: register(Runtime builder)
  A->>R: one agent per definition, root first
```

```mermaid
stateDiagram-v2
  [*] --> Defined: from_manifest
  Defined --> Defined: var, agent_var
  Defined --> Bound: bind, tools and vars are consistent
  Defined --> Refused: bind, a mistake in the files or the tools
  Bound --> Bound: state, model_aliases
  Bound --> Assembled: model, every agent built
  Bound --> Refused: model, a bad alias or a missing state
  Assembled --> [*]: registered on a Runtime
  Refused --> [*]: the process does not start
```

## What binding checks

**Tools.** `tools:` names are checked against the `ToolSet`. A name nobody registered is
`Error::UnknownTool`, with the closest registered name as a suggestion and the list of what is
registered (`Read` in a Claude Code file suggests `read_diff`). An entry with a `*` (`linear__*`, the
MCP spelling) is a pattern over the registered names and must match at least one
(`Error::NoToolMatches`). A tool listed twice, or matched twice, is bound once, in the order the file
lists. A `ToolSet` with two tools of one name is `Error::DuplicateTool`. Without `tools:` the root
agent gets every registered tool and a subagent gets none (decision D3 of `docs/authoring.md`); `*` is
every tool and `[]` none. A subagent's tools come from the same set but are only the ones it lists: it
inherits nothing.

**Vars.** `{{name}}` is logic-free substitution into the body of `instructions.md` and each
`instructions/*.md`; spaces inside the braces are ignored, `{{{{` writes a literal `{{` (so a
literal `{{name}}` is `{{{{name}}`), and a `}}` in text is always itself. The rules, checked per agent:

| Mistake | Error |
|---|---|
| a `{{` that is not closed, is empty or is not a var name | `Template { line, problem }` |
| a placeholder that `vars` does not declare | `UnknownVar`, with a suggestion |
| a value supplied in code for a var `vars` does not declare | `UnknownVarValue`, with a suggestion |
| a var declared and never used, even if a value was supplied | `UnusedVar` |
| a used var with an empty default and no value from the code (`vars:` with `repo:` and nothing after it) | `UnsetVar` |
| `agent_var` for an agent the definition does not have | `UnknownAgent`, with a suggestion |

A var is required, with no default, when its default is empty. The value comes from
`AgentDef::var` (root) or `agent_var("coder/reviewer", ..)` (any agent by its registered name), and any
`ToString` works. Lines are counted from the start of the body of the file (the frontmatter is not
counted); the error names the file, `instructions/10-style.md` for a part.

**Model.** One `DynModel` serves every agent (decision: one OpenAI-compatible endpoint), and the
gateway alias of each agent is, in this order: the `model:` it names; the alias of its parent when it
says `inherit` or names nothing (a subagent's default); the alias passed to `model(..)` (the root's
default). The alias is a deployment setting, so the code picks the default and a file may pin one for
an agent that needs a particular model. `model_aliases([..])` lists what the deployment serves and
turns an alias outside the list into `Error::ModelAlias` with a suggestion; an empty alias or one with
whitespace is refused either way.

**State.** `state(Arc<T>)` is given to every agent, like `LlmAgentBuilder::state`. `model()` builds
the agents with `try_build`, so a tool whose `required_state` nobody gave is
`Error::Build { origin, source: MissingState }`, naming the agent, before anything runs.

**Limits.** The frontmatter `limits` (`max_turns`, `max_tool_calls`, `max_output_tokens`,
`max_history_tokens`) replace the loop's default for the keys that are set. Each subagent has its own.

**The card.** With feature `a2a`, `Assembly::card(url, version)` is the root's `card:` as an
`adam_a2a::AgentCardConfig`: `card.name` (default: the agent's), `card.description` (default: the
frontmatter `description`, else `Error::MissingCardDescription`) and `card.skills`. The public URL and the
version are the deployment's, so they are arguments. A2A card skills are not Agent Skills.

## Errors

`Error` is a closed enum; every variant about the files carries an `Origin { agent, file }`, printed as
``agent `coder/reviewer` (agent/subagents/reviewer.md)``. All are `ErrorClass::Invalid` (the same input
never succeeds) except `Manifest`, which keeps its source's class. `bind` and `model` return the first
problem found, in the order of the tables above.

## Subagents, skills and the seams left

A subagent that is a local file becomes an `LlmAgent` of its own, registered as `<parent>/<name>`
(`coder/reviewer`, `coder/researcher/summarizer`), with its own prompt, tools, model and limits. It is
*defined*, not yet callable: nothing gives the parent a tool that starts it. A remote subagent (`a2a:`)
is data in `Assembly::remotes()`. Skills, `mcp.json` and schedules stay in `Assembly::manifest()`.
The seams are in code, in one place each:

| Slice | What plugs in | Where |
|---|---|---|
| S7 skills | the catalog appended to the prompt, `load_skill` and `read_skill_file` in the tools | `BoundDef::build`, from `manifest().skills`, filtered by `skills:` |
| S8, S9 subagents | a tool per child that starts a durable child run; least-privilege tools are already resolved | `BoundDef::build`: the children of a node are the nodes whose `parent` is its index, and `AgentInfo::description` is the tool description |
| S9b remote subagents | a tool per `RemoteInfo` | `BoundDef::build`, with `remotes` |
| S10 dev reload | `from_source` + `bind` + `model` again on a changed directory, swapped at a step boundary | a caller of this crate; `AgentDef` and `BoundDef` are plain values |
| S11 MCP tools | the discovered tools go into the `ToolSet` given to `bind`; `linear__*` patterns already match them | `AgentDef::bind` |

## Tests

`cargo test -p adam-assembly --all-features`:

* `tests/bind.rs`: unknown tool (the message asserted, the suggestion, the subagent's origin), patterns,
  default tool access, duplicate tools; unknown, unused and unset vars, values for undeclared vars and
  unknown agents, syntax errors with their line, an error in an `instructions/*.md` part.
* `tests/model.rs`: alias resolution through three generations, alias errors, a deployment's alias list,
  missing state and state reaching a running tool, manifests by value and by reference, `from_source`
  for one agent, several agents and a directory with errors.
* `tests/fixture.rs`: the fixture of [`adam-agent-fixture`](../adam-agent-fixture/README.md) (the valid
  directory of `adam-agent-fs`), embedded and read from disk, binds to equal `AgentInfo`s; each agent is
  bound as its files say; the root and a subagent run to the end on a `MockModel` through a `Runtime` on
  the in-memory store.
* `tests/card.rs` (feature `a2a`): the card of the fixture against `tests/golden/card.json`
  (regenerate with `ADAM_UPDATE_GOLDEN=1`), the fallbacks and the missing description.
* Unit tests next to the code: the template scanner (with proptest), the suggestions, the error texts,
  glob matching, the limits.

The dev-dependency on `adam-agent-fixture` is a cycle through `adam` (which re-exports this crate);
cargo allows cycles through dev-dependencies, and only the integration tests use the fixture.
