# Architecture

adam-rs is a Rust workspace of 22 crates for **durable AI agents**. An agent is
a state machine. The runtime saves its state after every step, so a worker that
dies loses nothing: another worker resumes from the last saved step. Every piece
of infrastructure (database, model, code host, A2A backend) sits behind a trait,
so each deployment picks its own.

This document describes the code **as built**. For the durable model in detail
(runs, journal, leases, scheduling) and the store adapters, read the
[root README](../README.md). This page shows how the parts connect.

> **Status of the facts.** Everything about this repository's own behaviour was
> *verified* on 2026-09-29 by reading the code at commit `172a117` (`main`).
> Claims about third-party products and standards are marked *verified* or
> *unverified* where they occur, and collected in
> [Verified and unverified](#verified-and-unverified).

Contents:

* [The crate map](#the-crate-map)
* [Ports and implementations](#ports-and-implementations)
* [The path of a task](#the-path-of-a-task)
* [The run lifecycle](#the-run-lifecycle)
* [The error tree](#the-error-tree)
* [The coder agent](#the-coder-agent)
* [The generic agent](#the-generic-agent)
* [Where to go next](#where-to-go-next)
* [Verified and unverified](#verified-and-unverified)

## The crate map

Arrows point from a crate to a crate it depends on. Solid arrows come from
`[dependencies]` in the `Cargo.toml` files. Dotted arrows are
`[dev-dependencies]` that are not also normal dependencies (tests only). Grey
arrows go to `adam-error`: every crate except the two test kits,
`adam-agent-fixture` and `adam-macros` depends on it. `adam-assembly` reaches
`adam-a2a` through its optional feature `a2a`, and its dotted edge to
`adam-agent-fixture` closes a cycle through `adam` that cargo allows because
it is a dev-dependency.

```mermaid
flowchart TB
    subgraph authoring["Authoring"]
        adam["adam"]
        macros["adam-macros"]
        agentfs["adam-agent-fs"]
        asm["adam-assembly"]
    end
    subgraph agents["Agents"]
        coder["adam-coder"]
        agent["adam-agent"]
        llm["adam-llm-agent"]
        ui["adam-ui"]
    end
    subgraph runtime["Runtime"]
        a2art["adam-a2a-runtime"]
        rt["adam-runtime"]
        service["adam-service"]
    end
    subgraph impls["Implementations"]
        pg["adam-store-postgres"]
        mongo["adam-store-mongodb"]
        openai["adam-model-openai"]
        pgn["adam-notify-postgres"]
        ws["adam-workspace"]
        devc["adam-devcontainer"]
        acp["adam-acp"]
    end
    subgraph contracts["Contracts and ports"]
        a2a["adam-a2a"]
        core["adam-core"]
        model["adam-model"]
        err["adam-error"]
        host["adam-host"]
    end
    subgraph kits["Test kits"]
        testkit["adam-store-testkit"]
        nk["adam-notify-testkit"]
        fixture["adam-agent-fixture"]
    end

    coder --> a2a
    coder --> adam
    coder --> a2art
    coder --> acp
    coder --> core
    coder --> llm
    coder --> model
    coder --> rt
    coder --> service
    coder --> ws
    devc --> ws
    coder --> host
    coder --> ui
    agent --> ui
    ui --> a2a
    ui --> a2art
    ui --> llm
    agent --> a2a
    agent --> adam
    agent --> llm
    agent --> model
    agent --> rt
    agent --> service
    service --> a2a
    service --> a2art
    service --> core
    service --> host
    service --> openai
    service --> pg
    service --> pgn
    service --> rt
    a2art --> a2a
    a2art --> core
    a2art --> rt
    llm --> core
    llm --> model
    llm --> rt
    rt --> core
    openai --> model
    mongo --> core
    pg --> core
    testkit --> core
    pgn --> core
    pgn --> rt
    nk --> core
    nk --> rt
    fixture --> adam
    fixture --> agentfs
    asm --> a2a
    asm --> agentfs
    asm --> llm
    asm --> model
    asm --> rt
    adam --> agentfs
    adam --> asm
    adam --> core
    adam --> llm
    adam --> macros
    adam --> model
    adam --> rt

    rt -.-> mongo
    rt -.-> pg
    rt -.-> testkit
    a2art -.-> llm
    a2art -.-> model
    a2art -.-> pg
    mongo -.-> testkit
    pg -.-> testkit
    pgn -.-> nk
    pgn -.-> pg
    asm -.-> fixture
    coder -.-> openai
    coder -.-> pg
    agent -.-> core
    agent -.-> host
    agent -.-> pg

    a2a --> err
    adam --> err
    a2art --> err
    acp --> err
    coder --> err
    service --> err
    agent --> err
    core --> err
    llm --> err
    openai --> err
    model --> err
    rt --> err
    mongo --> err
    pg --> err
    ws --> err
    agentfs --> err
    asm --> err
    host --> err
    pgn --> err
    devc --> err

    linkStyle 71,72,73,74,75,76,77,78,79,80,81,82,83,84,85,86,87,88,89,90 stroke:#999,stroke-width:1px
```

The layers, from the bottom:

* **Contracts and ports.** Small crates that define what other crates
  implement or call.
  * [`adam-error`](../crates/adam-error/README.md): `ErrorClass`, `Classify`,
    `BoxError`, `report()`. No I/O, no async.
  * [`adam-core`](../crates/adam-core/README.md): the `Store` trait, the run,
    journal and lease types, and `MemoryStore`, the reference implementation.
  * [`adam-model`](../crates/adam-model/README.md): the `ModelClient` trait and
    its request and response types. It also holds `MockModel`, a scripted
    double that is always compiled.
  * [`adam-a2a`](../crates/adam-a2a/README.md): the `TaskBackend` seam, and
    the axum server that exposes any backend as an A2A 1.0 agent. It knows
    nothing about the runtime.
  * [`adam-host`](../crates/adam-host/README.md): the closed process `Role`
    (`all`, `control-plane`, `worker`), the closed workspace `Placement`
    (`shared`, `affinity`, `isolated`, `a2a-only`) and `Host`, a small
    supervisor that runs only the components a role asks for and stops them in a
    fixed order. A host app such as `adam-coder` reads the role and the
    placement from its own variables.
* **Implementations.** Each one is a separate crate, so a binary links only what
  it uses.
  * `adam-store-postgres` and `adam-store-mongodb` implement `Store`.
  * `adam-model-openai` implements `ModelClient` for any OpenAI-compatible
    chat-completions endpoint.
  * `adam-workspace` runs `git` for mirrors, worktrees, commit and push, under
    an in-process lock and a file lock on each mirror, so several processes can
    share a root. A run's workspace (`Workspaces::run`) is a directory of
    **slots**, each a repository's worktree or a scratch project, under its own
    lock ([ADR 0008](decisions/0008-a-workspace-holds-several-repositories.md)).
    It also owns three small ports of its own, `GitCredentials`, `CodeHost` (GitHub)
    and `Environment`, where a run's processes run (`Local`, this container, is
    the implementation it holds).
  * `adam-devcontainer` is the other `Environment`: it runs a run's processes in the
    devcontainer of the run's first repository, on a rootless Podman service, through
    the official devcontainer CLI ([ADR 0010](decisions/0010-a-run-works-in-its-repositorys-devcontainer.md)).
  * `adam-acp` is a client for the Agent Client Protocol: it drives a coding
    agent (OpenCode) over stdio.
  * `adam-notify-postgres` implements two ports of the runtime, `EventSink`
    (`PgEventSink`) and `Notifier` (`PgNotifier`), over PostgreSQL
    `LISTEN`/`NOTIFY`. It is separate from `adam-store-postgres` because the
    store must not depend on the runtime.
* **Runtime.**
  * `adam-runtime` owns the run state machine, the journal, the workers and the
    retry policy. It depends on `adam-core` and `adam-error` only.
  * `adam-a2a-runtime` implements the `adam-a2a` seam over the runtime.
  * [`adam-service`](../crates/adam-service/README.md) is the composition every agent binary
    shares: `serve` connects the Postgres store, builds the runtime, the A2A router and the
    cross-process notifications, and runs the components a role asks for under an
    `adam_host::Host`. It also holds the configuration the binaries have in common
    (`ServiceConfig`, `ModelConfig`, `McpSettings`) and the exit codes. It does not know what an
    agent does: a binary hands it the agent's name, its card and a closure that registers it.
* **Agents.**
  * `adam-llm-agent` is a reusable model-and-tools loop written as an
    `adam_runtime::Agent`.
  * `adam-coder` is the coder agent. It is a composition
    root: it wires the pieces below it, and `adam-service` runs the process.
  * `adam-agent` is the general binary: it serves **any agent folder** (instructions, card, skills,
    subagents and `mcp.json` tools, read at startup from `ADAM_AGENT_DIR`) with the screen's tools of `adam-ui`
    (`ask_user`, `show`, `ui_catalog`) as its only tools of its own, over the same `adam-service`. It has no embedded agent and requires the folder
    ([ADR 0005](decisions/0005-one-binary-serves-any-agent-folder.md), [README](../bin/adam-agent/README.md)).
  * [`adam-ui`](../crates/adam-ui/README.md) turns the screen's UI catalog into model tools for any agent:
    `ask_user` with `choices` (one form, the answers back as the result), `show` (blocks of the screen's
    components, validated against the catalog's JSON Schema), `ui_catalog`, and `ThreadTools`, a
    `ToolSource` of `adam-llm-agent` that offers every tool of the conversation's MCP endpoint at each model
    turn. It reads what `adam-a2a-runtime`'s `vymalo_inbound` puts in the run's inbound context and reaches the
    endpoint with `adam-mcp`'s `Endpoint`
    ([ADR 0006](decisions/0006-a2ui-and-the-vymalo-extensions-in-adam-rs.md)).
* **Authoring.**
  * `adam-macros` is the `#[tool]` attribute macro: a proc-macro crate whose
    expansion is a pure function over token streams. It depends on `syn`,
    `quote` and `proc-macro2` only.
  * `adam-agent-fs` parses and validates an agent directory (`agent/instructions.md`,
    skills, subagents, `mcp.json`, schedules) into an `AgentManifest` and reports
    every mistake with its file and line. It is a leaf over `serde` and
    `serde-saphyr`, with no async and no runtime dependency. Its feature `build`
    is the code generator a `build.rs` calls to embed the directory in the
    binary as a `'static` manifest. See [`docs/authoring.md`](authoring.md).
  * `adam-assembly` binds a manifest to `LlmAgent`s: `AgentDef::from_manifest`
    takes the embedded manifest or one read from a directory (one code path;
    `AgentFolder::load` reads one agent's folder at startup, `ADAM_AGENT_DIR`, see
    [ADR 0004](decisions/0004-agent-folders-at-run-time.md)),
    `bind` checks `tools:` against a `ToolSet` (unknown names come back with a
    "did you mean") and renders the `{{var}}` placeholders of the prompt,
    `model` gives every agent its gateway alias and builds the root and one
    `LlmAgent` per local subagent definition, and, with feature `a2a`, `card`
    turns the root's `card:` into an `AgentCardConfig`. It depends on
    `adam-agent-fs`, `adam-llm-agent`, `adam-model` and `adam-runtime`.
  * `adam-mcp` is the MCP client behind the tools of an agent's `mcp.json`
    (feature `mcp` of `adam-assembly`): it connects to each server at startup,
    lists its tools and gives them back as `Tool`s, so an MCP call is an
    ordinary journaled tool step. It depends on `adam-agent-fs`,
    `adam-llm-agent`, `adam-model`, `adam-error` and `rmcp`.
  * `adam` is the facade a user writes agents against: `prelude`, the
    re-exported `adam-llm-agent` API, the `#[tool]` macro behind the
    default feature `macros`, `AgentDef` and its stages from `adam-assembly`
    (feature `a2a` turns on the card), and `adam-agent-fs` as `adam::agent_fs`
    with the `include_agent!` macro for the agent embedded by `build.rs`. Generated
    code refers to `adam::__private` and `adam::agent_fs`.
    See [`docs/authoring.md`](authoring.md).
* **Test kits.** `adam-store-testkit` is the conformance suite every store must
  pass, and `adam-notify-testkit` the one every `Notifier` (and its event
  transport) must pass. It is a crate of its own because `adam-runtime` cannot
  dev-depend on a crate that depends on it. `adam-agent-fixture` (not
  published) is a crate with a real `build.rs` and `adam::include_agent!()`; its
  tests prove that an embedded agent equals the directory it came from. The
  other test doubles live inside
  the crates they double for (see
  [Ports and implementations](#ports-and-implementations)).

Dev-only edges (dotted): the runtime's tests run against real Postgres and
MongoDB stores, every store runs the store testkit, and `adam-notify-postgres`
runs the notifier testkit and its two-runtime tests over `adam-store-postgres`.
`adam-a2a-runtime`'s tests also put a real `LlmAgent` and a `MockModel` behind the backend, to
show that a task which continues another gives the model the earlier messages. `adam-a2a`,
`adam-workspace` and `adam-coder` also enable their own `test-util` feature in
tests. That adds no new crate edge.

The workspace is `crates/*` and `bin/*` (see the root `Cargo.toml`): libraries
live in `crates/`, binaries (the agents you can run) in `bin/`, so a new crate or
binary joins by adding a directory. `adam-coder` and `adam-agent` are the binaries today.

## Ports and implementations

A **port** is a trait that a crate defines and other crates implement. The
core never names an implementation: it holds a `dyn` handle (`DynStore`,
`DynModel`, `DynTaskBackend`, `DynCodeHost`, ...). Swapping happens **at build
time**: pick the crates in `Cargo.toml` (and features), and pick the values in
the composition root. There are no runtime plugins and no dynamic loading. This
is the same rule as ADR 0009 of the sibling orchestration layer
(`vymalo/another-agentic-system`): swappable implementations, at build time
(*verified* 2026-09-29: invariant 6 in that repository's agent guide, `AGENTS.md`).

```mermaid
classDiagram
    direction LR

    namespace adam_core {
        class Store {
            <<interface>>
            migrate()
            create_run()
            commit_run()
            journal_get()
            journal_put()
            claim_due()
            renew_lease()
            release_lease()
        }
        class MemoryStore
    }
    namespace adam_store_postgres {
        class PgStore
    }
    namespace adam_store_mongodb {
        class MongoStore
    }
    namespace adam_store_testkit {
        class FaultyStore
    }
    Store <|.. MemoryStore
    Store <|.. PgStore
    Store <|.. MongoStore
    Store <|.. FaultyStore

    namespace adam_model {
        class ModelClient {
            <<interface>>
            complete()
            stream()
        }
        class MockModel
    }
    namespace adam_model_openai {
        class OpenAiCompatible
    }
    ModelClient <|.. OpenAiCompatible
    ModelClient <|.. MockModel

    namespace adam_a2a {
        class TaskBackend {
            <<interface>>
            submit()
            get()
            cancel()
            subscribe()
        }
        class InMemoryBackend
    }
    namespace adam_a2a_runtime {
        class RuntimeTaskBackend
    }
    TaskBackend <|.. RuntimeTaskBackend
    TaskBackend <|.. InMemoryBackend

    namespace adam_workspace {
        class CodeHost {
            <<interface>>
            open_pull_request()
            find_pull_request()
            create_repository()
            owner_kind()
            authenticated_login()
        }
        class GitCredentials {
            <<interface>>
            token_for()
        }
        class GitHub
        class MemoryCodeHost
        class StaticToken
        class ScopedToken
        class HostScoped
        class GitHubApp
    }
    CodeHost <|.. GitHub
    CodeHost <|.. MemoryCodeHost
    GitCredentials <|.. StaticToken
    GitCredentials <|.. ScopedToken
    GitCredentials <|.. HostScoped
    GitCredentials <|.. GitHubApp

    namespace adam_runtime {
        class Agent {
            <<interface>>
            name()
            init()
            init_continuing()
            step()
        }
        class AgentStarter {
            <<interface>>
            name()
            init()
            init_continuing()
        }
        class EventSink {
            <<interface>>
            emit()
        }
        class Clock {
            <<interface>>
            now()
        }
        class Notifier {
            <<interface>>
            publish()
            subscribe()
        }
        class LocalNotifier
        class NoopSink
        class BroadcastSink
        class CollectingSink
        class SystemClock
        class ManualClock
    }
    EventSink <|.. NoopSink
    EventSink <|.. BroadcastSink
    EventSink <|.. CollectingSink
    Notifier <|.. LocalNotifier
    Clock <|.. SystemClock
    Clock <|.. ManualClock

    namespace adam_notify_postgres {
        class PgEventSink
        class PgNotifier
    }
    EventSink <|.. PgEventSink
    Notifier <|.. PgNotifier

    namespace adam_llm_agent {
        class Tool {
            <<interface>>
            spec()
            call()
            required_state()
        }
        class FnTool
        class LlmAgent
        class LlmStarter
    }
    namespace adam_coder {
        class CoderAgent
        class CoderStarter
        class PrepareWorkspace
        class DelegateToOpenCode
        class RunChecks
        class CommitAndPush
        class OpenPullRequest
        class AskUser
    }
    Tool <|.. FnTool
    Agent <|.. LlmAgent
    Agent <|.. CoderAgent
    AgentStarter <|.. LlmStarter
    AgentStarter <|.. CoderStarter
    Tool <|.. PrepareWorkspace
    Tool <|.. DelegateToOpenCode
    Tool <|.. RunChecks
    Tool <|.. CommitAndPush
    Tool <|.. OpenPullRequest
    Tool <|.. AskUser

    namespace adam_acp {
        class PermissionPrompt {
            <<interface>>
            ask()
        }
        class StaticPrompt
    }
    PermissionPrompt <|.. StaticPrompt
```

Each box is a crate (underscores stand for hyphens). The twelve coder tools are
`prepare_workspace`, `start_scratch`, `publish_scratch`, `run_command`, `read_file`, `write_file`, `apply_patch`, `delegate_to_opencode`, `run_checks`,
`commit_and_push`, `open_pull_request` and `ask_user` (the tool of `adam-ui`, under the coder's own words about when to ask), and
`adam-ui` adds `show` and `ui_catalog` and a source of the tools of the conversation's endpoint. A thirteenth type,
`Redacting`, wraps each of them to scrub secrets (`bin/adam-coder/src/tools/mod.rs`). `CoderAgent`
wraps the `LlmAgent` that `adam-assembly` builds from `bin/adam-coder/agent/instructions.md` (the prompt, the
limits and the A2A card are that file) and adds its completion rule. `FnTool` is a tool made from a closure. A tool
reads shared dependencies with `ToolCtx::state::<T>()` (given to the agent with
`LlmAgentBuilder::state`), declares them in `Tool::required_state`, and
`LlmAgentBuilder::try_build` fails at startup when one is missing; `parse_args`,
`IntoToolOutput`, `ToolSet` and, with the `schema` feature, `spec_for` remove the boilerplate
(see the [crate README](../crates/adam-llm-agent/README.md)). `AgentStarter` is the start-only half
of `Agent` (`name`, `init` and `init_continuing`, no `step`): a process that only accepts requests registers
a starter (`LlmStarter`, `CoderStarter`) and never holds the agent's model or credentials.

The boundaries, by what they swap:

| Port | Defined in | Real implementations | Doubles |
|---|---|---|---|
| `Store` | `adam-core` | `PgStore`, `MongoStore` | `MemoryStore` (reference), `FaultyStore` (fault injection, `adam-store-testkit`) |
| `ModelClient` | `adam-model` | `OpenAiCompatible` | `MockModel` |
| `TaskBackend` | `adam-a2a` | `RuntimeTaskBackend` | `InMemoryBackend` (feature `test-util`) |
| `CodeHost` | `adam-workspace` | `GitHub` (feature `github`, on by default) | `MemoryCodeHost` (feature `test-util`) |
| `Environment` | `adam-workspace` | `Local` (the caller's own container), `DevContainer` (`adam-devcontainer`: the first repository's devcontainer, on a rootless Podman service) | the stub Podman and stub CLI of `adam-devcontainer`'s tests |
| `GitCredentials` | `adam-workspace` | `ScopedToken` (one token, limited to named hosts), `GitHubApp` (installation access tokens minted from a GitHub App's key, feature `github`; wrapped in `HostScoped`, which limits any credentials to named hosts), `StaticToken` (one token, any host) | none needed |
| `Agent` | `adam-runtime` | `LlmAgent`, `CoderAgent` | test agents |
| `AgentStarter` | `adam-runtime` | `LlmStarter`, `CoderStarter` | test starters |
| `Tool` | `adam-llm-agent` | the coder tools, `FnTool` | test tools |
| `EventSink` | `adam-runtime` | `BroadcastSink` (in-process), `PgEventSink` (`adam-notify-postgres`: local first, then `NOTIFY` to other processes) | `NoopSink` (default), `CollectingSink` |
| `Notifier` | `adam-runtime` | `PgNotifier` (`adam-notify-postgres`) | `LocalNotifier` (in-process; also the fan-out inside `PgNotify`) |
| `Clock` | `adam-runtime` | `SystemClock` | `ManualClock` |
| `PermissionPrompt` | `adam-acp` | none in this repository (the default policy needs no prompt) | `StaticPrompt` |

Not every boundary is a trait. `Workspaces` (in `adam-workspace`) and
`AcpClient` (in `adam-acp`) are concrete types: one shells out to the `git`
CLI, the other spawns a child process. The traits in `adam-workspace` cover the
parts that vary (credentials and the code host).

Rules the code follows, from the crate docs:

* No implementation type appears in a trait signature. A driver error is boxed
  as a `BoxError` `source` (see [The error tree](#the-error-tree)).
* A store passes the **conformance suite** in `adam-store-testkit`
  (`store_conformance!`), so "passes the testkit" means "behaves like every other
  store". To add a backend, implement `Store` and add one line.
* The correctness guarantee is the version compare-and-swap in
  `Store::commit_run`, not the lease. Any store that honours the trait keeps
  it.

### How a binary composes them

`adam-coder` is the composition root. Its `serve` function
(`bin/adam-coder/src/serve.rs`) reads the agent files, builds the coder's agent (the model client,
GitHub, the workspaces, the MCP servers of the folder) and hands it to
[`adam-service`](../crates/adam-service/README.md)'s `serve`, which is the rest of the process; `main` is
`serve(Config::from_env(), sigterm)`. To use MongoDB, another model client or
another code host, write another root that builds the same pieces. A second binary,
over another agent, is the same two steps with another agent: `adam-agent` is that binary for any agent folder.

`adam_service::serve` does not supervise anything by hand. It builds the pieces, registers what
the process runs as components of an `adam_host::Host`, and calls `run`. The
`ROLE` variable, parsed with `adam_host::Role`, decides which components run.

```mermaid
flowchart LR
    env["Config::from_env()<br/>environment variables, ROLE included"] --> serve

    subgraph serve["adam-coder: serve()"]
        direction LR
        subgraph wcfg["roles that run workers (all, worker): Config::worker is Some, build_agent()"]
            direction LR
            mdl["ModelConfig::client<br/>OpenAiCompatible as DynModel"]
            creds["ScopedToken, or HostScoped over GitHubApp<br/>inside RedactingCredentials<br/>as DynGitCredentials"]
            wsp["Workspaces::new<br/>allow_hosts, allow_local"]
            gh["GitHub::new(creds)<br/>as DynCodeHost"]
            tenv["ToolEnv<br/>workspaces + code host + settings + Redactor"]
            mcp["AgentDef::connect_mcp<br/>the folder's mcp.json, McpSettings policy"]
            agent["CoderAgent::try_from_def<br/>model + ToolEnv + MCP tools"]
        end
        starter["CoderStarter<br/>name + init only<br/>(role control-plane: no model, no GitHub)"]
        agents["Agents { name, card, register, options }"]
    end

    subgraph svc["adam-service: serve(ServiceConfig, Agents, shutdown)"]
        direction LR
        pg["PgStore::connect + migrate()<br/>as DynStore"]
        pgn["PgNotify::new(store pool, BroadcastSink)<br/>LiveSignals: PgEventSink + PgNotifier"]
        service["Service::new_with(register(Runtime::builder(store)), name, options, live)"]
        rtm["Runtime<br/>PgEventSink as EventSink, PgNotifier as Notifier"]
        bk["RuntimeTaskBackend"]
        router["A2aServer::router<br/>card + backend + AuthConfig"]
        cp["control-plane component a2a-server<br/>axum::serve"]
        wrk["worker component worker<br/>Runtime::run_worker"]
        ntf["component notify<br/>PgNotify::run<br/>(worker component in all and worker,<br/>control-plane component in control-plane)"]
        hlt["worker component health<br/>A2aServer::health_router<br/>(role worker only)"]
        host["Host::new(role)<br/>.run(shutdown)"]
    end

    serve --> svc
    creds --> wsp
    creds --> gh
    wsp --> tenv
    gh --> tenv
    tenv --> agent
    mdl --> agent
    mcp --> agent
    agent -- "all, worker" --> agents
    starter -- "control-plane" --> agents
    agents --> service
    pg --> service
    pg -- "pool" --> pgn
    pgn --> service
    pgn --> ntf
    service --> rtm
    service --> bk
    bk --> router
    router --> cp
    rtm --> wrk
    cp --> host
    wrk --> host
    ntf --> host
    hlt --> host
```

| `ROLE` | Components that run | Needs |
|---|---|---|
| `all` (default) | `a2a-server`, `worker` and `notify`: one process, as before | every variable |
| `control-plane` | `a2a-server` and `notify`, over a `Service` whose `Runtime` has the agent's `CoderStarter` only (`Coder::control_plane_with`), which starts, delivers, cancels and views runs; `run_worker` is never called | `DATABASE_URL`, `A2A_BEARER_TOKENS`, `PUBLIC_URL`; no model, GitHub or workspace variables, and no workspace root is created |
| `worker` | `worker` (`run_worker`), `notify` and `health` (`GET /healthz` on `LISTEN_ADDR`, no A2A) | everything except `A2A_BEARER_TOKENS` and `PUBLIC_URL` |

Only the roles that run workers hold the model, GitHub and workspace settings. Starting a run
needs the agent's name and its `init` and nothing else, so the control plane registers a
`CoderStarter` (an `adam_runtime::AgentStarter`, `RuntimeBuilder::starter`) instead of the
`CoderAgent`, and `Config::worker` is `None` for it: no model client, GitHub client or
workspaces are built, and none of their variables is read
(`bin/adam-coder/src/serve.rs`, `config.rs`; the components, `crates/adam-service/src/serve.rs`). The runtime claims only registered agents, so
a control plane never steps a run even if `run_worker` were called. `CoderAgent::init`
delegates to `CoderStarter`, so both start a run with the same state. The two roles meet in the Postgres store: the run record with its version
compare-and-swap, and leases, which is what makes them correct. They also meet in `NOTIFY`
(see [Across processes: signals and events](#across-processes-signals-and-events)): every role
runs the component `notify`, one `PgNotify` over the store's own pool, so a worker wakes at
once for a run a control plane started, a cancel reaches a step in another process at once,
and the control plane's stream carries a worker's progress events as they happen. Polling
stays on, so without `notify` (a library user's `LiveSignals::local()`, a lost connection, a
transaction-mode pooler) the same things happen a poll interval (250 ms) late. In `all` and
`worker`, `notify` is a worker component that stops only after `worker` has finished, so the
last step's events and signals are still sent; in `control-plane` it stops with the server.

On SIGTERM `Host` stops the control plane first: the server stops taking
connections and open streams get 10 seconds. It then stops the workers, with no
bound, so they finish and commit the steps they are in. If a component stops on
its own the others are stopped in the same order and the process exits with the
component named in the error (`HostError`, exit code 70).

### Where a run's files live: workspace placement

A run moves between workers at every step: a `Continue` is committed, the lease is released, and
the next claim may go to any worker (`crates/adam-runtime/src/worker.rs`, `run_worker`). The
coder keeps a workspace per run (worktrees, below) under a local root, so with two workers on two disks a run can land
on a worker that has no workspace for it. Nothing fails: `prepare_workspace` clones again, the
branch `agent/<short>` is taken, a longer one is picked, the run notes are gone, and a second pull
request is opened. The deployer therefore chooses a **placement**
([ADR 0002](decisions/0002-workspace-placement.md)), the closed enum `adam_host::Placement`, read
by `adam-coder` from `WORKSPACE_PLACEMENT`:

| `WORKSPACE_PLACEMENT` | Worker root | Claims | Needs `WORKER_ID` |
|---|---|---|---|
| `shared` (default) | `WORKSPACE_ROOT`, one volume for all workers | `ClaimScope::Any` | no |
| `affinity` | `WORKSPACE_ROOT/<WORKER_ID>` | `ClaimScope::Pinned` | yes |
| `isolated` | `WORKSPACE_ROOT`, a volume of this worker only | `ClaimScope::Pinned` | yes |
| `a2a-only` | none | `Any` | refused by `adam-coder` (its tools need a workspace) |

`serve` maps `Placement::pins_runs()` to `RuntimeOptions::claim_scope`
(`bin/adam-coder/src/serve.rs`, `adam_service::claim_scope_for`), and `RuntimeBuilder::claim_scope` to the claim. The store keeps
the **owner** of a run beside its lease (`runs.owner` in Postgres, `owner` in MongoDB), set by the
first pinned claim and never cleared by a release or a commit:

```mermaid
sequenceDiagram
    autonumber
    participant W1 as worker w1 (Pinned)
    participant DB as Store
    participant W2 as worker w2 (Pinned)

    W1->>DB: claim_due(agents, w1, Pinned, now, ttl, limit)
    DB-->>W1: lease on run R, owner of R was unset and is now w1
    W1->>DB: commit_run(R, version, next state)
    W1->>DB: release_lease(R, w1)
    W2->>DB: claim_due(agents, w2, Pinned, now, ttl, limit)
    DB-->>W2: no lease: R is owned by w1
    W1->>DB: claim_due(agents, w1, Pinned, now, ttl, limit)
    DB-->>W1: lease on run R again
```

```mermaid
stateDiagram-v2
    [*] --> Unowned: create_run
    Unowned --> Unowned: claim_due with Any (owner ignored)
    Unowned --> Owned: first claim_due with Pinned, owner = that worker
    Owned --> Owned: claim_due by the owner, or with Any
    Owned --> Owned: commit_run, release_lease (owner kept)
    Unowned --> [*]: purge_finished
    Owned --> [*]: purge_finished
```

* **A pinned run whose owner is gone is stranded.** A claim cannot tell "the owner is busy" from
  "the owner is gone", so nothing adopts the run and nothing reports it. Adoption is future work;
  until then a deployment that pins keeps worker ids stable and does not scale workers in.
* **`shared` needs the mirror lock.** Several worker processes on one volume would otherwise
  fail in git with "could not lock". `adam-workspace` holds an exclusive `flock` on
  `<mirror>.lock` under its in-process lock (`crates/adam-workspace/src/workspace.rs`,
  `lock_mirror`). *Unverified:* `flock` on NFS and Longhorn RWX volumes.
* **`a2a-only`** is for hosts whose agents only call remote agents, such as the
  `another-agentic-system` orchestrator; the enum is shared so both speak the same vocabulary.

### What a run's workspace holds

A run's files are a **workspace**, `<root>/workspaces/<run>/`: a directory of slots
([ADR 0008](decisions/0008-a-workspace-holds-several-repositories.md)). A slot is a worktree of one
repository on the run's own branch `agent/<run-short-id>`, or a scratch project, a local git
repository with an empty root commit where work can start before anyone has named a repository. A run
has at most one slot per repository, called the repository's name (`<name>-<owner>` when another
repository of the run has it), and slots keep the order they joined the run. The workspace lives as
long as the run and is deleted when it ends (the coder's janitor, below); the run's notes and its `agent/*` branches stay.
`copy_into` is the only way a scratch project's files reach a repository, all or nothing (the coder's
`publish_scratch` calls it, for a repository the person named, and the project remembers where it went), and
`initialize_empty` gives a repository that has no branch its first commit, an empty one, the only push
outside `agent/*`. A workspace made before slots existed (one worktree in `<root>/worktrees/<run>`) is
read as a slot and removed with the rest. For the orchestration layer's open question 24, the MVP
default is `shared` placement with one coder process. The API, the layout and the locks are in the
[`adam-workspace` README](../crates/adam-workspace/README.md#a-runs-workspace-slots-scratch-projects-and-the-copy-between-them).

```mermaid
stateDiagram-v2
    [*] --> Empty: a run asks for its workspace
    Empty --> Scratch: add_scratch
    Empty --> RepoBacked: add_repository
    Scratch --> RepoBacked: add_repository, copy_into
    RepoBacked --> RepoBacked: add_repository (another repository, a new slot)
    Scratch --> Removed: remove, nothing was published
    RepoBacked --> Removed: remove, pushed branches remain
    Removed --> [*]
```

### Where a run's processes run

The processes that act on a run's files (the project's checks, a command to look around, OpenCode) are started
through the `Environment` port of `adam-workspace`. A tool asks the run's session to **prepare** the command from a
description (program or shell line, working directory, variables, the names of this process's secrets to hide) and
spawns what comes back; the files and the paths are the same in every environment, so the file tools and all git
work stay in the coder. `Local`, the coder's own container, behaves as the tools always did. The other,
`DevContainer` of [`adam-devcontainer`](../crates/adam-devcontainer/README.md), runs the processes in a container made from
the repository's own `devcontainer.json` (the first slot of the run decides, a default image when it has none), on a rootless
Podman service and never the host's Docker socket; the file is untrusted and checked three times, nothing of the coder's
environment enters the container, and every step of making it is shown
([ADR 0010](decisions/0010-a-run-works-in-its-repositorys-devcontainer.md), which has the sequence and the lifecycle of that
environment). It is chosen when the binary is composed (swapped at build time, not by a plugin): `adam-coder` composes it when
`DEVCONTAINER_RUNTIME=podman` (off by default, and off on Kubernetes), starts OpenCode inside it too (the coder's own binary, mounted
read-only; the model key as a file), has a `rebuild_environment` tool for the way out of a file that cannot be used, and has its
janitor release a run's environment before it removes the run's workspace ([the coder's README](../bin/adam-coder/README.md#the-work-environment-the-repositorys-devcontainer)).

```mermaid
sequenceDiagram
    participant T as tool (run_command, run_checks, delegate_to_opencode)
    participant E as Environment
    participant S as EnvSession
    participant P as process
    participant J as janitor
    T->>E: ensure(workspace of the run, progress)
    E-->>T: the steps of making it, shown under the tool call
    E-->>T: the run's session (made once, then reused)
    T->>S: prepare(program, cwd, env, hide)
    S-->>T: the command to spawn
    T->>P: spawn it in a process group of its own
    opt a timeout or a cancel
        T->>P: kill the process group
        T->>S: kill(the command's id)
    end
    J->>E: release(run) before the workspace is removed
    J->>E: held_runs, then release what a crash left
```

```mermaid
stateDiagram-v2
    [*] --> Unmade: a run starts
    Unmade --> Made: ensure
    Unmade --> Failed: ensure fails
    Failed --> Unmade: the next tool call tries again
    Made --> Made: prepare, spawn, kill
    Made --> Unmade: the environment was lost
    Made --> Released: the janitor releases it
    Failed --> Released: the janitor releases it
    Unmade --> Released: the janitor releases it
    Released --> [*]
```

A failure to make the environment is a result for the model (no check cycle is used, nothing runs); a secret is
never in a description, only the names to hide and, for a process that needs one, a reference
(`EnvSession::secret_ref`). See the [`adam-workspace` README](../crates/adam-workspace/README.md#where-a-runs-processes-run-the-environment-port)
and [the coder README](../bin/adam-coder/README.md#where-the-processes-of-a-run-run).

## The path of a task

A task is a **run**. Its id is the A2A `task_id`, and its A2A `context_id` is
part of the run's conversation id. A client talks JSON-RPC over HTTP to the
server in `adam-a2a`. The server hands the request to a `TaskBackend`
(`adam-a2a-runtime`), which starts or feeds a run in the `Runtime`. A worker
advances the run one step at a time and commits each step to the store. The
client sees the run's progress as task events on an SSE stream.

Two sequence diagrams follow. The first is the request and the event stream.
The second is the worker that does the work.

### Request in, events out

```mermaid
sequenceDiagram
    autonumber
    participant C as A2A client
    participant S as adam-a2a<br/>auth + JSON-RPC router
    participant H as BackendHandler<br/>adam-a2a
    participant B as RuntimeTaskBackend<br/>adam-a2a-runtime
    participant R as Runtime<br/>adam-runtime
    participant DB as Store<br/>Postgres
    participant K as BroadcastSink

    C->>S: POST / SendStreamingMessage<br/>Authorization: Bearer token
    S->>S: drop any client identity header,<br/>compare SHA-256 digests in constant time
    alt missing, wrong or duplicated credentials
        S-->>C: 401 + WWW-Authenticate: Bearer<br/>JSON-RPC error -32000
    end
    S->>H: request + trusted Caller (token-N) + A2A-Extensions header
    H->>H: Caller.extensions = the named extensions the card declares
    H->>H: validate: parts not empty, role ROLE_USER
    H->>B: submit(caller, message, task_id, context_id)
    B->>B: default_inbound(message) gives an Inbound<br/>conversation = subject:context id
    B->>R: start(agent, inbound, conversation)
    R->>DB: open_run_for_conversation
    alt the conversation has an open run
        R->>DB: load_run, then commit_run with the message in the inbox
    else no open run
        R->>R: starter or agent init(input) gives the first state (an Envelope)
        R->>DB: create_run (Runnable, version 1)
        R->>K: emit Status(Runnable, started)
    end
    R-->>B: run id
    B->>R: view(run)
    R->>DB: load_run
    B-->>H: Task (submitted)
    H->>B: subscribe(caller, task id)
    B->>K: subscribe_run(run), attach to live events first
    B->>R: view(run), the snapshot, read after attaching
    B-->>C: SSE: snapshot (state submitted or working)
    Note over R,DB: Workers advance the run. See the next diagram.
    loop until a terminal state or input-required
        par live events
            K-->>B: Progress, Custom or Artifact event
            B-->>C: SSE: status-update (working) or artifact-update
        and durable poll, every 250 ms or when a Status event arrives
            B->>R: view(run)
            R->>DB: load_run
            B-->>C: SSE: status change or new artifact
        end
    end
    B-->>C: SSE ends after completed, failed, canceled or input-required
```

What the diagram cannot say:

* **Authentication fails closed.** Only `GET /.well-known/agent-card.json` and
  `GET /healthz` are public. Every other route, including unknown ones, needs a
  bearer token (`crates/adam-a2a/src/auth.rs`). An empty token list rejects
  everything. `AuthConfig::AllowAnonymous` exists for local development and logs
  a warning.
* **Errors are HTTP 200 with a JSON-RPC error object,** as the A2A SDK does it.
  A body that is not JSON gets `-32700` and a null id. A body that is JSON but
  not a request gets `-32600`. The mapping from error class to code is in
  [The error tree](#the-error-tree).
* **Ownership needs no side table.** The caller's subject is part of the run's
  conversation id (`subject:context id`), and that is durable. A task that
  belongs to someone else looks exactly like one that does not exist.
* **A request names the extensions it wants; the card decides.** The `A2A-Extensions` header (and `message.extensions`
  of a message) lists extension URIs. The handler hands the backend, in `Caller::extensions`, only the ones the card
  declares (each once, in the order named), and echoes them in the response's `A2A-Extensions` header. An extension is
  therefore optional on both sides: an agent behaves as plain A2A for a client that names none, and a client cannot
  switch on what the card never declared ([ADR 0008 of the orchestration layer](https://github.com/vymalo/another-agentic-system/blob/main/docs/decisions/0008-platform-integration-via-a2a-extension.md)).
* **Follow-ups.** A message that carries a `taskId` is delivered to the run
  with `Runtime::deliver`, and only while the task is `input-required`. Any
  other state gives `-32602`. A message with a `contextId` and no `taskId`
  is delivered to the context's open task, or starts a new one when there is
  none. A new task whose message has `referenceTaskIds` starts from the
  conversation of one of them, see
  [A new task that continues a finished one](#a-new-task-that-continues-a-finished-one).
* **Streams survive restarts.** The subscription takes its snapshot from
  `Runtime::view`, which reads the durable record, and then polls it. The live
  events from the `BroadcastSink` only cut the latency and add intermediate
  progress. They are lost if the process dies, and nothing depends on them, so a
  subscriber attached to another replica, or after a restart, sees the same
  states and artifacts.
* **The task outlives the connection.** Dropping the SSE stream drops only the
  subscription. Only `CancelTask` cancels.
* **Other methods.** `SendMessage` (blocking), `GetTask`, `CancelTask` and
  `SubscribeToTask` are served. `ListTasks` is unsupported, push-notification
  methods return `PushNotificationNotSupported`, and there is no extended agent
  card (`crates/adam-a2a/src/handler.rs`).

### A new task that continues a finished one

A refinement, a follow-up or a rework is a new A2A task in the same `contextId`, because a finished
task accepts nothing more. Without help, the new run starts from nothing and the agent forgets the
conversation. A2A lets the client say what the new task builds on, in `Message.referenceTaskIds`, and
`RuntimeTaskBackend` uses it ([ADR 0003](decisions/0003-a-new-task-continues-the-task-it-references.md),
which has the state diagram of a reference's fate and the rejected alternatives).

```mermaid
sequenceDiagram
    autonumber
    participant C as A2A client
    participant B as RuntimeTaskBackend<br/>adam-a2a-runtime
    participant R as Runtime<br/>adam-runtime
    participant DB as Store<br/>Postgres
    participant A as AgentStarter or Agent<br/>LlmStarter, LlmAgent

    C->>B: submit(caller, message with referenceTaskIds [t1], no taskId, contextId c1)
    loop each reference, at most MAX_REFERENCES (8), in order (none at all for the anonymous caller)
        B->>DB: load_run(reference), the raw record
        B->>B: this agent's, this caller's, in c1, terminal?<br/>if not: debug log and next reference
        B->>R: view(reference), only for one that passed
        Note over B,R: a state that does not decode: warn, next reference
    end
    Note over B: a fresh task started though some were given: one info line with counts
    alt a reference qualifies (t1)
        B->>R: start_with_id_continuing(task_id_for(...), agent, inbound, conversation, t1)
        R->>DB: load_run(run id): a repeat answers false here
        R->>DB: load_run(t1), and the envelope's agent state
        R->>A: init_continuing(inbound, prior state, t1)
        Note over A: LlmAgent: carry the history, drop a tool call<br/>that never got its result, reset the counters,<br/>cap the history at 256 KiB (tool output first),<br/>keep the roles alternating
        A-->>R: the new run's state
        R->>DB: create_run (Runnable, version 1)
    else none, or the conversation has an open task
        B->>R: start_with_id (a fresh start), or deliver to the open task
    end
    B-->>C: Task t2 (a new task id), same contextId
```

What the diagram cannot say:

* **The reference is checked like every task id, from the raw record.** It must be this agent's and the
  caller's (the caller's subject is part of the run's conversation id), in the same context as the new
  task, and terminal (`completed`, `failed` or `canceled`), and all of that is read from the stored record
  before any state is decoded, so a record the caller does not own is never decoded and cannot fail the
  request. One that is malformed, unknown, someone else's, in another context, still open or (the caller's
  own) unreadable is skipped, and the client gets the fresh task it would for an id that never existed, so
  it learns nothing about other callers' tasks. A message without a `contextId` gets a context of its own
  and continues nothing. Without a reference nothing is carried: the backend never guesses "the latest
  task of the context". The operator gets one `info` line, with a count per reason, when references were
  given and none qualified.
* **"Caller" is the authenticated subject.** With token authentication that is `token-<index>`, so
  reordering or replacing the configured tokens hands the history of an index to whoever holds it next;
  and the anonymous caller is every client at once, so it continues nothing.
* **It works on a front that holds only the starter.** The prior state is read from the store and
  decoded as the starter's `State`, so the split control plane needs no model or credentials.
  `Agent::init_continuing` and `AgentStarter::init_continuing` default to `init`, and a state that
  does not decode falls back to `init` with a warning. An agent that wraps another must forward
  `init_continuing`, as it forwards `init`.
* **The history is bounded, and the task is not what gives.** `Conversation::continued` shortens the tool
  outputs of old turns first (the truncation the loop already applies to what it sends), then, if that is not
  enough, drops the oldest whole turns beyond `MAX_CARRIED_BYTES` (256 KiB of JSON), saying so in a marker
  text and in `omitted_turns`, and shortens the newest prior turn's outputs only last. The first user message
  of the chain and the newest prior turn are always kept. Adjacent user messages become one message with several text parts, so the roles alternate.
* **The new run is an ordinary run**: a new id, its own journal and limits, its own worktree. Only its
  first state comes from the old run.
* **The coder carries the work on, not only the words.** `CoderAgent` and `CoderStarter` forward
  `init_continuing`. The repository rule reads the person's messages of the whole carried conversation, part
  by part, without the omission marker. A rework can check out the branch an earlier task pushed
  (`prepare_workspace`'s `branch`, accepted only for a branch that a `commit_and_push` result of that
  conversation recorded for that repository, in the run notes by the tool itself) and, once its checks have
  passed, `open_pull_request` moves that branch to the run's commits, which updates the same pull request,
  and reports it as already open. See the
  [coder's README](../bin/adam-coder/README.md#a-task-that-continues-a-task).

### The worker: claim, step, journal, commit

The worker loop is `Runtime::run_worker` in
`crates/adam-runtime/src/worker.rs`. In the default role (`all`) it runs in the same
process as the A2A server; with `ROLE=worker` it runs alone. Any number of
processes can share one database.

```mermaid
sequenceDiagram
    autonumber
    participant W as run_worker loop
    participant DB as Store
    participant T as advance task
    participant A as Agent.step<br/>LlmAgent inside CoderAgent
    participant X as Ctx
    participant M as ModelClient<br/>OpenAiCompatible
    participant L as Tool<br/>coder tools
    participant E as workspace, OpenCode, GitHub
    participant K as EventSink

    loop until shutdown
        W->>DB: claim_due(agents, worker id, claim scope, runs in flight here, now, lease ttl, free slots)
        DB-->>W: leases: due runs with no live lease, not in flight here (and, if pinned, not owned by another worker), earliest first
        W->>T: spawn advance(lease), at most concurrency at once
        T->>T: start lease renewer (every ttl/3)<br/>and cancel watch (every poll interval)
        T->>T: decode the Envelope, build a Ctx<br/>with seq, inbox and attempt
        T->>A: step(ctx, state)
        A->>X: take_inbox()
        A->>X: step("model:N", call the model)
        X->>DB: journal_get(run, seq)
        alt already recorded (replay)
            DB-->>X: recorded entry, the effect does not run again
        else not recorded
            X->>M: stream(request), or complete(request) when stream_text is off
            M-->>X: text deltas, then the response, or a ModelError
            X->>K: emit TextDelta pieces while the model writes (streamed text)
            X->>DB: journal_put(run, entry): the response and the stream's id, first writer wins
        end
        X-->>A: the recorded outcome
        A->>K: emit Custom agent_text (with the stream's id for words before a tool call)
        loop each tool call of the turn
            A->>K: emit Step tool:call id running
            A->>X: step("tool:call id", run the tool)
            X->>DB: journal_get(run, seq)
            opt not recorded
                X->>L: call(ctx, args)
                L->>E: git, OpenCode over ACP, shell checks, pull request
                L->>K: emit Step updates (emit_progress, report_step) and Artifact events
                X->>DB: journal_put(run, entry)
            end
            A->>K: emit Step tool:call id completed, failed or waiting
        end
        A-->>T: Transition (Continue, Park, Done, Fail) or AgentError
        T->>DB: commit_run(run, version, update), compare-and-swap
        alt version moved (someone else advanced or cancelled the run)
            DB-->>T: Conflict
            T->>T: merge only newly delivered messages,<br/>otherwise drop this result
        end
        T->>K: emit Status (Parked, Done, Failed, or retry note)
        T->>DB: release_lease
    end
```

What the diagram cannot say:

* **One step is one model turn.** `LlmAgent::step` calls the model once (in the
  journaled step `model:N`, as a stream whose words are sent while it writes, or `complete`
  when `stream_text` is off), runs the tools that call
  asked for (each in a journaled step `tool:<call id>`), and returns
  `Continue`. Every turn is committed before the next begins.
* **The journal makes replay safe, not effects exactly-once.** A recorded
  outcome is never run again. But the effect runs *before* its outcome is
  written, so a crash in between runs it again on replay. Tools must be safe to
  repeat. The coder's tools are (see
  [The coder agent](#the-coder-agent)). A replay that asks for a different
  step name at a recorded `seq` fails the run with `NonDeterminism`.
* **Waking is polling plus hints.** An idle worker sleeps at most one
  `poll_interval` (250 ms by default), and the same `Runtime` wakes its own
  workers at once. With a `Notifier` configured
  (`RuntimeBuilder::notifier`), `start` and `deliver` publish
  `Signal::Runnable` so a worker of another process polls now, and `cancel`
  publishes `Signal::Finished` so a step of another process sees its
  `CancelToken` fire at once (`crates/adam-runtime/src/notify.rs`, `worker.rs`).
  A signal is a hint that may be lost; polling and the cancel watch stay on.
* **The lease is an optimisation.** The compare-and-swap on `version` is the
  guarantee. A worker whose lease expired mid-step cannot overwrite newer state:
  its commit is rejected and it drops its result.
* **Messages that arrive during a step** are kept. The commit merges them,
  and a step that asked to park resumes at once (`Runnable`) instead of
  sleeping through them.
* **Retry.** A transient failure does not fail the run. It is committed as
  `Runnable` with a `wake_at` in the future, and the journal entries of the
  failed try are abandoned, so the retry runs its steps afresh. See
  [The run lifecycle](#the-run-lifecycle).
* **Defaults** (`crates/adam-runtime/src/runtime.rs`, `retry.rs`): lease 30 s,
  idle poll 250 ms, 4 concurrent runs per worker loop, 5 tries per transition,
  backoff 1 s doubling to 60 s.

### Across processes: signals and events

With the front and the workers in different processes, they share only the
database. Left alone, a worker finds a new run at its next poll and a step
learns of a cancel at the next poll of its run. `adam-notify-postgres` closes
both gaps over `LISTEN`/`NOTIFY` without changing who is right: every
notification is a hint, the compare-and-swap and the polling stay in place, and
a run completes with the crate removed, only later. The processes are wired by
their composition root. `adam-coder`'s `serve` does it for every role (done, no new
variable); the crate's `tests/two_runtimes.rs` wires a front and a worker the same way.

Each process has one `PgNotify` (`crates/adam-notify-postgres/src/lib.rs`) whose
`run` holds a listener on two channels, `{prefix}events` and `{prefix}signals`,
and a publisher that drains a queue with `pg_notify`. Its `PgEventSink` is the
runtime's event sink and its `PgNotifier` the runtime's notifier.

```mermaid
sequenceDiagram
    autonumber
    participant C as A2A client
    participant F as Front<br/>RuntimeTaskBackend + Runtime
    participant FN as Front PgNotify
    participant DB as PostgreSQL<br/>runs table and NOTIFY
    participant WN as Worker PgNotify
    participant W as Worker<br/>run_worker + Agent.step

    Note over F,W: adam-service serve wires one PgNotify per process,<br/>the component notify, in every role
    C->>F: SendStreamingMessage
    F->>DB: create_run (Runnable)
    F-)FN: publish Signal Runnable (queued, never waits)
    FN-)DB: pg_notify(signals, runnable)
    DB-)WN: NOTIFY signals
    WN->>W: Delivery Signal Runnable, for an agent this worker steps
    W->>W: notify_workers, the idle loop wakes
    W->>DB: claim_due
    W->>W: Agent.step, then ctx.emit Progress
    W->>WN: PgEventSink.emit: local BroadcastSink first, then queued
    WN-)DB: pg_notify(events, origin = worker, progress)
    DB-)FN: NOTIFY events
    FN->>F: origin is not ours: emit into the front's BroadcastSink
    F-->>C: SSE status-update, from the live subscription
    W->>DB: commit_run (Parked), emit Status
    Note over F,C: a Status event makes the subscription re-read the durable run
    C->>F: CancelTask
    F->>DB: commit_run (Failed, cancelled)
    F-)FN: publish Signal Finished, after firing its own local token
    FN-)DB: pg_notify(signals, finished)
    DB-)WN: NOTIFY signals
    WN->>W: Delivery Signal Finished
    W->>W: fire_cancel(run): ctx.cancelled() resolves in the step
```

What the diagram cannot say:

* **Every arrow into the database from a notifier is best effort.** `publish` and
  `emit` only queue (1 024 items, then drop) and return; a failed `pg_notify`
  drops its item. The worker that polls would have found the run anyway,
  `Runtime::view` holds the durable status and artifacts, and the worker's cancel
  watch finds a finished run at its next poll.
* **No echo, no re-publish.** Events carry the sender's id; the listener skips
  its own and delivers others' into the local `BroadcastSink`, never back into
  the database. Signals have no origin: the publisher hears its own too.
* **Size.** A payload of 8000 bytes or more is rejected by PostgreSQL, so nothing
  over 7 999 is sent. An oversize `Status` loses the tail of its detail; any other
  oversize event stays local, and there is no events table (durable status and
  artifacts are already the run record, and replayable events would cost a write
  per event).
* **The listener holds one pooled connection** and needs a session, so no
  transaction-mode pooler in front of it.
* **Not built here:** MongoDB has no equivalent (no change streams on a standalone
  `mongod`), so it keeps polling; `adam-coder` is Postgres only and uses the crate in
  every role (`crates/adam-service/src/serve.rs`, and the coder's `binary.rs` test of a control plane
  and a worker in two processes, which sees the worker's progress in the front's stream).

The listener's lifecycle (`crates/adam-notify-postgres/src/lib.rs`, `listen_loop`
and `session`):

```mermaid
stateDiagram-v2
    [*] --> Connecting
    Connecting --> Listening: connected and LISTEN active, Resync broadcast
    Connecting --> Reconnecting: connect or LISTEN failed
    Listening --> Listening: notification dispatched
    Listening --> Resyncing: try_recv returned None, sqlx reconnected and listened again
    Resyncing --> Listening: Resync broadcast
    Listening --> Reconnecting: try_recv failed
    Reconnecting --> Connecting: after the backoff, 100 ms doubling to 10 s
    Connecting --> [*]: stop, or the pool is closed
    Listening --> [*]: stop, or the pool is closed
    Reconnecting --> [*]: stop
```

A `Resync` tells the subscribers that notifications may have been lost while the
connection was down. In the runtime (`crates/adam-runtime/src/worker.rs`) it makes
the worker poll at once and re-read every run it is stepping, firing the cancel
token of those that finished. `LISTEN` is active before the `Resync` is sent, so
what happens after the catch-up is heard.

### Steps: what a run's work looks like to a client

A tool call, the work it hands to another agent and the commands that agent runs are **steps**: `RunEvent::Step`
([ADR 0007](decisions/0007-progress-as-steps-and-streamed-text.md)). The agent reports `tool:<call id>` before and after
every call, in the style of `Tool::step_style`; a tool says more with `ToolCtx::emit_progress` and
`ToolCtx::report_step`, whose steps run under the call's. The subscription of a client that **activated `steps/v1`**
(the request named the extension and the card declares it, so `Caller::extensions` has it) turns each into a
`working` status whose message carries the report in its metadata; any other client reads the same step as a line of
text. The report of a tool call also carries what the tool was given (`input`, on the report that starts it) and what it
answered (`output`, on the one that ends it), redacted by the agent and cut to 4 KiB and 8 KiB
([ADR 0011](decisions/0011-a-tool-calls-step-carries-its-input-and-output.md)); a step too big for a Postgres
`NOTIFY` crosses between processes without them.

```mermaid
sequenceDiagram
    participant C as Client
    participant S as A2A server
    participant B as Subscription
    participant K as BroadcastSink
    participant A as LlmAgent
    C->>S: SendStreamingMessage, A2A-Extensions: steps/v1
    S->>S: Caller.extensions = named and declared by the card
    S->>B: subscribe(caller, task)
    S-->>C: header A2A-Extensions: steps/v1
    A->>K: Step tool:c1 running
    K-->>B: RunEvent::Step
    alt the caller activated steps/v1
        B->>B: admit: the first report of a state, a change of state, or an end
        B-->>C: working, one text part + the report in metadata
    else it did not
        B-->>C: working, one line of text
    end
```

```mermaid
stateDiagram-v2
    [*] --> Reported: the first report of a state of a step goes out
    Reported --> Held: the same state again within a second (only for an activated client)
    Held --> Reported: a second has passed
    Reported --> Reported: a change of state goes out at once
    Reported --> [*]: an end goes out, and the step is forgotten
```

The events are live, as every event is: a subscription attached after they were emitted, or in another process with
no event sink, sees only the durable record of the task. What is durable about steps is what the orchestration layer
logs of them (it keeps a start, a few updates and an end per step).

### Streamed text: the words as the model writes them

A model turn is a **stream** by default (`LlmAgentBuilder::stream_text`): the journaled step `model:<turn>` calls
`ModelClient::stream` and, while the model writes, sends what it has written as `RunEvent::TextDelta` events, the
pieces of one stream ([ADR 0007](decisions/0007-progress-as-steps-and-streamed-text.md)). The subscription of a client
that **activated `text-stream/v1`** turns each into a chunk (an artifact update whose artifact is the stream, with the
piece's byte offset in its metadata), and the whole text is stated once: by a `working` status for the words before a
tool call, and by the status that ends the turn: the `completed` status of the answer that ends the run, whose message
names its stream (`output.stream` in the run, `{streamId}` in the message's metadata), or the `input-required` status of
a reply the agent turned into a question (the coder's: `PendingQuestion::stream`). The orchestration layer relays the chunks to the
screens and logs only the final text.

```mermaid
sequenceDiagram
    participant C as Client
    participant B as Subscription
    participant K as BroadcastSink
    participant A as LlmAgent (step model:N)
    participant M as Model
    C->>B: SendStreamingMessage, A2A-Extensions: text-stream/v1
    A->>M: stream(request)
    M-->>A: Text deltas
    A->>A: coalesce: 200 bytes or 100 ms, at most 1024 bytes a piece
    A->>K: TextDelta(stream, offset, text)
    K-->>B: RunEvent::TextDelta
    alt the caller activated text-stream/v1
        B-->>C: artifact update: the piece, its offset, append, lastChunk
    else it did not
        B->>B: nothing: the whole reply comes with the turn
    end
    M-->>A: Finished(response)
    A->>K: TextDelta(last)
    A->>A: journal the response and the stream id, then Done with output.stream
    B-->>C: completed: the whole text, metadata {streamId}
```

```mermaid
stateDiagram-v2
    [*] --> Streaming: the first word that is not blank
    Streaming --> Streaming: a piece (200 bytes, or 100 ms since the last)
    Streaming --> Ended: the model finished: the last piece
    Streaming --> Abandoned: the model failed: the last piece, abandoned
    Ended --> Stated: output.stream (the answer), or agent_text with the stream (words before a tool call)
    Stated --> [*]
    Abandoned --> [*]: the run fails or retries as another stream
```

What the diagrams cannot say: the pieces are live and meant to be lost (a replay of a recorded step sends none, and says
the same words whole under the recorded stream id); a failure in the middle of the answer is the call's failure
(`ModelFailure`, the same retry and the same failed run as a `complete` that failed); a chunk is at most 1024 bytes so
the event fits a `NOTIFY` payload between processes; and a client that did not activate the extension, and a blocking
`message/send`, get the whole reply with the turn as ever.

## The run lifecycle

`RunStatus` (`crates/adam-core/src/store/mod.rs`) has four values: `Runnable`,
`Parked`, `Done` and `Failed`. The transitions below are the ones the runtime
makes (`runtime.rs` and `worker.rs`).

```mermaid
stateDiagram-v2
    [*] --> Runnable: start, start_with_id, start_child or a continuing start, version 1

    Runnable --> Runnable: Continue, next turn
    Runnable --> Runnable: transient error with tries left, wake_at is now plus max of backoff and retry_after
    Runnable --> Runnable: lease expired, any worker re-claims and resumes from the last commit
    Runnable --> Runnable: Park requested but a message arrived during the step
    Runnable --> Parked: Park, with a timer or waiting for a message
    Runnable --> Done: Transition Done
    Runnable --> Failed: Transition Fail
    Runnable --> Failed: Permanent, NonDeterminism, or store Corrupt or Invalid
    Runnable --> Failed: transient error, tries used up
    Runnable --> Failed: unreadable state envelope
    Runnable --> Failed: cancel

    Parked --> Runnable: deliver, an inbound message
    Parked --> Runnable: timer due, step commits Continue
    Parked --> Parked: timer due, agent parks again
    Parked --> Done: timer due, Transition Done
    Parked --> Failed: timer due, a failure as above
    Parked --> Failed: cancel

    Done --> [*]
    Failed --> [*]
```

The lease is not a `RunStatus`. It is a separate lifecycle that a run goes
through each time a worker takes it:

```mermaid
stateDiagram-v2
    [*] --> Unleased
    Unleased --> Leased: claim_due, run is due and has no unexpired lease
    Leased --> Leased: renew_lease every ttl/3 while the step runs
    Leased --> Unleased: release_lease after the commit, or after a rejected commit
    Leased --> Expired: worker died or hung, or renewal kept failing
    Leased --> Expired: store trouble while stepping, lease kept and not released
    Leased --> Expired: a step outlives its lease, for example a clock that jumped
    Expired --> Leased: claim_due by any worker (a pinned claim: only by the run's owner)
    Unleased --> [*]: run reached Done or Failed
```

How the two fit:

* **What makes a run due.** `sched_at` is derived from status and `wake_at`
  (`adam_core::store::sched_at`): `Runnable` is due at once, or at `wake_at`
  for a retry backoff. `Parked` with a `wake_at` is due then. `Parked` without
  one is never due: only `deliver` or `cancel` moves it. `Done` and `Failed`
  are never due.
* **Retries.** A step that fails with `AgentError::Transient` (or that panics,
  which is treated as transient) is committed as `Runnable` with
  `wake_at = now + delay` and `attempt + 1`. The delay is the policy's
  exponential backoff. If the error carries a `retry_after` (a provider's
  `Retry-After`), the delay is the larger of the backoff and the hint, and a
  hint is capped at 24 hours (`MAX_RETRY_AFTER`). When `attempt` reaches
  `RetryPolicy::max_attempts` the run is `Failed` with "gave up after N
  attempts".
* **A lease that lapses under a step.** The worker still holds the run in flight, so it passes it in
  every `claim_due` as `busy` and the store never gives it back to the same worker: that would lease
  the run a second time, with a snapshot that the running step is about to make stale, and the release
  at the end of the step (it matches the worker, not the claim) would clear the new lease. The worker
  releases the lease before it counts the run as free. Another worker claims the run once the lease
  has expired, and the compare-and-swap rejects the commit that comes second.
* **Store trouble.** If the store fails while a step runs, the class decides.
  `Corrupt` and `Invalid` fail the run, because a row that can never be read
  must not be leased for ever. Any other class commits nothing and keeps the
  lease, so the run is retried when the lease expires and is not hammered.
* **Cancel.** `Runtime::cancel` commits `Failed` with the error
  `cancelled: <reason>`. A step running at that moment is told through its
  `CancelToken` (at once in the same process, within one poll interval from
  another one). Its later commit is rejected by the compare-and-swap. A run that
  already finished is left as it is.
* **Deliver.** `Runtime::deliver` appends to the inbox. A `Parked` run becomes
  `Runnable` at once, even if it was waiting on a timer. A run that is `Done`
  or `Failed` answers `Finished`.
* **Children.** A run started with `Runtime::start_child` records its parent. When it reaches `Done` or
  `Failed` (by a step, by a cancel, or because its state cannot be read), the runtime delivers
  `adam.run.finished` to the parent after the commit. See [Child runs](#child-runs).
* **Terminal states** are `Done` and `Failed`. A finished run is kept until
  `Store::purge_finished` deletes it with its journal.

How a run looks to an A2A client (`task_state` in
`crates/adam-a2a-runtime/src/convert.rs`):

| Run | A2A task state |
|---|---|
| `Runnable`, version 1 (no worker has committed yet) | `submitted` |
| `Runnable`, or `Parked` with a timer | `working` |
| `Parked` with no timer | `input-required` |
| `Done` | `completed` |
| `Failed` with an error that starts `cancelled: ` | `canceled` |
| `Failed` otherwise | `failed` |

The ids an A2A client sees are derived, not random. The `message_id` of a
status message is a hash of the task id, the state and the text, so every
event and snapshot of one status carries the same id. A new task's id is a hash
of the agent, the caller, the `contextId` and the `messageId` (`task_id_for` in
`crates/adam-a2a-runtime/src/ids.rs`), started with `Runtime::start_with_id`, so
a repeated `SendMessage` reaches the task its first attempt made and the agent
reads the input once.

## Child runs

A run can start other runs and wait for them. This is what a subagent is: an agent started by a tool of
another agent, with its own history, tools and limits, whose final answer is the result of that tool call.
The runtime supplies four things and no scheduler. `Runtime::start_child(parent, id, agent, input)` creates
a run that records its parent (`RunRecord::parent_id`, which the stores have always kept). When such a run
reaches a terminal state, the runtime delivers an inbound message of kind `adam.run.finished` to the parent.
`Ctx::child_status(run)` reads a child from the store. `ToolError::AwaitRun { run }` is how a tool of an
`LlmAgent` says "my result is that run's outcome".

```mermaid
sequenceDiagram
    autonumber
    participant P as Parent run<br/>LlmAgent
    participant J as Journal<br/>tool:call id
    participant R as Runtime
    participant DB as Store
    participant C as Child run

    P->>J: step tool:c1, run the tool
    J->>R: start_child(parent, child_run_id(parent, c1), agent, input)
    R->>DB: create_run with parent_id, or AlreadyExists (a replay)
    J-->>P: Err(AwaitRun { run: child }), journaled
    P->>DB: commit Park, wake_at = now + 60 s, pending_wait = { c1, child }
    C->>DB: claim, step ... terminal commit, Done or Failed
    C->>R: after the commit: notify_parent
    R->>DB: deliver to the parent: adam.run.finished, id = child id,<br/>payload { status, output or error }
    DB-->>P: parent is Runnable, inbox holds the message
    P->>P: message matches pending_wait: it becomes the tool result
    Note over P,DB: If the message never arrives: at wake_at the parent<br/>reads the child with Ctx::child_status and answers or parks again
```

```mermaid
stateDiagram-v2
    [*] --> Calling: the model calls the tool
    Calling --> Waiting: child started or found, AwaitRun journaled, run parked with a timer
    Calling --> Answered: the child's message is already in the inbox (a replay or a retry)
    Waiting --> Answered: adam.run.finished for this child arrives
    Waiting --> Answered: timer wake, and the store says the child is terminal
    Waiting --> Answered: timer wake, and the child no longer exists (an error result)
    Waiting --> Waiting: timer wake, the child is still working (one read, new timer)
    Waiting --> Waiting: a user message or a stray notice arrives (nothing answers, new timer)
    Waiting --> Cancelled: the parent is cancelled (the child is not)
    Answered --> [*]: tool result is the child's text, or an error result
    Cancelled --> [*]
```

* **The child id is derived.** `child_run_id(parent, key)` is a UUID (version 8) from a SHA-256 over the
  parent id and the key, the same way the A2A adapter derives task ids. `ToolCtx::child_run_id()` is that
  for the current tool call. A tool that runs again, after a crash, a lost lease or a transient retry, asks
  for the same child, and `start_child` (which is `start_with_id` plus the parent) answers `false` and
  leaves the first one alone. The only side effects of the tool are that creation and reads.
* **The message is a hint, the timer is the guarantee.** The runtime sends the message after the child's
  commit, in the worker that made it. It cannot be part of that commit (the store commits one run at a
  time), so a crash or a store error in between loses it, and so does a commit whose acknowledgement is lost
  (the worker then believes the commit failed and sends nothing). The parent therefore never waits without a
  timer: `LlmAgentBuilder::wait_poll` (60 s by default) bounds how late it can be, and each wake without the
  message is one `load_run`. A failed send is logged at `warn`; a parent that is finished or gone is logged
  at `debug` and is not an error.
* **The message is deduplicated by its id.** `Inbound::id` is the child's run id. The `LlmAgent` matches it
  to the run it waits for, uses it once (the wait is cleared in the same commit that consumes it) and drops
  every other: copies of a message already used, a message for a run it does not wait for, one that does not
  parse, one whose status is not final. Such messages are never read as user text.
* **The answer.** A finished child's `output.text` (what an `LlmAgent` child ends with), else its output as
  JSON, is the tool result. A `Failed` child, a cancelled one (`cancelled: <reason>`), one whose state cannot
  be read and one that was purged are an error result (`the run failed: <why>`): the model sees it and the
  run goes on. Nothing in the parent fails because a child did.
* **Who may be asked about.** `Ctx::child_status` answers only for children of the calling run. An id that
  belongs to a run of another parent (or to none) is a permanent error, so an agent cannot read the results
  of unrelated runs, and a derived id that collided with a stranger's run would fail loudly and not answer
  with the stranger's output.
* **`Conversation::pending_wait`** is what the parent is parked on: a `Question` for the user (this field was
  `pending_question`, and state stored under that name still loads) or a `Run` for a child. A2A reads a run
  parked on a timer as `working` and only a run parked with no timer as `input-required`, so a parent
  waiting for a child is `working`.

### Failure interleavings

Written down before the code was final, and each one has a test that makes it happen without sleeping: steps
are held at gates, the 60 s timer is moved with a `ManualClock`, and a fault is scripted on one run of the
`FaultyStore` (`fail_run`, `fail_run_after_apply`). Names are in `crates/adam-runtime/tests/runtime.rs` (run
on the memory, PostgreSQL and MongoDB stores) and `crates/adam-llm-agent/tests/child_runs.rs` (same three).

| # | What happens | What the design does | Test |
|---|---|---|---|
| 1 | The child's terminal commit lands, then its process dies, or delivering to the parent fails | The parent stays parked on its timer. At the timer it reads the child, finds it terminal and answers. Nothing else knows anything is missing | `a_lost_notice_is_recovered_by_the_timer`, `a_lost_notice_is_recovered_when_the_timer_fires` |
| 2 | The child's terminal commit is applied but its acknowledgement is lost | The worker treats the commit as failed and sends nothing (the run is `Done` in the store and nobody claims it again). Same recovery as 1 | `a_lost_terminal_ack_sends_no_notice_and_the_timer_recovers`, `a_lost_terminal_ack_is_recovered_when_the_timer_fires` |
| 3 | The message is delivered twice, or three times, or again after the answer is in, or after the run is over | The first match answers the call and clears the wait. Later copies match no wait and are dropped. On a finished parent `deliver` answers `Finished`, which the sender ignores. The model is not asked again | `a_duplicate_notice_is_ignored`, `only_the_awaited_childs_notice_answers` |
| 4 | The parent is cancelled while it waits | **The child is not cancelled** (see below). It runs to its end within its own limits, its message finds a finished parent and is dropped, and the parent stays `Failed` with `cancelled: <reason>`, untouched | `cancelling_a_waiting_parent_does_not_cancel_its_child`, `cancelling_the_parent_leaves_the_child_running` |
| 5 | The child fails, is cancelled, or was purged | The parent gets `status: failed` and the reason (or, for a purged child, finds it gone) and the tool result is an error result. The run continues | `a_failed_or_cancelled_child_tells_its_parent_why`, `a_failing_child_is_an_error_result`, `a_cancelled_child_is_an_error_result`, `a_child_that_is_gone_is_an_error_result`, `a_purged_child_reads_as_gone` |
| 6 | The parent's lease is lost while it waits, or while it starts the child | Fencing is the version compare-and-swap, not the lease. The worker that took over replays the step: the journal has the start, and `start_child` would answer `false` anyway, so there is one child. The first worker's late commit is refused. The message is delivered with the same compare-and-swap, so it lands on whichever version is current. The parent resumes once. The same holds for the step that answers the call: its consumption of the message commits with it, so a worker that takes over sees the message again and answers the same way | `a_parent_that_loses_its_lease_does_not_start_or_resume_twice`, `a_worker_that_loses_its_lease_while_answering_changes_nothing` |
| 7 | The message arrives before the parent has parked | While the tool step runs: the message waits in the inbox, and the commit turns the park into a wake-up (the rule that already keeps a parked agent from sleeping through a message that arrived during its step). In a replay or a retry of that step: the snapshot the step starts from already holds the message but the wait is not recorded yet, so the agent matches it *after* the tool has returned `AwaitRun`, in the same transition, and never parks | `a_notice_that_beats_the_park_is_not_lost`, `a_notice_that_beats_the_park_is_used`, `a_retry_finds_the_notice_before_the_wait_is_recorded` |
| 8 | The timer fires and the child is still working | One `child_status` read, no answer, a new timer one interval later. No second child, no model call | `a_parent_woken_early_parks_again`, `the_parent_looks_at_the_child_when_the_timer_fires` |
| 9 | A user message arrives while the parent waits | It wakes the parent, which finds nothing to answer with and parks again. The message is kept and appended after the tool result, as for any owed result | `a_message_while_waiting_queues_behind_the_result` |
| 10 | A forged, stray or malformed `adam.run.finished` | Matched by the child's run id only, and only a final status counts. Otherwise it is dropped without becoming user text. `child_status` refuses runs that are not the caller's children | `only_the_awaited_childs_notice_answers`, `child_status_reads_only_the_callers_children` |
| 11 | Two workers start the same child | `create_run` refuses the second (`AlreadyExists`), `start_child` returns `false`. One row | `start_child_records_the_parent_and_is_idempotent` |
| 12 | The process that finishes the child has never heard of the parent's agent | Delivering needs only the parent's run id: it does not have to register the parent's agent. The parent's own worker steps it | the split `front` and `back` runtimes of cases 1, 2, 5 and 6 |

What the design refuses to do, and why:

* **No cascade on cancel (v1).** Cancelling a parent does not cancel its children. A cascade needs a way to
  list children, which the `Store` port does not have (`parent_id` is stored and indexed, and nothing reads
  it), so it is a new port method for three stores and the testkit, and it makes `cancel` more than one
  compare-and-swap. What it costs: a child whose parent is gone runs on until it finishes or hits its limits
  (`max_turns`, `max_tool_calls`), and its message is dropped. That is bounded and visible (the child is a
  normal run, listed and cancellable by id), and `docs/authoring.md` records the same choice for subagents.
  The seam is `Store::children(parent)` plus a loop in `Runtime::cancel`.
* **No parallel fan-out yet.** An `LlmAgent` runs the calls of one model turn in order, and the first
  `AwaitRun` parks the run, so three subagent calls in one turn run one after the other. Starting all the
  children first and waiting for all of them needs `pending_wait` to hold several runs.
* **No timeout on a child, and no asking tools in a subagent.** A parent waits as long as its child is open. A
  child that parks with no timer, for instance because one of its tools asked the user something, waits for
  an answer nobody is placed to give (`Limits` bound a running child, not a parked one). The subagent binding
  (`adam-assembly`) closes this at startup instead of at run time: a tool that can ask says so
  (`Tool::asks_user()`), and `bind` refuses a subagent that has one, so the situation cannot arise from an
  agent directory. A tool that returns `NeedsInput` without declaring it still parks its child, and a child
  can still be cancelled by id: the parent then gets the error result.
* **The output travels whole.** The child's output is the payload of the message, then the text of the tool
  result in the parent's history. Long histories are shortened for the model by `max_history_tokens` as for any
  tool output, but the stored conversation keeps it.
* **No guarantee for a child purged early.** `purge_finished` deletes a finished run with its journal. A
  parent that has not looked yet then reads `None` and reports "the child run no longer exists". Keep the
  retention above the parent's longest wait plus one `wait_poll`.
* **No mixed versions.** The journal entry `AwaitRun` and the `pending_wait` field are not read by a build
  that predates them (it would fail the run as non-deterministic on replay). Roll the workers before the
  tools that start children are enabled. The other direction is safe: journals and states written before
  (`ToolError` without `AwaitRun`, `Conversation` with `pending_question`) load and replay in the new build.

Code that runs inside a step but cannot hold the `Ctx`, such as a tool of an `LlmAgent`, starts a child through
`Ctx::child_starter()`: an owned `ChildStarter` for the runtime that is stepping the run, which can start only
children of that run (`start` is `start_child` with the parent fixed). `LlmAgent` hands it to every tool as
`ToolCtx::start_child(agent, message)` (under `ToolCtx::child_run_id()`), so a tool needs no `Runtime` handle,
and the child starts on the runtime that steps the parent, whichever process that is. The subagent tool of
`adam-assembly` is exactly this call followed by `AwaitRun`; see [subagents](authoring.md#the-subagent-tool-s9).

A hand-written `Agent` can wait for children too: start them with `start_child`, `Park` with a timer, and on
the next step read `take_inbox()` for messages of kind `adam.run.finished` (`ChildStatus::from_notice`) and
`Ctx::child_status` when there is none. It must take its inbox in the step that parks as well: a message
already in the inbox when a step starts is not "arrived during the step", so an agent that parks without
reading it sleeps until its timer.

### Remote tasks: the same wait without a message

A subagent on another A2A agent (`a2a:` in its file, [authoring](authoring.md#remote-subagents-a2a-s9b)) is
the same idea with one thing missing: nothing tells the parent when the remote task is over. The tool starts
the task, returns `ToolError::AwaitRemote { task, timeout_ms }`, and the parent records
`PendingWait::Remote { call_id, tool, task, deadline }` and parks with the `wait_poll` timer. Each time the
timer fires the agent asks the tool how the task stands, as a journaled step, until it is over. There is no
`adam.run.finished` here and no fallback role for the timer: **the timer is the mechanism**.

```mermaid
sequenceDiagram
    autonumber
    participant P as Parent run<br/>LlmAgent
    participant J as Journal
    participant T as RemoteSubagentTool
    participant A as Remote A2A agent
    participant DB as Store

    P->>J: step tool:c1
    J->>T: call(message)
    T->>A: SendMessage, returnImmediately, messageId = child_run_id(parent, c1)
    A-->>T: Task, working
    J-->>P: Err(AwaitRemote { task, timeout_ms }), journaled
    P->>J: now_journaled, to fix the deadline
    P->>DB: commit Park, wake_at = now + wait_poll, pending_wait = Remote { c1, tool, task, deadline }
    Note over P,DB: the timer fires and the run is claimed again
    P->>J: now_journaled, deadline not reached
    P->>J: step poll:c1
    J->>T: poll_remote(task)
    T->>A: GetTask
    A-->>T: Task, working
    J-->>P: Ok(Working), journaled
    P->>DB: commit Park, wake_at = now + wait_poll
    Note over P,DB: the next wake finds the task final
    J-->>P: Ok(Ready(result)), journaled
    P->>P: the result answers c1, the wait is cleared, the loop goes on
```

```mermaid
stateDiagram-v2
    [*] --> Calling: the model calls the tool
    Calling --> Answered: the reply is final already (a message, or a task in a final state)
    Calling --> Waiting: AwaitRemote journaled, deadline fixed, run parked with a timer
    Waiting --> Waiting: timer wake, poll:c1 says working (or a user message queued behind the result)
    Waiting --> Answered: poll:c1 says ready
    Waiting --> Answered: the deadline has passed (an error result, no poll)
    Waiting --> Answered: a permanent poll error, or the tool is gone (an error result)
    Waiting --> Waiting: a transient poll error (the wake is retried with backoff)
    Waiting --> Cancelled: the parent is cancelled (the remote task is not)
    Answered --> [*]: tool result is the tool's answer
    Cancelled --> [*]
```

* **Every look is a journal step.** `poll:<call id>` records `Working` or `Ready(result)`, and, when the wait
  has a deadline, `ctx.now` is read through the journal (`Ctx::now_journaled`) so that a replay takes the
  same branch (a replay that skipped a recorded `poll:` step because the clock had moved would meet the
  wrong step name at the next `seq` and fail as non-deterministic). A transient poll error fails the wake as
  `AgentError::Transient`, and the retry looks again from a fresh `seq`.
* **The start is idempotent by message id, not by run id.** A child run is started under an id the runtime
  refuses to create twice; a remote task is started by a `SendMessage` whose `messageId` is
  `child_run_id(parent run, call id)`. The guarantee is only as good as the remote's memory of that id
  (`adam-a2a-runtime` starts the task under `task_id_for(agent, caller, context, messageId)`, so a repeat reaches
  the same task). The journal makes the call once per recorded outcome regardless.
* **Failure interleavings.**

| # | What happens | What the design does | Test |
|---|---|---|---|
| 1 | The process dies while the parent waits | The wait is in the run's committed state. A new process claims the run at the timer and polls the recorded task: no second send | `a_restart_mid_wait_resumes_polling_without_sending_again` (`adam-assembly`), `a_new_process_keeps_polling_without_starting_the_task_again` (`adam-llm-agent`) |
| 2 | The send reaches the remote and its response is lost | The step fails transiently and is retried; the retry sends the same `messageId`, and a deduplicating remote returns the task it made | `a_send_whose_response_is_lost_is_retried_under_the_same_message_id` |
| 3 | The remote finishes while nobody polls | The next timer wake finds it final and answers | the restart case above |
| 4 | The task never ends | At the deadline (default one hour, `AgentDef::remote_timeout`) the call is answered with an error result; the remote task keeps running | `a_task_that_never_ends_is_given_up_on_after_the_limit`, `a_task_that_outlives_its_timeout_is_an_error_result_without_another_look` |
| 5 | The remote fails, cancels, rejects, or wants input | An error result naming the state and the remote's message; the run goes on | `a_remote_task_that_fails_is_an_error_result_and_the_parent_goes_on`, `a_remote_task_that_is_canceled_is_an_error_result`, `a_remote_that_needs_input_is_an_error_result_because_nobody_can_answer` |
| 6 | The remote answers 401 or the card points the token elsewhere | An error result; nothing is sent to the other origin | `a_wrong_token_is_an_error_result_not_a_failed_run_and_the_token_stays_out_of_it`, `a_card_that_points_the_token_at_another_origin_is_refused_before_anything_is_sent` |

* **What it refuses to do.** No streaming yet (`SubscribeToTask` would end the wait sooner; the poll stays as
  the fallback); no cancel of the remote task when the parent is cancelled or the wait times out (that needs a
  tool hook for cancellation); no `contextId` continuity between calls. And, as with child runs, **no mixed
  versions**: a build that predates `AwaitRemote` cannot read a journal that contains it.

## The error tree

Each library defines its own error enum with `thiserror`. A variant says **what
happened**. Its `ErrorClass` (from `adam-error`) says **what to do**. Retry loops,
the A2A error a client sees and the process exit code all decide from the class,
never from a variant. Every enum implements `Classify`, and its test matches
every variant exhaustively, so a new variant forces a class decision.

The first diagram shows which variants map to which class. A dotted arrow means
the enum wraps another and takes its class. `Unsupported` is a valid class, but
no enum in this repository maps to it today (checked by reading the 11
`impl Classify` blocks outside tests).

```mermaid
flowchart LR
    subgraph enums["Library error enums"]
        direction TB
        subgraph core_e["adam-core"]
            StoreError
        end
        subgraph rt_e["adam-runtime"]
            AgentError
            RuntimeError
        end
        subgraph model_e["adam-model and adam-model-openai"]
            ModelError
            OpenAiConfigError
        end
        subgraph ws_e["adam-workspace"]
            WorkspaceError
        end
        subgraph acp_e["adam-acp"]
            AcpError
        end
        subgraph a2a_e["adam-a2a"]
            BackendError
        end
        subgraph llm_e["adam-llm-agent"]
            ToolError
        end
        subgraph coder_e["adam-coder"]
            ConfigError
            StoppedUnexpectedly
        end
    end

    subgraph classes["adam_error::ErrorClass"]
        direction TB
        Transient
        RateLimited
        Conflict
        Invalid
        NotFound
        Rejected
        Unauthenticated
        Unsupported["Unsupported (none map here)"]
        Corrupt
        Internal
    end

    StoreError -->|"Backend chosen by adapter: unavailable"| Transient
    StoreError -->|Conflict| Conflict
    StoreError -->|InvalidInput| Invalid
    StoreError -->|NotFound| NotFound
    StoreError -->|"AlreadyExists, ConversationBusy"| Rejected
    StoreError -->|"Corrupt, NonDeterminism, Backend corrupt_source"| Corrupt
    StoreError -->|"Backend internal"| Internal

    AgentError -->|"Transient, no retry_after"| Transient
    AgentError -->|"Transient with retry_after"| RateLimited
    AgentError -->|Permanent| Invalid
    AgentError -->|NonDeterminism| Corrupt
    StoreError -.->|"Store(e)"| AgentError

    RuntimeError -->|UnknownAgent| Invalid
    RuntimeError -->|NotFound| NotFound
    RuntimeError -->|"Finished, ConversationBusy"| Rejected
    RuntimeError -->|Corrupt| Corrupt
    RuntimeError -->|Contended| Conflict
    AgentError -.->|"Agent(e)"| RuntimeError
    StoreError -.->|"Store(e)"| RuntimeError

    ModelError -->|RateLimited| RateLimited
    ModelError -->|Transient| Transient
    ModelError -->|"ContextLength, InvalidRequest"| Invalid
    ModelError -->|Auth| Unauthenticated
    ModelError -->|Protocol| Corrupt
    OpenAiConfigError -->|"InvalidBaseUrl, InvalidHeader, InvalidApiKey"| Invalid
    OpenAiConfigError -->|Client| Internal

    WorkspaceError -->|Auth| Unauthenticated
    WorkspaceError -->|NotFound| NotFound
    WorkspaceError -->|Invalid| Invalid
    WorkspaceError -->|Transient| Transient
    WorkspaceError -->|RateLimited| RateLimited
    WorkspaceError -->|Conflict| Rejected
    WorkspaceError -->|Corrupt| Corrupt
    WorkspaceError -->|"Git, Http, Io"| Internal

    AcpError -->|"Exited, Timeout"| Transient
    AcpError -->|AuthRequired| Unauthenticated
    AcpError -->|"Config, Rpc -32602"| Invalid
    AcpError -->|Protocol| Corrupt
    AcpError -->|"TurnInProgress, Closed"| Rejected
    AcpError -->|"Spawn, other Rpc"| Internal

    BackendError -->|TaskNotFound| NotFound
    BackendError -->|NotCancelable| Rejected
    BackendError -->|InvalidParams| Invalid
    BackendError -->|Unavailable| Transient
    BackendError -->|Internal| Internal

    ToolError -->|Transient| Transient
    ToolError -->|Permanent| Invalid
    ToolError -->|NeedsInput| Rejected

    ConfigError --> Invalid
    StoppedUnexpectedly --> Internal
```

The second diagram shows what each class decides. The full table (with the
"alert" column and the exact messages) is the **Errors** section of the
[root README](../README.md#errors). It is the reference, so this page does
not repeat it.

```mermaid
flowchart LR
    subgraph classes["ErrorClass"]
        direction TB
        Transient
        RateLimited
        Conflict
        Invalid
        NotFound
        Rejected
        Unauthenticated
        Unsupported
        Corrupt
        Internal
    end

    Transient & RateLimited & Conflict --> retry["is_retryable: retry<br/>RetryPolicy backoff, or wake at retry_after"]
    Corrupt & Internal --> alert["should_alert: tell an operator"]

    Transient & RateLimited & Conflict --> a1["A2A -32603<br/>backend temporarily unavailable"]
    Invalid & Rejected --> a2["A2A -32602<br/>invalid params"]
    NotFound --> a3["A2A -32001<br/>task not found"]
    Unauthenticated & Unsupported & Corrupt & Internal --> a4["A2A -32603<br/>internal error"]

    Transient & RateLimited & Conflict --> x1["exit 69<br/>EX_UNAVAILABLE"]
    Invalid --> x2["exit 78<br/>EX_CONFIG"]
    Corrupt & Internal --> x3["exit 70<br/>EX_SOFTWARE"]
    NotFound & Rejected & Unauthenticated & Unsupported --> x4["exit 1"]
```

Where each decision is made:

* **Retry** is in `adam-runtime` (`worker.rs`). The model, workspace and ACP
  errors reach it through the agent: a retryable one becomes
  `AgentError::Transient` (directly in `LlmAgent` for a model error, or through
  `ToolError::Transient` for a tool), carrying its `retry_after`. Anything else
  fails the call. In the coder, a permanent tool failure is an error result
  the model sees, and the run goes on.
* **Conflict** retries at once and is bounded: the runtime's commit, deliver,
  cancel and start loops try up to 16 times (`MAX_COMMIT_RETRIES`). Then
  `deliver`, `cancel` and `start` report `Contended`, and a worker drops its
  result with a warning.
* **The A2A code** is chosen at the trust boundary in two steps. First
  `adam-a2a-runtime` maps a `RuntimeError` **by class** to a `BackendError`
  (`map_err` in `crates/adam-a2a-runtime/src/backend.rs`): `NotFound` to
  `TaskNotFound`, `Invalid` and `Rejected` to `InvalidParams`, the three
  retryable classes to `Unavailable`, the rest to `Internal`. Then `adam-a2a`
  maps each `BackendError` to a code (`crates/adam-a2a/src/backend.rs`). The
  client gets a fixed sentence. The cause chain goes to the log, never to the
  client. `-32002` (task cannot be canceled) comes from
  `BackendError::NotCancelable`, which the backend raises when `CancelTask`
  targets a task that is finished and not already canceled.
* **The exit code** is chosen by `adam_coder::exit_code`
  (`bin/adam-coder/src/exit.rs`), which is `adam_service::exit_code_with`
  (`crates/adam-service/src/exit.rs`) plus the coder's own errors (a workspace, the agent
  files). It walks the `anyhow` chain from the
  outside in and takes the first match. A `ConfigError` is 78. A typed error
  (`StoreError`, `OpenAiConfigError`, `WorkspaceError`, `RuntimeError`,
  `StoppedUnexpectedly`) is decided by its class, as in the diagram. A panicked
  task is 70. A plain `std::io::Error` (a port that cannot bind, a directory
  that cannot be created) is 71. Anything else is 1. The process logs one
  structured line, `adam-coder failed`, with the whole cause chain and none of
  the process's secrets. The values are BSD `sysexits.h` numbers (*unverified*,
  see the end of this page).

Two rules keep this tree honest (details in the
[`adam-error` README](../crates/adam-error/README.md)):

* A message describes its own layer only, and the lower error is the `source`.
  `adam_error::report(&e)` prints the chain (`a: b: c`) once, and only where an
  error is flattened: the journal, a response to a client, a log line.
* Library crates use `thiserror`. Only binaries use `anyhow`.

## The coder agent

`adam-coder` turns a coding task into a pull request. A client sends
"in repository X, do Y". The agent makes the change in a private git worktree
(a slot of the run's workspace, which may hold other repositories too), runs the project's
own checks, and opens a pull request. It is an `LlmAgent`
with ten tools and one extra rule, running on the durable runtime and served
over A2A.

### What it does

The model decides the order of the tools. The tools enforce the rules, so the
rules hold even if the model ignores its prompt.

```mermaid
sequenceDiagram
    autonumber
    participant C as A2A client
    participant A as coder agent<br/>LlmAgent + tools
    participant M as model gateway<br/>OpenAI-compatible
    participant G as Workspaces<br/>git CLI
    participant W as worktree files<br/>read_file, write_file, apply_patch
    participant O as OpenCode<br/>opencode acp
    participant Sh as sh -lc<br/>project checks
    participant R as git remote
    participant H as GitHub API

    C->>A: SendStreamingMessage "in repo X (base main), do Y"
    loop each model turn
        A->>M: stream(history + the tool specs)
        M-->>A: text deltas, then the response: text or tool calls
    end
    Note over A,M: The turns below are the model's tool calls, in the order it picks.

    A->>G: prepare_workspace(repo_url, base_branch)
    G->>G: check the host against ALLOWED_REPO_HOSTS
    G->>R: git fetch --prune origin (token in the env of this one call)
    G->>G: git worktree add, new branch agent/short-run-id from origin/base
    A-->>C: progress: worktree ready
    Note over A,G: a second repository the person named is a second slot of the run's workspace, and the tools then say which one with repo
    Note over A,G: with no repository named, start_scratch makes a scratch slot to build and check in (commit_and_push there is local), and once the person names a repository, publish_scratch copies the files into its slot (an empty repository first gets an empty first commit) and the checks that ran on the same tree carry over

    alt a small, well-located change
        A->>W: read_file(path), then write_file(path, content) or apply_patch(diff)
        Note over A,W: every path is confined to the worktree, and a patch is checked by the paths git reads from it
        W-->>A: the text read, or the files changed
        A-->>C: progress lines: read README.md, patched README.md
    else a broad, multi-file change
        A->>O: delegate_to_opencode(instructions)
        O->>M: its own model calls, same gateway
        O-->>A: ACP updates: text, plan, tool calls
        A-->>C: progress lines
        O-->>A: TurnEnded, then the files that changed
    end

    loop until green, or the check cycles are used up
        A->>Sh: run_checks(command), with a time limit
        Sh-->>A: exit code and output tail
        A-->>C: artifact "checks"
        opt exit code is not 0
            A->>W: apply_patch or write_file (or delegate_to_opencode) to fix the failure
        end
    end

    A->>G: commit_and_push(message)
    G->>G: git add -A, git commit
    G->>R: git push origin agent/short-run-id (never forced)
    A-->>C: artifact "checks" (bound to the pushed commit)
    A-->>C: artifact "branch"

    A->>A: open_pull_request guard: HEAD is pushed, and the last check passed on this exact tree
    A->>H: find an open PR for the branch, else POST the pull request
    H-->>A: number and URL
    A-->>C: artifact "pull_request"

    opt the model needs a decision from the user
        A-->>C: input-required (ask_user question)
        C->>A: SendMessage with taskId (the answer)
    end
    A-->>C: completed, with the pull request as an artifact
```

The `pull_request` artifact has two parts: a data part with the JSON
(`url`, `number` as a string, `branch`, `repository`) and, after it, an A2A
`url` part (`Part.url`, A2A v1) with the pull request's URL, so a chat surface
shows a link. The mapping is `adam_a2a_runtime::artifact_of`, which does this
for any artifact whose data is an object with an absolute `http(s)` `url`; the
`branch` artifact has no such field and stays a single data part.

Every `run_checks` that ran its command also reports an artifact `checks` (`passed`, `commit`, `tree`, optional
`summary` and `findings`), and `commit_and_push` emits, before `branch`, a `checks` bound to the pushed commit:
the last run's report if it ran on the tree that was pushed, else `passed: false` with a finding saying the
pushed tree was not checked. An orchestrator gates on the last `checks` whose `commit` is the pushed SHA. The
schema, caps, binding rule and redaction are in the [coder README](../bin/adam-coder/README.md#artifacts).

A file the coder made is shared with `share_file`, and its artifact is different in kind: one A2A `raw` part (the bytes, base64
in JSON) with `mediaType` and `filename`, which any A2A client reads as a file ([ADR 0012](decisions/0012-files-as-a2a-artifacts.md)).
`adam_runtime::Artifact` has two forms, JSON and file; the file's bytes are journaled with the run (capped at 4 MiB a file and
6 MiB a run) and never reach the model, which is told one line. How the tool, the journal and the A2A server hand a file over
is the diagram in the ADR; the coder's side is in the [coder README](../bin/adam-coder/README.md#sharing-a-file).

The same flow as states, from the point of view of the run notes and the
tools' guards:

```mermaid
stateDiagram-v2
    [*] --> NoWorkspace
    NoWorkspace --> NoWorkspace: prepare_workspace refuses a repository the person did not name, the model asks
    NoWorkspace --> WorktreeReady: prepare_workspace on a repository the person named
    NoWorkspace --> ScratchReady: start_scratch, no repository is named
    ScratchReady --> Edited: write_file, apply_patch or delegate_to_opencode, a local commit with commit_and_push
    ScratchReady --> InputRequired: the model asks which repository to publish to
    ScratchReady --> WorktreeReady: publish_scratch to a repository the person named
    WorktreeReady --> WorktreeReady: prepare_workspace on another repository the person named, a new slot
    WorktreeReady --> Edited: write_file, apply_patch or delegate_to_opencode
    Edited --> ChecksGreen: run_checks passes
    Edited --> ChecksRed: run_checks fails, one cycle used
    Edited --> Pushed: commit_and_push without a green check, unless the budget is used up
    ChecksRed --> Edited: the file tools or delegate_to_opencode to fix, cycles left
    ChecksRed --> Exhausted: failed runs reach MAX_CHECK_CYCLES
    ChecksRed --> Pushed: commit_and_push, unless the budget is used up
    ChecksGreen --> Pushed: commit_and_push
    Pushed --> Edited: more changes, the green run no longer covers the tree
    Pushed --> PullRequest: open_pull_request, checks green on the pushed tree
    Pushed --> PullRequest: accept_red_checks after the user agreed through ask_user
    PullRequest --> Completed: the model ends its turn
    Exhausted --> FailedRun: the model reports the findings and stops, the run fails
    NoWorkspace --> InputRequired: the model stops with nothing delivered, or asks
    WorktreeReady --> InputRequired: the model stops with no pull request
    Edited --> InputRequired: the model stops with no pull request
    ChecksGreen --> InputRequired: the model stops with no pull request
    ChecksRed --> InputRequired: the model stops with no pull request, cycles left
    Pushed --> InputRequired: the model stops with no pull request
    InputRequired --> NoWorkspace: the person answers, nothing prepared yet
    InputRequired --> Edited: the person answers, work in progress
    InputRequired --> Canceled: CancelTask
    Completed --> [*]
    FailedRun --> [*]
    Canceled --> [*]
```

`Exhausted` has no way out except failure: `run_checks`, `commit_and_push` and
`open_pull_request` all refuse, and `accept_red_checks` never overrides it.
`InputRequired` is the chat waiting for the person (an `ask_user`, or a stop that delivered nothing): the
run ends only with a pull request, a failure, `CancelTask` or `max_turns`, and the person may answer, say
something else or stop it. The states are not stored as an enum:
they follow from the per-run notes (failures counted, last check and its tree,
pushed sha, pull request) and the worktree.

What the diagrams cannot say (`bin/adam-coder/src/`):

* **The tools** (`tools/`): `prepare_workspace`, `start_scratch` and `publish_scratch` (a scratch project to start in
  before a repository is named, and its copy into the repository the person names later: see
  [the coder README](../bin/adam-coder/README.md#scratch-projects)), `run_command` (looking around: no check, no cycle,
  changes to HEAD, the branch, the working tree, refs and git configuration are undone), `read_file`, `write_file` and
  `apply_patch` (small changes made in the coder's own process, confined to the worktree: see
  [the coder README](../bin/adam-coder/README.md#reading-and-changing-files-itself)), `delegate_to_opencode`, `run_checks` (the project's own checks only),
  `commit_and_push`, `open_pull_request` and `ask_user`, then the screen's `show` and `ui_catalog` (all three
  of `adam-ui`: `Ui::tools()`).
* **The prompt and the card** (`agent/instructions.md`, embedded by `build.rs`, or read at startup from the
  folder `ADAM_AGENT_DIR` names): the agent says its name (`vars.display_name`, `Coder`; the card says it too),
  answers a greeting with a greeting and "what can you do?" in plain words (adam-rs#55); the system prompt with its `{{max_check_cycles}}`, the loop's limits and the
  A2A card are a file, not Rust; `CoderAgent::new` puts the file, the tools, the `ToolEnv` state and the
  model together with `AgentDef`, and keeps only the completion policy in Rust. `serve` reads the files
  first, for every role (`AgentFiles::load`: the folder, else the embedded copy), logs the `agent files`
  line and refuses a folder with mistakes (exit 78); the control plane serves the folder's card
  (`agent_card_from`), the workers assemble from it (`CoderAgent::try_from_files`; they connect the
  servers of the folder's `mcp.json` first, `AgentDef::connect_mcp` under the `MCP_ALLOW_*` policy, and
  assemble with `CoderAgent::try_from_def`), and a folder's subagents are registered beside the coder. A restart applies an edit; there is no hot reload
  ([ADR 0004](decisions/0004-agent-folders-at-run-time.md)).
* **Rules in code.**
  * `prepare_workspace` and `publish_scratch` accept only a **granted** repository: one the person named, or
    one the person agreed to add when `request_repository` asked
    ([ADR 0008](decisions/0008-a-workspace-holds-several-repositories.md), decision 9). The question is the tool's
    own (it names the repository and quotes the model's reason), the grant is recorded by the agent from the
    person's answer to that call and from nothing the model says, and an explicit no is remembered so that it is not
    asked again (any other message, `wait` or `?`, records nothing and the question can be asked again). See [`bin/adam-coder`](../bin/adam-coder/README.md#another-repository-only-with-the-persons-yes).
  * `create_repository` makes a new, **empty** repository (private unless asked otherwise) for an owner `CREATE_REPO_OWNERS`
    names, only after the person says yes to a question the tool writes, once per owner, name and visibility; the
    repository it makes is granted. A GitHub App creates for organisations only. See
    [`bin/adam-coder`](../bin/adam-coder/README.md#a-repository-of-its-own-on-request).
  * After `MAX_CHECK_CYCLES` (default 3) failed check runs, `run_checks`
    refuses to run. `commit_and_push` and `open_pull_request` refuse too.
  * `open_pull_request` refuses unless the pushed `HEAD` is the current commit
    and the last check run passed **on exactly the tree it contains**. A run that
    continues a pushed branch pushes to a branch of its own, and only after this
    gate does `open_pull_request` fast-forward the continued branch (never forced), so the pull
    request that is open for it never carries unverified commits.
  * **A workspace of several slots** ([the coder README](../bin/adam-coder/README.md#the-workspace-of-a-run)):
    `prepare_workspace` on a second repository the person named adds a slot (a repository has at most one
    slot per run); `run_command`, the file tools, `delegate_to_opencode`, `run_checks`, `commit_and_push` and
    `open_pull_request` take an optional `repo` (the slot's directory or the repository's address), which
    may be left out while there is one slot and is refused with the list of slots when there are several.
    A check and a push are of one slot; the gate does not change: `open_pull_request` wants the most recent
    check of the pushed tree, whichever slot ran it, and the run notes keep the last 32 check records for it.
  * **A scratch project** is temporary and has no remote: `commit_and_push` there is a local commit with no `branch`
    and no bound `checks`, and `open_pull_request` refuses. `publish_scratch` works only on a repository the person
    named (the rule of `prepare_workspace`), gives an empty repository an empty first commit (the only push outside
    `agent/*`), asks for a `path` or `overwrite` for one that has files, and copies all or nothing. The gate does not
    change: the checks that ran on the project bind the pushed commit only when its tree is the same.
  * The file tools (`read_file`, `write_file`, `apply_patch`) refuse a path that is empty, absolute, goes up with `..`,
    names `.git` (any case), leaves the worktree through a symlink (read) or goes through a symlink (write); a patch is
    checked by the paths `git apply --numstat -z` reports and refused if it creates a symlink or a submodule; a hunk that
    does not match changes nothing. They change files and not git: the commit that follows is bound to a check only
    after a new `run_checks`.
  * A command the shell cannot find (exit 127, `not found`) is a missing toolchain: reported to the model,
    no check cycle used, no `checks` artifact, and the model asks the person and waits.
  * A run that stops with no pull request fails if the check-cycle budget is
    used up with the last check red, or the credentials were rejected
    (`CoderAgent::verdict`). "The model said it
    is done" is not the same as "delivered".
  * Any other stop without a pull request is a question, not a completion: the
    run parks as `ask_user` would (`input-required`, the model's text as the
    question) and the person's answer resumes it.
  * `prepare_workspace` and `publish_scratch` refuse a repository the person did not name in their
    own messages of the run (recorded in the run notes before each step from the
    conversation, never from the model's argument alone; text quoted in
    `untrusted` fences does not count), with a tool error that sends the model
    to `ask_user`.
* **Safe to repeat.** A tool call that dies before its result is journaled
  runs again, so each tool is safe to repeat. `prepare_workspace` reuses the
  run's slot of that repository, `start_scratch` returns the project of that name, `publish_scratch` finds
  the repository's slot (a repository that holds only the empty first commit it was given is not "a repository
  with files") and skips the files that are already what the project has,
  `commit_and_push` does nothing when there is nothing new,
  `open_pull_request` returns the open pull request of the same branch, and
  failed checks are counted per call id.
* **Where state lives.** Conversation, journal and run state are in Postgres.
  The mirrors, the workspaces of runs (`<WORKSPACE_ROOT>/workspaces/<run>/<slot>`) and per-run notes
  (`<WORKSPACE_ROOT>/coder/<run>.json`) are files under `WORKSPACE_ROOT`. Git is
  the durable artifact: a lost database loses the run ledger, not the pushed
  branches or the pull requests.
* **The janitor.** A workspace lives as long as its run
  ([ADR 0008](decisions/0008-a-workspace-holds-several-repositories.md)). `Janitor`, a worker component of the
  host (`Agents::worker_component`, so in the `all` and `worker` roles), sweeps at startup and every
  `WORKSPACE_SWEEP_SECS` (300; `0` is off): the workspace of a run that is `done` or `failed` (a cancel
  included), or that the store does not know, is removed, every slot of it, after what the run's environment
  holds is released (and a release that fails leaves the workspace for the next sweep); the workspace of a run that is
  `runnable` or `parked` stays, however long the person takes to answer. The notes and the `agent/*`
  branches in the mirrors stay: they are the only copy of an unpushed commit. A store that does not answer
  leaves the workspace alone, and a failed removal is logged and tried again at the next sweep.
* **Secrets.**
  * The git token reaches `git` only through the environment of a single
    invocation, never in a remote URL or `.git/config`, and only for hosts on
    the allow-list (`ScopedToken`, or `HostScoped` over `GitHubApp`, plus `Workspaces::allow_hosts`).
  * **A token or a GitHub App installation, never both**
    ([ADR 0009](decisions/0009-github-per-installation-read-through-mcp.md)): `GITHUB_TOKEN`, or
    `GITHUB_APP_ID`, `GITHUB_APP_INSTALLATION_ID` and a private key (a file, or the PEM in a variable). The
    key is parsed at startup. `GitHubApp` signs a short JWT with it and trades it, at
    `{GITHUB_API_URL}/app/installations/{id}/access_tokens`, for an installation token, kept until five minutes
    before it expires and minted again by one caller at a time. `RedactingCredentials` hands every token it
    gets to the shared `Redactor`, so a minted token is a secret from the moment it exists. The sequence and the
    states of the cached token are in the ADR and in the
    [`adam-workspace` README](../crates/adam-workspace/README.md#github-app-credentials).
  * **GitHub is read through the official GitHub MCP server, read-only**
    ([ADR 0009](decisions/0009-github-per-installation-read-through-mcp.md), decision 8). The coder's shipped
    `agent/mcp.json` starts `github-mcp-server stdio --read-only` as a child process (the image carries it, pinned
    by tag and digest; the coder's deployment, not the image, sets `MCP_ALLOW_STDIO=true`), hands it the coder's own credentials by the names it reads
    (`GITHUB_TOKEN` as `GITHUB_PERSONAL_ACCESS_TOKEN`, or the App's id, installation and key *file*; the other mode
    is an empty variable, which the server counts as unset) and offers the model twelve of its tools as
    `github__<name>`. Everything that writes stays the coder's own, behind the gate. The dev stack points the coder at
    a WireMock of the server's HTTP endpoint instead (`dev/coder-agent/mcp.json`, `mock-github-mcp`). See
    [`bin/adam-coder`](../bin/adam-coder/README.md#github-over-mcp-read-only).
  * OpenCode's child process gets `MODEL_API_KEY` through its environment (its
    config says `{env:MODEL_API_KEY}`, so the key is not inlined). `GITHUB_TOKEN`,
    `GITHUB_APP_PRIVATE_KEY`, `DATABASE_URL` and `A2A_BEARER_TOKENS` are blanked in the child (they are the names the
    description of the process asks its environment to hide; a description never carries a value).
  * A `Redactor` scrubs the process's own secrets from every tool result, event
    and failure text.
* **Limits** (`limits:` in `agent/instructions.md`): 200 model turns, 400 tool calls, 8192 output
  tokens per call, 100,000 tokens of history sent to the model. A limit that
  trips fails the run.
* **Cancel.** When a run is cancelled while OpenCode works, the tool sends ACP
  `session/cancel`, waits 2 seconds, then kills OpenCode and its process group and tells the run's
  environment which command to kill (nothing more to do in `Local`).
* **Configuration** is environment variables only. The table is in
  `bin/adam-coder/src/config.rs` and the
  [crate README](../bin/adam-coder/README.md). Every problem is reported at
  once as `invalid configuration`.

### How it is deployed

One container, one process, one database. The image is built by
`docker/coder/Dockerfile` and installed by the Helm chart in `deploy/coder`.

```mermaid
flowchart LR
    orch["Orchestrator<br/>namespace another-agentic-system"]
    gw["OpenAI-compatible<br/>model gateway"]
    remote["Git remote<br/>github.com"]
    ghapi["GitHub REST API"]

    subgraph k8s["Kubernetes: Helm chart deploy/coder"]
        np["NetworkPolicy<br/>ingress only from the orchestrator namespace"]
        svc["Service<br/>ClusterIP :8080, no Ingress"]
        subgraph pod["Pod: StatefulSet, 1 replica, uid 10001"]
            tini["tini (PID 1)<br/>forwards SIGTERM, reaps children"]
            coder["adam-coder<br/>A2A server + workers"]
            oc["opencode acp<br/>child process"]
            kids["sh, git<br/>child processes"]
            tini --> coder
            coder --> oc
            coder --> kids
        end
        pvc[("PVC at /work<br/>mirrors, workspaces, notes")]
        cnpg[("CloudNativePG cluster<br/>Postgres: runs and journal")]
        secret["ExternalSecret to Secret<br/>MODEL_API_KEY, GITHUB_TOKEN (not for role control-plane, nor with github.auth=app), A2A_BEARER_TOKENS"]
    end

    orch -->|"A2A JSON-RPC + bearer token"| svc
    np -.->|guards| svc
    svc --> coder
    coder --- pvc
    coder -->|"DATABASE_URL"| cnpg
    secret -.-> coder
    coder -->|"chat completions"| gw
    oc -->|"chat completions"| gw
    coder -->|"git fetch, git push"| remote
    coder -->|"find and open pull request"| ghapi
```

With `topology: split`:

```mermaid
flowchart LR
    orch["Orchestrator<br/>namespace another-agentic-system"]
    gw["OpenAI-compatible<br/>model gateway"]
    remote["Git remote and GitHub REST API"]

    subgraph k8s["Kubernetes: Helm chart deploy/coder, topology split"]
        svc["Service<br/>ClusterIP :8080, selects the front"]
        front["Deployment: front, front.replicas<br/>ROLE=control-plane, no volume"]
        worker["StatefulSet: worker, 1 replica<br/>ROLE=worker, /healthz only"]
        pvc[("PVC at /work")]
        cnpg[("CloudNativePG cluster<br/>Postgres: runs and journal")]
    end

    orch -->|"A2A JSON-RPC + bearer token"| svc
    svc --> front
    front -->|"DATABASE_URL"| cnpg
    worker -->|"DATABASE_URL"| cnpg
    worker --- pvc
    worker -->|"chat completions"| gw
    worker -->|"git, pull requests"| remote
```

Facts about the deployment (`docker/coder/Dockerfile`, `deploy/coder/`):

* **Image.** Two stages. The first compiles `adam-coder` on `rust:1.94-trixie`
  (the base image is pinned by tag and digest) so that its glibc matches the
  runtime image. The second is the `workspace` image of
  `vymalo/another-agentic-images` (Rust, Flutter/Dart, Node, git, tini,
  OpenCode), pinned by an immutable tag. The last `RUN` is a smoke test as uid
  10001. The entrypoint is `tini -- adam-coder`. It listens on `0.0.0.0:8080`.
* **Topology.** `topology: combined` (the default, the diagram above) is one StatefulSet
  running `adam-coder` as `all`. `topology: split` deploys the two halves as separate
  workloads (diagram below): a front `Deployment` (`ROLE=control-plane`, no volume, replicas
  from `front.replicas`) that the Service selects, and the worker `StatefulSet`
  (`ROLE=worker`), which keeps the combined StatefulSet's name, selector and volume claim, so
  switching topology reuses the same PVC. The front holds only `DATABASE_URL` and the A2A
  tokens; the worker holds the model and GitHub secrets and the volume. Deploy-only: no
  binary changed. Verified 2026-09-29: `bin/adam-coder/src/config.rs` requires
  `A2A_BEARER_TOKENS` and `PUBLIC_URL` only for the roles that serve A2A.
* **More than one worker needs a placement.** Runs move between workers at every step
  (`adam-runtime`'s worker), while a workspace lives in one worker's `/work`. A second worker
  that does not have a run's workspace would continue it on a checkout that is not there, and
  fork it into a second pull request. The chart therefore refuses `replicaCount > 1` for the
  roles that run workers until `workspace.placement` is set
  ([ADR 0002](decisions/0002-workspace-placement.md)), and passes it to the binary as
  `WORKSPACE_PLACEMENT` (with `WORKER_ID` from the pod name, the downward API, for the
  placements that pin runs): `isolated` keeps a PVC per pod, `affinity` mounts one
  ReadWriteMany claim and the coder keeps a folder per worker in it, `shared` mounts the same
  claim and lets any worker step any run (guarded by the mirror lock). `a2a-only` is refused.
  See *Where a run's files live: workspace placement*. The front scales freely: it is
  stateless.
* **Probes** hit `/healthz`. Graceful shutdown gets 120 seconds: on SIGTERM the
  workers finish the steps they are in. A step cut short by SIGKILL is taken
  over by the next start once its lease expires.
* **No Ingress.** The orchestrator reaches the Service inside the cluster with
  a bearer token. The chart's `NetworkPolicy` allows ingress only from the
  orchestrator's namespace. It is enforced only if the cluster's CNI enforces
  network policies.
* **Secrets** come from an `ExternalSecret` (AWS Secrets Manager through External
  Secrets). The database URL is the `uri` key of the Secret that CloudNativePG
  creates. With `config.role=control-plane` the chart renders neither
  `MODEL_API_KEY` nor `GITHUB_TOKEN` (nor the model, GitHub and workspace
  settings): the control plane holds none of them. `all` and `worker` need both.
  With `github.auth: app` the roles that run workers get no `GITHUB_TOKEN` either: they get
  `GITHUB_APP_ID`, `GITHUB_APP_INSTALLATION_ID` and `GITHUB_APP_PRIVATE_KEY_PATH`, and the App's private key
  is a Secret you manage (`github.app.privateKeySecret`, key `private-key.pem`) mounted read-only at
  `/var/run/secrets/github-app`. The chart carries no key, and a control plane has no GitHub setting or key volume.
* **Known risks** (stated in the chart README): no database backups, a pinned run whose
  worker never returns is stranded, and `flock` on NFS or Longhorn RWX is unverified.

How a change reaches the chart (`.github/workflows/coder.yml`):

```mermaid
sequenceDiagram
    autonumber
    participant Dev as Pull request
    participant CI as coder.yml
    participant GH as ghcr.io
    participant Chart as deploy/coder/values.yaml

    Dev->>CI: helm lint and template, kubeconform, hadolint, shellcheck
    Dev->>CI: build the image and run the container smoke test
    CI-->>Dev: green
    Dev->>CI: merge to main
    CI->>CI: build the image and smoke-test it again
    CI->>GH: push ghcr.io/vymalo/another-adam-rs/coder, tag sha-XXXXXXX
    CI->>Chart: set image.tag, commit "chore(deploy): bump coder to sha-XXXXXXX"
```

The workflow ends at the commit. How the chart is then applied to the cluster
is outside this repository (*unverified* here).

### The local Compose stack

`compose.yaml` runs the databases, WireMock stand-ins for the external systems,
a local git remote, and, under the `app` profile, the coder wired to all of
them. Ports bind to `127.0.0.1` and every credential is a dummy. The
[root README](../README.md#local-development) has the ports, the environment
variables, and the scenario switches of each mock.

```mermaid
flowchart LR
    subgraph host["Host"]
        cargo["cargo test<br/>ADAM_TEST_* variables"]
        curl["curl, an A2A client"]
    end

    subgraph stack["Compose project adam-rs"]
        pgs[("postgres :5432<br/>database adam_test")]
        mongos[("mongodb :27017<br/>standalone")]
        moai["mock-openai :8081<br/>WireMock, chat completions"]
        mogh["mock-github :8082<br/>WireMock, pull requests, GitHub App token trade"]
        gitsrv["git-server :8083<br/>nginx + git-http-backend<br/>local/sandbox.git"]
        cdr["coder :8080<br/>profile app, built from docker/coder/Dockerfile"]
        agentdir[/"bin/adam-coder/agent<br/>mounted read-only at /etc/adam/agent"/]
        gen["agent :8084<br/>profile app, the same image, entrypoint adam-agent"]
        genagentdir[/"dev/agents/assistant/agent<br/>mounted read-only at /etc/adam/agent"/]
    end

    curl -->|"A2A, bearer dev-token"| cdr
    cdr -->|"DATABASE_URL"| pgs
    cdr -->|"MODEL_BASE_URL"| moai
    cdr -->|"GITHUB_API_URL"| mogh
    cdr -->|"repository in the task"| gitsrv
    agentdir -->|"ADAM_AGENT_DIR, read at startup"| cdr
    curl -->|"A2A, bearer dev-token"| gen
    gen -->|"DATABASE_URL, runs scoped by agent name"| pgs
    gen -->|"MODEL_BASE_URL, model mock-assistant"| moai
    genagentdir -->|"ADAM_AGENT_DIR, read at startup"| gen

    cargo --> pgs
    cargo --> mongos
    cargo --> moai
    cargo --> mogh
```

* `mongodb` is used by the store tests only. The coder does not use it.
* The coder reads its prompt and card from `bin/adam-coder/agent`, mounted at `/etc/adam/agent`
  (`ADAM_AGENT_DIR`; `CODER_AGENT_DIR` points the mount at a copy). An edit applies with
  `docker compose --profile app up -d coder`, no rebuild. `dev/greeting-e2e.sh` runs "hi" through the stack (the
  scripted `mock-coder` greets from the two persona lines of the prompt: its name and a one-sentence summary), lets
  the same task go on to a pull request, and restarts the coder on an edited copy of the folder.
  `dev/coder-choices-e2e.sh` asks the coder, with the screen's catalog in the message's metadata, for three questions
  at once: the status carries the question and an `application/a2ui+json` form, the person's answers go back as one
  A2UI action and the coder's next words quote them (the mock `mock-coder` scripts it for a task that carries
  `[mock:choices]`); on a screen it cannot read the options are text ([ADR 0006](decisions/0006-a2ui-and-the-vymalo-extensions-in-adam-rs.md)).
* The coder waits until `postgres`, `mock-openai`, `mock-github` and `git-server`
  are healthy.
* The coder is a token (`GITHUB_TOKEN`, a dummy) unless `-f dev/compose.github-app.yaml` is added: that override
  turns the token off, makes a throwaway RSA key into a volume with an init service (no key is committed) and gives
  the coder `GITHUB_APP_*`, so it trades a JWT at `mock-github` for an installation token that lasts four minutes
  (inside the coder's refresh margin, so the refresh runs all the time). `dev/coder-e2e.sh` with `GITHUB_AUTH=app`
  asserts that the trade happened and that every call to the repositories' API carried the installation token and
  never the JWT; CI runs the four scenarios of that script in both modes.
* The service `agent` is [`adam-agent`](../bin/adam-agent/README.md) from the **coder's image** with the entrypoint
  overridden (`tini -- adam-agent`; there is no second image): a chat persona in the folder
  `dev/agents/assistant/agent` (`AGENT_FOLDER` mounts another), the model `mock-assistant` (it answers in role from the
  two persona lines of the prompt), the same database as the coder (runs are scoped by the agent's name) and no
  workspace or GitHub. It waits for `postgres` and `mock-openai`. `dev/agent-e2e.sh` runs "hi" through it (the task
  completes with the folder's name and summary) and restarts it on an edited copy of the folder.
  `dev/agent-cards-e2e.sh` restarts it on the researcher folder (`dev/agents/researcher/agent`, minus its search
  server) and the model `mock-researcher`, and asks a question that carries `[mock:cards]` and the screen's catalog
  (version 3): the run ends with one `ui` artifact, a Text, a Cards of three sources and a Mermaid graph under the
  screen's `catalogId`; a screen on catalog version 2, or with none, gets the words only.
* The mock model is canned: it answers in text, or calls the first declared tool
  with `{}`. So it cannot drive OpenCode through a real change, and a local run
  does not end in a pull request. A complete run needs a model that can call
  tools. The git remote and the pull request API can still be `git-server` and
  `mock-github`. See the root README for this caveat and for the live smoke
  test in the [`adam-coder` README](../bin/adam-coder/README.md).
* The `compose` job in `.github/workflows/ci.yml` starts the mocks and runs the
  real clients (`OpenAiCompatible`, `GitHub`) against them, so the mappings
  cannot rot. The `image` job of `.github/workflows/coder.yml` builds the coder image once, smoke-tests both
  binaries in it (`docker/coder/test/container-smoke.sh`, `agent-smoke.sh`) and runs the scenarios against the
  stack, `dev/agent-e2e.sh`, `dev/coder-choices-e2e.sh` and `dev/agent-cards-e2e.sh` among them.

## The generic agent

`adam-agent` (`bin/adam-agent`) is a second composition root over [`adam-service`](../crates/adam-service/README.md): it
serves **any agent folder** and has no agent of its own. Its `serve` (`bin/adam-agent/src/serve.rs`) reads the
folder `ADAM_AGENT_DIR` names, which is required by every role (`folder::load`, `AgentFolder::load`), logs the same
`agent files` line as the coder, makes the card from it (`card_of`), and, for the roles that run workers, assembles
the agent (`assemble`: `AgentDef::connect_mcp` under the `MCP_ALLOW_*` policy, `bind` with the tools of `adam-ui`, `model`) and
registers it (`Assembly::register`: the root and its subagents); a control plane registers the start-only half
(`LlmStarter`). Then it hands `Agents { name, card, register, options }` to `adam_service::serve`, which is the process
of [How a binary composes them](#how-a-binary-composes-them). The sequence and the lifecycle of that startup are in
its [README](../bin/adam-agent/README.md#the-process) and in
[ADR 0005](decisions/0005-one-binary-serves-any-agent-folder.md).

What the diagrams cannot say:

* **There is no embedded agent and no default**: without `ADAM_AGENT_DIR` no role starts (exit 78).
* **The agent's tools are its folder's**: `ask_user`, `show` and `ui_catalog` (the screen's, of `adam-ui`), the tools of its MCP servers (`<server>__<tool>`), the
  skills' tools and one per subagent. Nothing in the binary touches a filesystem, a shell or git.
* **Runs are scoped by the agent's name**, so several `adam-agent` services with different folders share one
  database (`RuntimeTaskBackend` refuses another agent's runs, a worker claims only what it registered). They are
  not pinned to a worker (`ClaimScope::Any`): there is no workspace.
* **It ships inside the coder image** (decision 8 of the ADR): the entrypoint stays `adam-coder`, and a service that
  runs a folder overrides it with `adam-agent`.

## Where to go next

| Crate | Layer | README |
|---|---|---|
| `adam-error` | contracts | [crates/adam-error](../crates/adam-error/README.md) |
| `adam-core` | contracts | [crates/adam-core](../crates/adam-core/README.md) |
| `adam-model` | contracts | [crates/adam-model](../crates/adam-model/README.md) |
| `adam-a2a` | contracts | [crates/adam-a2a](../crates/adam-a2a/README.md) |
| `adam-store-postgres` | implementation | [crates/adam-store-postgres](../crates/adam-store-postgres/README.md) |
| `adam-store-mongodb` | implementation | [crates/adam-store-mongodb](../crates/adam-store-mongodb/README.md) |
| `adam-model-openai` | implementation | [crates/adam-model-openai](../crates/adam-model-openai/README.md) |
| `adam-workspace` | implementation | [crates/adam-workspace](../crates/adam-workspace/README.md) |
| `adam-devcontainer` | implementation | [crates/adam-devcontainer](../crates/adam-devcontainer/README.md) |
| `adam-acp` | implementation | [crates/adam-acp](../crates/adam-acp/README.md) |
| `adam-notify-postgres` | implementation | [crates/adam-notify-postgres](../crates/adam-notify-postgres/README.md) |
| `adam-runtime` | runtime | [crates/adam-runtime](../crates/adam-runtime/README.md) |
| `adam-a2a-runtime` | runtime | [crates/adam-a2a-runtime](../crates/adam-a2a-runtime/README.md) |
| `adam-llm-agent` | agent | [crates/adam-llm-agent](../crates/adam-llm-agent/README.md) |
| `adam-service` | runtime | [crates/adam-service](../crates/adam-service/README.md) |
| `adam-ui` | agent | [crates/adam-ui](../crates/adam-ui/README.md) |
| `adam-coder` | agent, binary | [bin/adam-coder](../bin/adam-coder/README.md) |
| `adam-agent` | agent, binary | [bin/adam-agent](../bin/adam-agent/README.md) |
| `adam-macros` | authoring, proc-macro | [crates/adam-macros](../crates/adam-macros/README.md) |
| `adam` | authoring, facade | [crates/adam](../crates/adam/README.md) |
| `adam-store-testkit` | test kit | [crates/adam-store-testkit](../crates/adam-store-testkit/README.md) |
| `adam-notify-testkit` | test kit | [crates/adam-notify-testkit](../crates/adam-notify-testkit/README.md) |

Also:

* [Root README](../README.md): the durable model, how each store keeps its
  promises, local development, errors, testing.
* [`deploy/coder/README.md`](../deploy/coder/README.md): the Helm chart, its
  secrets and its known risks.

## Verified and unverified

**Verified 2026-09-29, source: this repository at commit `172a117`.** The crate
graph (from each `Cargo.toml`), the traits and their implementations (from a
search for every `pub trait` and `impl` of it), every error enum and its
`Classify` impl, the run transitions (`runtime.rs`, `worker.rs`), the request
path, the coder's tools and rules, the Dockerfile, the chart templates and
`compose.yaml`. Nothing was executed to check them: the diagrams come from
reading the code. The Mermaid syntax of every diagram is checked in CI by
`tools/docs-check`.

**Verified 2026-09-29, source: this repository with the `Notifier` port and
`adam-notify-postgres`, executed.** The cross-process fan-out and the listener
lifecycle above are checked by `crates/adam-notify-postgres/tests/two_runtimes.rs`
(a front and a worker with a 30 s poll, against PostgreSQL 16), and the
`NOTIFY` payload limit of 8000 bytes against a 16.13 server. See the crate's
README for the third-party facts (PostgreSQL docs, `sqlx-postgres` 0.9.0 source).

**Verified 2026-09-30, source: this repository with ADR 0003, executed.** The path of a task that
references a finished one, and what `LlmAgent` carries over, are checked by the tests the ADR names
(`adam-runtime` over every store, `adam-llm-agent`, and `adam-a2a-runtime` over memory and PostgreSQL,
across a restart and with a real `LlmAgent` behind the backend; executed against memory and PostgreSQL 16.13,
the MongoDB variants run in CI). The A2A text and the `a2a-lf` field
behind it are quoted in the ADR.

**Unverified.**

* The `sysexits.h` numbers 78, 69, 71 and 70 are from memory. The code and the
  root README say the same. The header is not in this repository.
* The A2A method names (`SendMessage`, `SendStreamingMessage`, `GetTask`,
  `CancelTask`, `SubscribeToTask`), the `TASK_STATE_*` names and the error
  codes `-32001` and `-32002` are as documented in `crates/adam-a2a/src/lib.rs`
  for the pinned SDK (`a2a-lf` 0.3, `a2a-server-lf` 0.4). They were not checked
  here against the A2A specification.
* How the SDK frames SSE, and that it sends a keepalive comment every 15 seconds,
  is taken from the `adam-a2a` docs, not re-tested.
* OpenCode's behaviour (ACP over stdio, `{env:VAR}` substitution in its
  inline config) is as recorded in `bin/adam-coder/src/opencode.rs`, which
  cites the OpenCode source at `sst/opencode@7945de2`. It was not re-checked
  here, and a live run against a real gateway is not covered by CI.
* The chart was rendered but, per its README, not applied to a cluster or
  validated against the CRD schemas of the installed operators. The compose
  `app` profile was validated with `docker compose config` only when it was
  written.
* How the chart reaches the cluster after the tag bump is outside this
  repository.
