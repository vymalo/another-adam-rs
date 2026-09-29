# adam

The facade of adam-rs: one dependency for writing an agent. It re-exports
[`adam-llm-agent`](../adam-llm-agent/README.md) (with its `schema` feature), the `#[tool]` macro of
[`adam-macros`](../adam-macros/README.md), and the model, runtime, core and error crates as modules
(`adam::model`, `adam::runtime`, `adam::core`, `adam::error`), [`adam-agent-fs`](../adam-agent-fs/README.md)
as `adam::agent_fs`, [`adam-assembly`](../adam-assembly/README.md) as `adam::assembly`, and it adds a
`prelude` and the `include_agent!` macro.

```toml
[dependencies]
adam = "0.1"
```

| Feature | Default | What |
|---|---|---|
| `macros` | yes | `#[tool]` (`adam::tool`, and in the prelude) |
| `a2a` | no | `Assembly::card`: the root agent's `card:` as an `adam_a2a::AgentCardConfig` (turns on `adam-assembly/a2a`) |
| `dev` | no | dev reload: `adam::LiveAssembly` reads the agent directory at run time and swaps the agents when a file changes (turns on `adam-assembly/dev`, which brings `notify`). Off by default, so a release build cannot read prompts from disk unless it opts in |

The authoring layer around it (agent directories, skills, subagents) is designed in
[`docs/authoring.md`](../../docs/authoring.md). What exists: `#[tool]`, the agent directory
embedded at build time (`adam::include_agent!()`, below), and `AgentDef`, which binds it to
`LlmAgent`s ([`adam-assembly`](../adam-assembly/README.md), also `adam::assembly`), and, with the feature
`dev`, `adam::LiveAssembly`, which reloads the directory while the process runs (the rules for runs in flight
are in the [dev reload](../adam-assembly/README.md#dev-reload-feature-dev) section).

## `#[tool]`

An `async fn` becomes a tool. The function stays as it is (call it directly in a unit test), and a
unit struct named after it (`get_weather` becomes `GetWeather`) implements `Tool`.

```rust
use std::sync::Arc;

use adam::model::MockModel;
use adam::prelude::*;

/// Shared state: any `Send + Sync + 'static` type the agent is given.
struct Units(&'static str);

/// Get the current weather for a city. Say which city; the answer is
/// in the agent's configured units.
#[tool]
pub async fn get_weather(
    units: State<Units>,
    /// The city, for example "Berlin"
    city: String,
    /// Days of forecast to include (default 0)
    #[serde(default)]
    days: u32,
) -> Result<String, ToolError> {
    if city.trim().is_empty() {
        return Err(ToolError::Permanent("city is empty".into()));
    }
    Ok(format!("{city}: 18 degrees {} (+{days} days)", units.0))
}

// The spec is what the model sees: name from the function, description from its doc comment,
// property descriptions from the parameter docs, `required` from the types.
let spec = GetWeather.spec();
assert_eq!(spec.name, "get_weather");
assert_eq!(
    spec.description,
    "Get the current weather for a city. Say which city; the answer is in the agent's configured units."
);
assert_eq!(spec.parameters["required"], serde_json::json!(["city"]));
assert_eq!(spec.parameters["properties"]["city"]["description"], "The city, for example \"Berlin\"");

// `State<Units>` is checked when the agent is built, not in the middle of a run.
let _agent = LlmAgent::builder("assistant", Arc::new(MockModel::new()), "my-model")
    .state(Arc::new(Units("C")))
    .tools(tools![GetWeather])
    .try_build()
    .expect("every tool has its state");
```

### What the macro accepts

| In the function | Becomes |
|---|---|
| the doc comment (required) | the tool's description. Wrapped lines are joined with a space; a blank line keeps a paragraph break; list items, headings, quotes and fenced code keep their own lines |
| a doc comment on a parameter | that argument's `description` in the schema |
| `name: T` | a field of the generated arguments struct, which derives `Deserialize` and `JsonSchema`. `T` must be owned; `Option<T>` and `#[serde(default)]` make it optional |
| `#[serde(..)]`, `#[schemars(..)]` on a parameter | copied to that field |
| `&ToolCtx` (at most one, any position) | the call's context: run id, call id, cancellation, progress |
| `State<T>` (any number) | the shared `T` from the agent, read with `ctx.require_state::<T>()?` and reported by `Tool::required_state`, so `try_build` fails at startup when it is missing |
| `#[args] a: MyArgs` | an existing `Deserialize + JsonSchema` struct as the whole argument object; the only model argument |
| the return type | `Result<T, E>` or a bare `T`, with `T: IntoToolOutput` (`ToolOutput`, `String`, `&'static str`, `Value`, `Json<S>`) and `E: Into<ToolError>` |

Bad input from the model is not an error of the run: a JSON mismatch becomes a `ToolOutput::error`
(the text is "invalid arguments for get_weather: missing field city", with backticks around the names)
that the model reads and corrects.
The call still runs inside `LlmAgent`'s journaled `tool:CALL_ID` step, so the retry rules of the `Tool`
docs apply unchanged.

Options, `#[tool(..)]`:

| Option | Meaning |
|---|---|
| `name = "..."` | the tool's name; default the function's. Must match `^[a-z][a-z0-9_]{0,63}$` |
| `type = Name` | the generated struct; default the function's name in `UpperCamelCase`. It has the function's visibility |
| `strict` | unknown argument fields are refused (`deny_unknown_fields`, and `additionalProperties: false` in the schema) |
| `classify` | the error type is `adam::error::Classify` (`adam_error::Classify`): retryable classes become `ToolError::Transient`, the others `Permanent` (`ToolError::from_classified`) |
| `asks_user` | the tool can end a call with `ToolError::NeedsInput`: the generated `Tool::asks_user` says `true`, and `adam-assembly` refuses to give the tool to a subagent (nobody could answer it) |
| `crate = path` | where `Tool` and `__private` live; default `::adam`. `::adam_llm_agent` works without the facade, see below |

The schema is computed once per tool (a `OnceLock`) and is draft 2020-12 with subschemas inlined, no
`$schema` and no `title` (see `spec_for` in `adam-llm-agent`).

### Without the facade

A crate that depends on `adam-llm-agent` alone (feature `schema`) points the macro at it:

```rust
use adam_llm_agent::{ToolError, ToolOutput};
use adam_macros::tool;

/// Echo the text back.
#[tool(crate = ::adam_llm_agent)]
async fn echo(text: String) -> Result<ToolOutput, ToolError> {
    Ok(ToolOutput::text(text))
}
```

`adam_llm_agent::__private` carries the paths the generated code needs (`serde`, `schemars`,
`async_trait`, `spec_for`, `parse_args`, ...), so the crate using the macro needs none of them itself.
Your own `#[derive(Deserialize, JsonSchema)]` types (for `#[args]`, or as an argument type) still need
`serde` and `schemars` in your `Cargo.toml`; `adam-llm-agent`'s `schema` feature enables schemars'
`derive`.

### Agent directories: `include_agent!`

An agent written as Markdown files (`agent/instructions.md`, skills, subagents, `mcp.json`) is
compiled into the binary by a build script and included with one line.

```toml
[dependencies]
adam = "0.1"

[build-dependencies]
adam-agent-fs = { version = "0.1", features = ["build"] }
```

```rust,ignore
// build.rs: parses and validates agent/ before rustc runs; a mistake is a build error with
// file:line, and a new or changed file reruns the script.
fn main() -> Result<(), adam_agent_fs::BuildError> {
    adam_agent_fs::build("agent").emit()?;
    Ok(())
}
```

```rust,ignore
// src/main.rs
adam::include_agent!(); // AGENTS, AGENT (for agent/) and PACKAGE, all `'static`

fn main() {
    println!("{} {}", AGENT.name, AGENT.digest);      // the digest identifies these exact files
    println!("{}", AGENT.instructions.body);          // the prompt, no parsing at startup
    for skill in AGENT.skills {                       // the catalog
        println!("{}: {}", skill.name, skill.description);
    }
}
```

`AGENT` is a `&'static` [`adam::agent_fs::EmbeddedAgent`](../adam-agent-fs/README.md#embedding-at-build-time).
`PACKAGE` is an `EmbeddedPackage`, a `ManifestSource` like `adam::agent_fs::Dir`, so the same
`Package` can come from the binary or from a directory at run time and the two compared. The macro
is `include!(concat!(env!("OUT_DIR"), "/adam_agent.rs"))` and nothing else; it needs the build
script and a crate that depends on `adam` (the generated code names `::adam::agent_fs`; use
`.crate_path("::adam_agent_fs")` when it depends on `adam-agent-fs` directly).
[`adam-agent-fixture`](../adam-agent-fixture/README.md) is a complete example with tests.

### Binding the agent: `AgentDef`

`AgentDef` (in the prelude, from [`adam-assembly`](../adam-assembly/README.md)) turns the embedded
agent, or one read from a directory, into `LlmAgent`s. Every mistake is found at startup, with the agent
and the file in the message.

```rust,ignore
use std::sync::Arc;
use adam::prelude::*;

adam::include_agent!();

let assembly = AgentDef::from_manifest(AGENT)?            // or an AgentManifest from a Dir
    .var("repo", "acme/widgets")                          // a value for {{repo}} in the prompt
    .bind(tools![PrepareWorkspace, RunChecks, AskUser])?  // `tools:` in the files must name these
    .state(Arc::new(env))                                 // what `State<T>` parameters read
    .model(model, "coder-large")?;                        // one client; the default gateway alias

let runtime = assembly.register(Runtime::builder(store)).build(); // the root and every subagent
```

A typo in the files fails `bind`: a `tools: [run_check]` gets
``agent `coder` (agent/instructions.md): `tools` names `run_check`, which is not a registered tool; did you mean `run_checks`?``,
and so do a `{{placeholder}}` that `vars` does not declare, a var that is never used, and a var with no
value. The stages, the rules for tools, vars, models, state and skills, and the seams left for
subagents are in the [`adam-assembly` README](../adam-assembly/README.md). `Assembly::info()` describes
each agent made (name, alias, rendered prompt, tools, skills, limits); with the `a2a` feature,
`Assembly::card(url, version)` is the root's `AgentCardConfig` (`AgentDef::card` gives it before
anything is bound).

An agent's `skills/` need no code: the prompt gets a catalog (name and description of each skill) and the
agent gets a `load_skill` tool (the body of `SKILL.md`) and a `read_skill_file` tool (a bundled text file),
the [Agent Skills](https://agentskills.io/specification) progressive disclosure. `preload_skills:` puts a
body in the prompt instead. It works the same for the embedded agent and for a directory read with
`AgentDef::from_source`.

### Compile errors

The macro checks what it can and says so where you wrote it:

| Mistake | Message |
|---|---|
| no doc comment | `#[tool]` needs a doc comment: it is the description the model reads |
| not `async` | `#[tool]` functions must be `async` |
| generic, `where`, or `impl Trait` | `#[tool]` functions cannot be generic; model arguments are deserialized into concrete types |
| `self` receiver | `#[tool]` works on free functions; put shared state in `State<T>` |
| `&str` argument | model arguments must be owned (`String`, not `&str`): they are deserialized from JSON |
| two `&ToolCtx` | at most one `&ToolCtx` parameter |
| `#[args]` with another model argument | `#[args]` must be the only model argument |
| a bad tool name | tool names must match `^[a-z][a-z0-9_]{0,63}$` |
| an unknown option | unknown `#[tool]` option `aproval`; expected one of: name, type, strict, classify, asks_user, crate |
| on a struct or trait | `#[tool]` goes on an `async fn` |

Every mistake of one function is reported in one compile. rustc reports the rest, with messages
written for it: an argument type without `Deserialize` or `JsonSchema` ("`Foo` cannot be a `#[tool]`
argument"), and a return type that is not a tool result.

## Tests

* `crates/adam-macros/src/expand.rs`: unit tests of the expansion on token streams (every option, every
  parameter kind, every error message, the doc-comment joining).
* `tests/tool_macro.rs`: generated tools through a real `LlmAgent` and `MockModel` on the in-memory
  store: arguments in, output back, bad input, state, `try_build`, cancellation context, `classify`.
* `tests/ui.rs`: `trybuild`. `tests/ui/pass/*.rs` must compile, `tests/ui/fail/*.rs` must fail with the
  message in the `.stderr` beside it (errors the macro produces, stable across toolchains). The errors
  rustc itself produces (`tests/ui/rustc/*.rs`) change wording between toolchains, so they run only with
  `ADAM_TRYBUILD=1`, in the CI job pinned to one toolchain. To regenerate snapshots:
  `TRYBUILD=overwrite cargo test -p adam --test ui` (and with `ADAM_TRYBUILD=1` for the rustc ones).
* The example above is a doctest.
