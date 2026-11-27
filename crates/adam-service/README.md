# adam-service

The A2A service shared by every agent binary: the run store chosen from `DATABASE_URL`
(PostgreSQL, MongoDB or SQLite), the runtime and its workers, the A2A router, cross-process
notifications where the store has them, the process's common configuration and its exit codes.
`adam-coder` and `adam-agent` are each a composition of this crate and the agent they serve; what
differs between them (the tools, the files, the credentials) stays in the binary.

It is the generic half of what `adam-coder` used to hold itself, moved and not rewritten: the same
components, the same order, the same drain and the same log lines.

## Where it sits

```mermaid
flowchart LR
  bin["bin/adam-coder, bin/adam-agent"] --> svc["adam-service"]
  svc --> a2a["adam-a2a + adam-a2a-runtime"]
  svc --> rt["adam-runtime"]
  svc --> pg["adam-store-postgres + adam-notify-postgres,<br/>adam-store-mongodb, adam-store-sqlite"]
  svc --> host["adam-host (roles, supervisor)"]
  svc --> oa["adam-model-openai (ModelConfig::client)"]
  bin --> asm["adam / adam-assembly (the agent's files)"]
```

The binary reads its files and its own configuration, assembles its agent, and hands this crate
the agent's name, its card and a closure that registers it on the runtime. Nothing here knows what
an agent does.

## API at a glance

| Item | What |
|---|---|
| `ServiceConfig::parse(&lookup, &mut problems)` | `ROLE`, `DATABASE_URL`, `NOTIFY_LOCAL_ONLY`, `A2A_BEARER_TOKENS`, `PUBLIC_URL`, `LISTEN_ADDR`, `ACTIVITY_LISTEN_ADDR` and `ACTIVITY_BEARER_TOKENS` (every role, `ServiceConfig::activity`, through `ActivitySettings::parse`), and for a role that runs workers `WORKERS`, `WORKER_ID`, `MAX_PARALLEL_TOOL_CALLS`, `MAX_TOOL_CALLS_IN_FLIGHT`, `MAX_MODEL_CALLS` and `MODEL_TOKENS_PER_MINUTE`, and `DEFAULT_LOCALE` (a BCP 47 tag; `serve` lists `locale/v1` on the card with it, none when unset, exit 78 when malformed). A role reads only what it uses; every problem is collected, none stops the parse. `ServiceConfig` gained the field `notify_local_only` (read by every role, since every role runs `notify`): **a literal built by hand needs it**; so does the field `default_locale`, and the field `prompt_statement` (`MODEL_AUTH=injector`, read by every role as the word only: `serve` lists `prompt-statement/v1` on the card, ADR 0077) |
| `A2aSettings`, `PushSettings`, `CardSigning` | the optional A2A features, read by the roles that serve A2A (`ServiceConfig::a2a`): `A2A_PUSH_ALLOWED_URLS`, `A2A_PUSH_ALLOW_PRIVATE`, `A2A_PUSH_GIVE_UP_AFTER_SECS`, `A2A_PUSH_REQUEST_TIMEOUT_SECS`, `A2A_CARD_SIGNING_KEY_FILE`, `A2A_CARD_SIGNING_KEY_ID`, `A2A_CARD_SIGNING_JKU`, and `A2A_DOCS` (the field `docs`, **new: a literal built by hand needs it**; `Default` is hand-written and sets it `true`). **Nothing is on unless set, but the docs, which are on unless `A2A_DOCS=false`.** The key file is read and checked at startup (a problem naming the variable, never the key). `ServiceConfig` gained the field `a2a`: **a literal built by hand needs it** |
| `WorkerSettings` | `WORKERS` (at least 1), `WORKER_ID` (`is_worker_id`), `MAX_PARALLEL_TOOL_CALLS` (`max_parallel_tool_calls`, 1 to `MAX_PARALLEL_TOOL_CALLS_LIMIT` = 32, default `DEFAULT_MAX_PARALLEL_TOOL_CALLS` = 10), `MAX_TOOL_CALLS_IN_FLIGHT` (`max_tool_calls_in_flight`, 1 to 256, default 4), `MAX_MODEL_CALLS` (`max_model_calls`, 1 to 64, default 2) and `MODEL_TOKENS_PER_MINUTE` (`model_tokens_per_minute`, 1 to 100000000, `None` when unset), `MAX_PROCESSES` (`max_processes`, 1 to 256, default 4), `MAX_GIT_PROCESSES` (`max_git_processes`, 1 to 64, default 2), `COMMAND_MEMORY_MAX` (`command_memory_max`, bytes, at least 64 MiB, default a quarter of the RAM at most 8 GB) and `COMMAND_CPU_MAX` (`command_cpu_max`, millicores, at least 100, default half the cores); **new fields: a literal built by hand needs them**; `options()` gives the `RuntimeOptions` and `budget()` the process's `adam_budget::Budget` (ADR 0065 slice 2) |
| `ModelConfig::parse`, `client()` | `MODEL_BASE_URL`, `MODEL_API_KEY` (may be empty, not unset; with `MODEL_AUTH=device` unset or empty), `MODEL_AUTH`, `MODEL_OAUTH_ISSUER`, `MODEL_OAUTH_CLIENT_ID`, `MODEL_OAUTH_SCOPE` (the `device_grant` field, an [`adam-model-oauth`](../adam-model-oauth/README.md) `DeviceGrant`, **a new public field: a struct literal needs `device_grant: None`**; `client()` starts its sign-in and gives it to the client, ADR 0058), `MODEL_AUTH=injector` (the `injected_run` field: `client()` gives the client `adam_model::InjectedRun`, so every call carries `X-Run-Id` and no `Authorization`; a non-empty `MODEL_API_KEY` or any `MODEL_OAUTH_*` beside it is a problem, [ADR 0067](../../docs/decisions/0067-a-model-call-through-an-injector-names-its-run-and-a-refusal-parks-the-run.md)), `MODEL_CA_FILE` (the `ca_roots` field, an `adam_model_openai::ExtraRoots` read and checked at startup, which `client()` trusts beside the default roots; **`injected_run` and `ca_roots` are new public fields: a `ModelConfig` literal needs them**), `MODEL_PARK_LIMIT` (the `park_limit` field, **a new public field: a `ModelConfig` literal needs it**), `MODEL`, and `MODEL_EXTRA_BODY`, `MODEL_ECHO_REASONING` and `MODEL_CONTEXT_WINDOW` (the `extra_body`, `echo_reasoning` and `context_window` fields, **new public fields: a `ModelConfig` built with a struct literal needs them**; `context_window` is new with `usage/v1`); the OpenAI-compatible client over them, which says `openai` as its provider and the context window for the alias `MODEL` only. `MAX_CONTEXT_WINDOW` is the largest window (2^53 - 1). `MODEL_ALIASES_FILE` (ADR 0063, ADR 0064; **JSON only for now**) gives the `model_aliases_file` and `profiles` fields, read and validated at startup (a file that cannot be read or breaks a rule is a problem naming the file and the key, exit 78 in the binaries; **new public fields: a `ModelConfig` literal needs them**); `client()` gives them to the client (`with_profiles`), which sends a request's `reasoning_effort` through its model's `reasoningWire` after `MODEL_EXTRA_BODY` (ADR 0063 decision 6), and the binaries also pass them to the assembly, which resolves each agent's default reasoning effort and output cap; a process whose `MODEL_ALIASES_FILE` declares no `maxOutputTokensLimit` for `MODEL` (no profile, or one without the key) warns at startup, naming the 128,000 fallback and the window, and with no profiles file the same fallback is logged at `info`. Its `Debug` (the startup log prints the configuration) shows the gateway by **scheme and host only** (`adam_service::endpoint_for_logs`, re-exported from `adam-model-openai`), the key not at all, and `ServiceConfig`'s shows `PUBLIC_URL` the same way and `DATABASE_URL` as `[REDACTED]`: deployments keep these addresses in secrets |
| `harden::make_non_dumpable()` | `prctl(PR_SET_DUMPABLE, 0)` on Linux, through `rustix`'s safe call: a same-user child without `CAP_SYS_PTRACE` cannot read the process's `/proc/<pid>/environ`. Every agent binary calls it at startup (`adam-coder`, `adam-agent`); `adam_coder::harden` re-exports it. Returns whether it was set; a failure is a warning |
| `logging::env_filter()`, `logging::DEFAULT_LOG_FILTER` | the log filter of a binary: `RUST_LOG` when set (it replaces the default whole), else `info,rmcp=warn`: the MCP client library logs a whole `peer_info` at INFO with "task cancelled" and "serve finished" at every connection, and the thread-tools endpoint is connected when its listing is read (once per grant, again after the 60 s `LISTING_TTL`) and on every call |
| `with_prompt_commands(Commands, &[PromptCommand])`, `card_with_commands(card, &[CommandInfo])`, `refuse_prompt_commands(Commands, Option<&McpConfig>)` (feature `mcp`) | the prompts of an agent's MCP servers added to the backend's commands and to the `commands/v1` entry of the card (every entry the card has is kept as it is); a prompt with the name of a command the folder has is left out of both, with a warning; the folder's commands and the prompts together are at most 50 (the folder's first), the rest dropped with one warning that gives how many. `refuse_prompt_commands` is for a process that connects no MCP server (a control plane): it refuses `/<server>__...` for each server of the config that serves prompts (`invalid params`), with one warning at start |
| `McpSettings::parse`, `policy()` (feature `mcp`) | `MCP_ALLOW_STDIO`, `MCP_ALLOW_INSECURE`, `MCP_ALLOW_URL_VARS`, `MCP_MAX_CALL_SECS` and `THREAD_TOOLS_MAX_CALL_SECS`; the `McpPolicy` for `AgentDef::connect_mcp` and the thread-tools client |
| `parse_or`, `parse_flag`, `parse_file`, `is_worker_id` | the helpers a binary parses its own variables with, into the same list of problems (`parse_file`: an existing file, or a problem naming the variable; `ADAM_EXTRA_MCP_FILE` of `adam-coder` and `adam-agent`) |
| `ConfigError { problems }` | what a non-empty list becomes (`ConfigError::check(problems)`); `Classify` gives `Invalid` |
| `RuntimeOptions`, `LiveSignals` | how the runtime is set up (worker id, claim scope, concurrency, lease, and the three intervals: `poll_interval` 250 ms for the worker's idle poll, `cancel_poll_interval` 1 s for a step's cancel watch, `view_poll_interval` 2 s for a subscription's fallback), and how a process learns of other processes (`LiveSignals::local()` or the Postgres `NOTIFY` ones `serve` builds; SQLite gets an in-process `LocalNotifier`, and MongoDB polling). **`cancel_poll_interval` and `view_poll_interval` are new public fields: a `RuntimeOptions` built with a struct literal needs them** |
| `Service { runtime, backend }` | `Service::new(builder, name, options)`, `new_with(.., live)`, `with_inbound(Option<InboundFn>)` (how an A2A message becomes the agent's input), `router(card, auth)`, `router_with_options(card, auth, ServerOptions)` (push notifications, the card's signature), `run_worker(shutdown)`; `router(&backend, card, auth)` and `router_with_options(&backend, ..)` for a composition that holds the parts itself |
| `Agents::new(name, register)` | the agent a process serves: its name, `.card(card)`, `.options(options)`, `.inbound(f)` (read A2A messages with `f`, e.g. `adam_a2a_runtime::vymalo_inbound` for an agent behind a screen; the default is `default_inbound`), `.access_tiers(tiers)` (the agent's card lists `access-tier/v1` with these tiers: the backend then reads, validates and says them, see `adam-a2a-runtime`; `Service::with_access_tiers(Option<AccessTiers>)` is the same for a composition that holds the service itself), `.model_choices(choices)` (the agent's card lists `model-pick/v1` with these choices — each `{"id": "<alias>", "endpoint"?: "<label>"}`, built by `ModelChoices::from_profiles` so the label comes from `MODEL_ALIASES_FILE` when the deployment names one (ANR-623): the backend then reads the pick of a message that activated it, refuses one outside them as `model_not_offered`, and **`serve` adds the extension to the card itself, only when the backend accepted a non-empty list**; `Service::with_model_choices(Option<ModelChoices>)` and `Service::model_choices()` are the same for a composition that holds the service itself; **`Agents` gained the public field `model_choices`**, as before only a destructuring without `..` breaks), `.reasoning_efforts(efforts)` (`adam_a2a::ReasoningEfforts`: the agent's card lists `reasoning/v1` with these levels and the backend reads the effort of a message that activated it, refusing one the agent's model does not take, see `adam-a2a-runtime`; **`serve` adds the entry to the card itself** with `card_with_reasoning(card, efforts, agent)`, which keeps an entry the card already lists, so a binary that also composes one does not list it twice; `Service::with_reasoning_efforts(Option<ReasoningEfforts>)` and `Service::reasoning_efforts()` are the same for a composition that holds the service itself; **`Agents` gained the public field `reasoning_efforts`**, as before only a destructuring without `..` breaks), `.commands(commands)` (`adam_a2a_runtime::Commands`: the agent's card lists `commands/v1`, and the backend expands a message that names one; `Service::with_commands(Option<Commands>)` is the same; the card still comes from the caller, so build both from one manifest (`Commands::from_manifest` and `card_of`), and `tests/serve.rs` proves `serve` passes the commands to the backend; **`Agents` gained the public field `commands`**, it is `#[non_exhaustive]` so only a destructuring without `..` breaks), and `register`, a closure that puts it on the runtime builder (`Assembly::register`, or the starter only for a control plane); `.child_sessions(sessions)` (`adam_a2a_runtime::ChildSessions`, from `AgentDef::child_sessions`: the backend serves `child-sessions/v1` for these helpers and **`serve` adds the extension to the card itself, only when the backend accepted them**, so the binary's card never lists what the process does not serve; every helper must be registered on the runtime (the full agent, or its starter on a control plane), or `serve` returns `ServeError::ChildSessions` before it serves anything; `Service::with_child_sessions(Option<ChildSessions>)` and `Service::child_sessions()` are the same for a composition that holds the service itself; **`Agents` gained the public field `child_sessions`**, as before only a destructuring without `..` breaks), `.agent_modes(modes)` (`adam_a2a::AgentModes`, from `Assembly::agent_modes`: the backend reads the mode of a message that activated `agent-mode/v1` and refuses one the folder does not declare, **`serve` adds the extension to the card itself, only when the backend accepted modes**, and returns `ServeError::AgentModes` (exit 78) when the card lists it and the backend was given none; `Service::with_agent_modes(Option<AgentModes>)` and `Service::agent_modes()` are the same for a composition that holds the service itself; **`Agents` gained the public field `agent_modes`**), `.worker_component(name, f)` adds a component of the binary to the worker tier (below) |
| `inline_upload_threshold(folder, env)` | the number a process serves `uploads/v1` under: the folder's `limits.max_inline_upload_bytes`, else `UPLOADS_INLINE_MAX_BYTES` (`ServiceConfig::uploads_inline_max_bytes`), else `adam_a2a::DEFAULT_UPLOADS_INLINE_MAX_BYTES` (10 MiB). `serve` logs it once; a tool that holds a file (the coder's `share_file`) is built with the same call, so the tool and the card cannot disagree |
| `serve(&config, agents, shutdown)` | the whole process, until `shutdown` resolves; `ServeError` on failure |
| `serve_on(&config, agents, listener, shutdown)` | the same on a `tokio::net::TcpListener` the caller bound (`LISTEN_ADDR` is not read, so never `Bind`): a host that must know its port first binds it and hands it over, with no moment in which another process can take it; the checks run in the same order |
| `serve_on_with_activity(&config, agents, listener, activity, shutdown)` | `serve_on`, with the activity listener bound by the caller too (`ACTIVITY_LISTEN_ADDR` is not read); `ServeError::ActivityWithoutTokens` (exit 78) when `config.activity` is `None`: a listener with no tokens of its own is never served |
| `ServeError` | `Connect` (the message names the backend, `connecting to Postgres`), `Migrate`, `NoCard`, `ChildSessions` (a helper of `child-sessions/v1` is not registered on the runtime: exit 78), `AgentModes` (the card lists `agent-mode/v1` while the backend was given no modes: exit 78), `UnsupportedScheme`, `StoreNotBuilt`, `SqliteWithoutPath`, `SplitRoleOnSqlite` (the store `DATABASE_URL` names cannot be served: exit 78), `Bind` (`LISTEN_ADDR` or `ACTIVITY_LISTEN_ADDR`), `LocalAddr`, `Host`, `Push` (push notifications are on and cannot be set up: exit 78), `ActivityWithoutTokens` (exit 78). The message names the step; the cause is the `source`, so a chain printed whole says it once |
| `metrics::render(&adam_budget::Usage)` | The process's pools in the Prometheus text exposition format (`adam_admission_in_use`, `adam_admission_limit`, `adam_admission_waiting{pool}`, and `adam_admission_tokens_available`/`_limit` when the budget meters tokens), with `+Inf` for a pool that has no ceiling (ADR 0065 decision 9). It is a rendering, not a listener: `adam-service` runs no `/metrics` endpoint, and the counters `adam_admission_waits_total`, `adam_admission_refusals_total`, `adam_process_stopped_total` and the size `adam_disk_bytes` are not built, because the admission keeps no such totals. No person, run or conversation is a label |
| `claim_scope_for(placement)` | `Pinned` for a placement that pins runs, `Any` otherwise |
| `exit_code(&err)`, `exit_code_with(&err, classify)`, `EX_*` | the exit code of an error chain (below); `classify` is how a binary adds its own error types |

## Environment

| Variable | Meaning | Default |
|---|---|---|
| `ROLE` | `all`, `control-plane` or `worker` (`adam_host::Role`) | `all` |
| `DATABASE_URL` | the run store: `postgres://`/`postgresql://`, `mongodb://`/`mongodb+srv://` or `sqlite://<path>`; any other scheme, and `sqlite://` with a split role, is exit 78 ([environment](../../docs/reference/environment.md#the-store-database_url-selects)) | required |
| `A2A_BEARER_TOKENS` | comma-separated accepted tokens (fail closed: none = no server) | required by `all` and `control-plane` |
| `PUBLIC_URL` | where clients reach the JSON-RPC endpoint (agent card) | required by `all` and `control-plane` |
| `LISTEN_ADDR` | bind address: the A2A server, or a worker's `/healthz` listener | `0.0.0.0:8080` |
| `ACTIVITY_LISTEN_ADDR` | turns on the activity listener: `GET /internal/activity` alone, on this address, behind `ACTIVITY_BEARER_TOKENS`; the route then leaves `LISTEN_ADDR`. Any role; another port than `LISTEN_ADDR`'s (ADR 0083) | unset: the route stays on `LISTEN_ADDR` (deprecated) |
| `ACTIVITY_BEARER_TOKENS` | comma-separated tokens of the activity listener, none an A2A token; required with `ACTIVITY_LISTEN_ADDR` (exit 78), refused without it | unset |
| `A2A_PUSH_ALLOWED_URLS` | turns A2A push notifications **on** and names the webhooks they may reach: comma-separated URL prefixes or hosts (`PushSettings`); a client's webhook that matches none, is not `https` or is a private address is refused | unset: off, the card says so |
| `A2A_PUSH_ALLOW_PRIVATE` | also allow loopback, private and link-local webhooks and `http` to loopback: development only | `false` |
| `A2A_PUSH_GIVE_UP_AFTER_SECS`, `A2A_PUSH_REQUEST_TIMEOUT_SECS` | how long a notification may keep failing before delivery to that webhook is abandoned (1 to 604800), and how long one request may take (1 to 120) | `3600`, `15` |
| `A2A_CARD_SIGNING_KEY_FILE`, `A2A_CARD_SIGNING_KEY_ID`, `A2A_CARD_SIGNING_JKU` | a PKCS#8 PEM key (ECDSA P-256 or Ed25519) that signs the agent card, its `kid` (default: the key's thumbprint) and its `jku` (`CardSigning`); the server serves the key set at `/.well-known/jwks.json` | unset: unsigned |
| `A2A_DOCS` | Swagger UI at `/docs` and the OpenAPI document at `/openapi.json` of the A2A server, both public (the calls need the token; `adam-a2a` README, *Routes*); `false` or `0` turns them off (`A2aSettings::docs`, `ServerOptions::with_docs`) | `true` |
| `WORKERS` | runs advanced concurrently | `4` (roles that run workers) |
| `WORKER_ID` | lease identity: 1 to 128 of letters, digits, `.`, `_`, `-`, not starting with `.` | random per process (roles that run workers) |
| `MAX_PARALLEL_TOOL_CALLS` | the most tool calls of one model turn that run at the same time, 1 to 32; anything else is a problem naming the variable (exit 78); an agent file's `limits.max_parallel` may lower it for its agent | `10` (roles that run workers) |
| `MAX_TOOL_CALLS_IN_FLIGHT` | the most tool calls of the whole process that run at the same time, 1 to 256: the pool `MAX_PARALLEL_TOOL_CALLS` sits inside (ADR 0065); anything else is a problem naming the variable (exit 78) | `4` (roles that run workers) |
| `MAX_MODEL_CALLS` | the most model calls in flight in the process, 1 to 64 (ADR 0065); anything else is a problem naming the variable | `2` |
| `MAX_PROCESSES` | the most commands (`run`, `run_command`, checks, background commands and language servers) of the whole process at once, 1 to 256 (ADR 0065 slice 3); anything else is a problem naming the variable | `4` |
| `MAX_GIT_PROCESSES` | the most git processes at once, 1 to 64; in the budget as `Pool::Git`; `adam-coder` makes every git of its container take a seat | `2` |
| `COMMAND_MEMORY_MAX` | the memory one command may use: bytes, or `512MiB`, `2GB`; at least 64 MiB | a quarter of the RAM, at most 8 GB |
| `COMMAND_CPU_MAX` | the CPU one command may use: cores (`2`, `1.5`) or millicores (`500m`); at least `100m` | half the cores, at least 1 |
| `MODEL_TOKENS_PER_MINUTE` | the model's tokens per minute, 1 to 100000000, charged to one bucket the whole process shares (ADR 0065); unset or blank meters none | unset |
| `MODEL_BASE_URL`, `MODEL_API_KEY`, `MODEL`, `MODEL_AUTH`, `MODEL_OAUTH_*` | the model (`ModelConfig`); `MODEL_AUTH` is `key`, `device` or `injector` (no key: each call names its root run in `X-Run-Id` and carries a run's prompt statement, and the card lists `prompt-statement/v1`; a non-empty `MODEL_API_KEY` or a `MODEL_OAUTH_*` beside it is exit 78) | required by the binaries that call `ModelConfig::parse`, for the roles that run workers |
| `TURN_SIGNING_KEY_FILE`, `TURN_SIGNING_KEY_ID`, `TURN_VERIFY_KEYS_FILE` | the turn key (`ModelConfig::turn_keys`, **a new public field**): set, every model call is signed (`adam_model::SignedRequests` around the credentials: `Turn-Signature`, `X-Run-Id`, the prompt headers; ADR 0068 amended 2026-11-25) and the card lists `prompt-statement/v1`; a key file that cannot be read or is not an Ed25519 private key, a bad id, or an id or a verify set without a key file is exit 78 | unset: nothing is signed |
| `MODEL_CA_FILE` | a PEM file of certificates trusted for the model endpoint beside the default roots (`ModelConfig::ca_roots`, ADR 0067); a file that cannot be read, is not PEM or holds no certificate is a problem naming the variable (exit 78 in the binaries) | unset: the default roots |
| `MODEL_PARK_LIMIT` | how long parks on a timer in a row may last before the run fails with the last reason (`ModelConfig::park_limit`, ADR 0067; a binary gives it to its agents as `adam_llm_agent::ModelParking`, with `injected_run` saying whether an unreachable gateway parks); whole seconds or a number with `s`, `m`, `h` or `d`, 60 s to 30 d, anything else a problem (exit 78) | `24h` |
| `MODEL_ALIASES_FILE` | the per-model profiles (ADR 0063, ADR 0064; `ModelConfig::model_aliases_file` and `profiles`, **JSON only for now**): one object with `profiles` (by model id) and `presets` (by name). A file that cannot be read, is not JSON, or breaks a rule (an unknown level or wire, a duplicate, a bad budget, a cap above the model's limit, an unknown preset, a reserved `extraBody` member) is a startup problem naming the file and the key (exit 78 in the binaries) | unset: no model has a profile |
| `MODEL_EXTRA_BODY` | a JSON object merged into every chat-completions request (`ModelConfig::extra_body`), for a flag that makes a model emit its reasoning; a request's typed `reasoning_effort` wins over the members its model's wire would duplicate (ADR 0063 decision 6). Not secret; not an object, or one that sets `model`, `messages`, `tools`, `tool_choice` or `stream`, is a startup problem (exit 78 in the binaries) | unset |
| `MODEL_PROMPT_CACHE_KEY` | `off` or `conversation` (`ModelConfig::prompt_cache_key`, `PromptCacheKey`): send each request's `prompt_cache_key` (the hash of the conversation id); anything else is a startup problem. **`ModelConfig` has a new public field: a struct literal needs it** | `off` |
| `MODEL_ECHO_REASONING` | `reasoning_content` or `reasoning` (`ModelConfig::echo_reasoning`): send the reasoning of earlier turns back under that name, for a provider that requires it; `false` or unset sends none | unset |
| `MODEL_CONTEXT_WINDOW` | the context window, in tokens, of the model `MODEL` names (`ModelConfig::context_window`): 1 to 9007199254740991, anything else a startup problem (exit 78 in the binaries). The client answers it for that alias only, and a `usage/v1` call report says it as `contextWindow`; the assembly also clamps the agent's output cap to what it leaves (ADR 0064) | unset |
| `MCP_ALLOW_STDIO`, `MCP_ALLOW_INSECURE`, `MCP_ALLOW_URL_VARS` | what an agent folder's MCP servers may be (`McpSettings`, feature `mcp`) | `false` each |
| `MCP_MAX_CALL_SECS` | the longest one MCP server is waited for, a call and a connection each: what `timeoutSecs` and `connectTimeoutSecs` ask for and the defaults (60 s and 30 s) are lowered to it, and a value the file asks for is at least 1 s (1 to 86400; `McpSettings`, feature `mcp`); a larger value is lowered to it with a warning at startup | `900` |
| `THREAD_TOOLS_MAX_CALL_SECS` | the longest a call to a tool of the thread's tools endpoint is waited for, whatever time the tool says it may take (1 to 86400; `McpSettings`, feature `mcp`); a tool that says nothing is waited for 60 s | `3600` |

A binary that uses these keeps its own variables beside them (the coder's `GITHUB_TOKEN`,
`WORKSPACE_*`, ...). What a role does not use is not validated, so a chart may set a variable for every
role. Secrets are `SecretString`s and never in `Debug` output.

## Images

`MODEL_INPUT_MODALITIES` (`text` or `text,image`), `MODEL_IMAGE_MAX_BYTES` (1 to 4,194,304; default 1 MiB), `MODEL_IMAGE_MAX_COUNT` (1 to 32; 8), `MODEL_IMAGE_MAX_SIDE` (64 to 16,384; 4,096) and `MODEL_IMAGE_SOURCES` (a subset of `person`, `workspace`, `mcp`; `person,workspace`) become `ModelConfig::images` and, through `ModelConfig::client`, `OpenAiCompatible::with_input_modalities` for the alias `MODEL` only. An image variable set while the modalities lack `image` is a startup problem that names it (exit 78), and so is `MODEL_OUTPUT_MODALITIES` with `image` (not built for any provider) and `MODEL_IMAGE_MAX_BYTES` times `MODEL_IMAGE_MAX_COUNT` over 32 MiB (`IMAGE_REQUEST_MAX_BYTES`: a request resends every admitted image; the message names both). `ModelConfig` gains a field: a literal of it needs `images: None`. The table is [environment](../../docs/reference/environment.md).

## The process
## The process

```mermaid
sequenceDiagram
  participant B as binary (main)
  participant S as serve
  participant P as Postgres
  participant H as Host (adam-host)
  B->>B: parse the configuration, read the agent's files, assemble the agent
  B->>S: serve(config, Agents { name, card, register, options, inbound, child_sessions }, shutdown)
  S->>S: open the pool without connecting (PgStore::connect_lazy)
  S->>S: PgNotify (LISTEN/NOTIFY) as LiveSignals
  S->>S: Service::new_with(register(Runtime::builder(store)), name, options, live)
  S->>S: every helper of child_sessions is registered, else ServeError::ChildSessions (78), and the card lists the extension
  S->>P: connect, migrate
  S->>S: bind LISTEN_ADDR, log `listening` (addr, role, workers, worker_id)
  S->>H: the components of the role
  H-->>S: shutdown: drain the server, let the steps finish, flush notify
  S-->>B: Ok(()), or the first error
```

```mermaid
stateDiagram-v2
  [*] --> Connecting
  Connecting --> Listening: migrated, bound
  Connecting --> Failed: no card, a helper of child-sessions/v1 not registered (78), the store down or schema refused, address taken
  Listening --> Draining: the shutdown future resolves
  Listening --> Failed: a component stops on its own
  Draining --> [*]: components stopped, notify flushed
  Failed --> [*]: the others are stopped the same way, the error is returned
```

| Role | Components | Also |
|---|---|---|
| `all` (default) | `a2a-server`, `worker`, `notify` | |
| `control-plane` | `a2a-server`, `notify` | the runtime knows the agent, and the helpers of `child-sessions/v1`, as starters only: no model, no tools |
| `worker` | `worker`, `health`, `notify` | `/healthz` on `LISTEN_ADDR` (and `GET /internal/activity`, bearer-protected, while `ACTIVITY_LISTEN_ADDR` is unset), no A2A |

With `ACTIVITY_LISTEN_ADDR` set, every role also runs `activity` (in the control-plane tier on `all` and `control-plane`, in the
worker tier on `worker`): the activity route alone on its own listener ([below](#get-internalactivity)).

`notify` exists only for a Postgres store: SQLite's signals are in-process (`LocalNotifier`) and
MongoDB has none, so a worker polls.

A binary adds components of its own with `Agents::worker_component(name, |store, stop| async { .. })`: they
join the worker tier, so they run in `all` and `worker` and never in `control-plane`, and the host stops them
with the workers. The closure is given the store `serve` connected and the token that says "stop"; the rules
are the host's (return when the token is cancelled; one that returns early, or with an error, stops the process
with a host error). The coder's sweep of the workspaces of finished runs is one.

`notify` (Postgres only) is the `adam-notify-postgres` listener and publisher: live events and wake-up/cancel signals cross
processes over `LISTEN`/`NOTIFY`, so a worker takes a run another process started at once and a control plane
streams the progress of a run a worker steps. It is a latency optimisation: polling stays on and correctness
never depends on a notification. With a worker it stops only after the worker has finished, then sends what is
still queued, so the last step's events normally still reach other processes (best effort, like any
notification).

The service backend serves one agent, `Agents::name`: a task is a run of that agent. Several services
may share one database, each under its own name, because a run of another agent is refused
(`RuntimeTaskBackend`) and a worker claims only the agents it registered.

## Exit codes

`exit_code` walks the error chain from the outside in and returns the first code that fits, so a supervisor
can tell a deployment that is misconfigured (do not restart) from one whose database is down (restart later)
from a bug. The values are those of BSD `sysexits.h` (*unverified*, from memory; the header is not part of this
repository's sources).

| Code | Name | Root cause |
|---|---|---|
| 0 | | clean shutdown after a signal |
| 78 | `EX_CONFIG` | `ConfigError`, `OpenAiConfigError`, `ServeError::NoCard`, `ServeError::ChildSessions`, `ServeError::AgentModes`, `ServeError::ActivityWithoutTokens`, or any error whose class is `Invalid` (including what a binary classifies with `exit_code_with`: a mistake in the agent's files) |
| 69 | `EX_UNAVAILABLE` | a `Transient`, `RateLimited` or `Conflict` error: the store, an MCP server |
| 71 | `EX_OSERR` | an `io::Error`, `ServeError::Bind`: a port that cannot be bound |
| 70 | `EX_SOFTWARE` | `HostError` (a component stopped, panicked or ended while still needed), a panicked task, a `Corrupt` or `Internal` error |
| 1 | | anything else |

A `HostError` decides whatever its source is: an `io::Error` from the server is not a 71, and a store error
from a worker not a 69.

## Features

| Feature | Default | Adds |
|---|---|---|
| `mcp` | no | `McpSettings` (brings `adam-mcp` for `McpPolicy`) |

## Tests

* `src/config.rs`: the parsers per role, problems collected together, `WORKERS` and `WORKER_ID`, the model
  (`MODEL_CONTEXT_WINDOW`: a positive count said for the alias `MODEL` only, anything else a problem; `MODEL_ALIASES_FILE`:
  read and validated, an unknown level or a missing file a problem naming the file; `MODEL_AUTH=injector` set and a key or a `MODEL_OAUTH_*` beside it
  refused with exit 78, `model_auth_injector_sends_the_run_and_refuses_a_key`; `MODEL_CA_FILE` read, a missing or empty file
  exit 78, `model_ca_file_is_read_at_startup_and_a_bad_one_is_78`; `MODEL_PARK_LIMIT` a day by default, its units and
  its range, `model_park_limit_is_a_day_by_default_and_a_minute_to_thirty_days`; `prompt_statement` set exactly by
  `MODEL_AUTH=injector` or a turn key, `the_prompt_statement_follows_model_auth_injector`; the turn key read, its id
  given, a bad key or id exit 78, an id without a key exit 78, no key nothing signed,
  `the_turn_signing_key_turns_on_request_signing`), the
  MCP flags, `A2A_DOCS` (on by default, `false` turns it off, a bad value is a problem, a worker ignores it), secrets
  hidden from `Debug`.
* `tests/model_auth.rs`: `MODEL_AUTH=injector` from the variable to the wire: the request carries `x-run-id` and no
  `authorization`, while the default `key` sends its key and no run.
  `MODEL_CA_FILE` the same way: a TLS server on the loopback whose certificate a test CA signed (the fixtures of
  `adam-model-openai`) is refused without the variable, nothing reaching it, and answers with it.
* `src/exit.rs`: the exit-code table, a `HostError` deciding over its source, a binary's own classifier.
* `src/service.rs`: `LiveSignals::local`, the defaults.
* `tests/metrics.rs`: `every_pool_is_one_series_and_an_unlimited_pool_is_infinite` (the whole text,
  one series per pool per metric, `+Inf` for a pool with no ceiling) and
  `a_process_that_meters_no_tokens_reports_no_bucket`.
* `tests/service.rs`: a `Service` over the in-memory store with a scripted agent: the card is served, `/healthz`
  answers, a bearer token is required, a task completes, a control plane and a worker over one store meet in it,
  and two services of different names share a store without stealing each other's runs.
* `tests/serve.rs` (the Postgres cases need `ADAM_TEST_POSTGRES_URL`, skipped without it; `ADAM_TEST_REQUIRE_DB=1` makes a
  missing URL a failure; the SQLite case needs neither): `serve` end to end on Postgres, for each role (through `serve_on`, on a listener the test
  bound, so no other test takes its port; `serve` itself for the bind error), and a binary's own component
  (`worker_component`) that runs with the workers and stops with them; `serve_runs_the_whole_agent_on_a_sqlite_file`
  runs the whole process over `DATABASE_URL=sqlite://<path>`, with no database server; nothing optional is on by default but the docs
  (`/openapi.json` 200 and `/docs` 303 without a token), and `A2A_DOCS=false` closes them (`a2a_docs_false_turns_the_docs_off`).
  `the_card_lists_prompt_statement_exactly_behind_the_injector`: the card lists `prompt-statement/v1`, once and with no
  `params`, with `MODEL_AUTH=injector`, and not with `key` or none (Postgres).

```sh
docker run -d -e POSTGRES_PASSWORD=postgres -p 5432:5432 postgres:18.7   # 18 or later: the store refuses an older server
ADAM_TEST_POSTGRES_URL=postgres://postgres:postgres@127.0.0.1:5432/postgres ADAM_TEST_REQUIRE_DB=1 \
  cargo test -p adam-service --features mcp
```

New in slice 4 ([ADR 0045](../../docs/decisions/0045-a-subagent-can-run-in-the-background-as-a-session.md)): `SessionSettings` (`limits`, `idle_ttl` and, since ANR-1093, `max_parallel_tool_calls`) from `SESSION_MAX_OPEN` (1-100, 16), `SESSION_MAX_WORKING` (1-32, at most open, 4; when unset it is cut down to `SESSION_MAX_OPEN`, so a small open alone is fine, and only two set values that conflict are exit 78), `SESSION_MAX_ROUNDS` (2-200, 20) and `SESSION_IDLE_DAYS` (1-365, 7), read by **every role** as `ServiceConfig::sessions`; a bad value is a problem (exit 78). `SessionSettings::max_parallel_tool_calls` is `MAX_PARALLEL_TOOL_CALLS` read by **every role, leniently** (1 to 32, else 10, never a problem here: the roles that run workers refuse a bad value through `WorkerSettings`, and a control plane still starts) so a control plane holds a person's `start.config` to the bounds of the workers' session tools (ADR 0054; the shipped agents give it to `AgentDef::session_max_parallel`). `RuntimeOptions::session_idle_ttl` is applied by `Service::new_with`; `serve` sets it from `ServiceConfig::sessions`. **Breaking:** `ServiceConfig` and `RuntimeOptions` gained fields, so struct literals stop compiling. Tests: `session_settings_have_defaults_and_bounds`, `a_control_plane_reads_the_session_settings`, `every_role_reads_max_parallel_for_the_session_bounds`.


Retention: `retention::RetentionSettings` from `RUN_RETENTION_DAYS` (1-36500 or `off`, default 90) and `RUN_RETENTION_SWEEP_SECS` (60-86400, 3600), read by **every role** as `ServiceConfig::retention`; a bad value is exit 78. Roles that run workers start a `retention` component that calls `Store::purge_finished(agent, now - retention)` at start and every sweep, logs the count (no run or person label), and never touches an open run. **Destructive default:** unset deletes finished runs older than 90 days; set `RUN_RETENTION_DAYS=off` to keep everything. **Breaking:** `ServiceConfig` gained `retention`. Tests: `unset_keeps_ninety_days`, `a_value_overrides_and_off_keeps_everything`, `the_cutoff_separates_ninety_days_from_eighty_nine`, `off_deletes_nothing`; the store-side deletion and concurrent sweeps are the testkit case `purge_finished_runs`.

## `GET /internal/activity`

**On a listener of its own** when `ACTIVITY_LISTEN_ADDR` is set (any role; `ServiceConfig::activity`, an
`ActivitySettings`): that listener serves this route and nothing else (every other path, `/healthz` included, is a 404),
behind `ACTIVITY_BEARER_TOKENS` alone, and `LISTEN_ADDR` no longer serves it. So its caller (an operator that scales the
service) holds no A2A token and needs no path to the A2A port, and a token of either list is refused on the other listener
([ADR 0083](../../docs/decisions/0083-the-activity-route-has-a-listener-and-tokens-of-its-own.md)). Fail closed, exit 78:
the address with no usable token, tokens with no address, an address that takes `LISTEN_ADDR`'s port, a token in both lists.
**Breaking:** `ServiceConfig` gained `activity`, so a struct literal stops compiling.

**Unset (deprecated)**, it stays where it was: served by every role on `LISTEN_ADDR`, behind `A2A_BEARER_TOKENS` like every
route but `/healthz`, and the process logs once at start that this form is deprecated. Either way it answers
`{"busy": N, "nextDueAt": "<RFC 3339>"}` from the run store, for this service's agents: `busy` counts the open
runs a worker holds a live lease on, `nextDueAt` is the earliest due time of an open run nobody holds (a
parked schedule's wake time included) and is omitted when there is none. No run content, no secret. A store
that does not implement `Store::activity` answers 503. `tests/serve.rs` proves the 401s and the idle answer
(`the_activity_route_needs_the_bearer_and_reports_an_idle_store`, the deprecated form), the listener of its own
(`the_activity_listener_answers_its_own_token_and_that_route_only`: its own token only, that route only, the A2A listener
refusing its token and no longer serving the route; `a_worker_serves_activity_on_its_own_listener`, on Postgres), and exit
78 (`an_activity_listener_without_its_own_tokens_is_78`, `an_activity_address_without_tokens_is_exit_78`); the config's
unit tests prove the refusals (`an_activity_address_without_its_own_tokens_is_refused`,
`activity_tokens_without_an_address_are_refused`, `an_activity_token_that_is_also_an_a2a_token_is_refused`,
`the_activity_address_must_be_an_address_on_another_port`). `activity_conformance!` proves the store half.
