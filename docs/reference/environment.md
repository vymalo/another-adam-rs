# Environment variables

Configuration of both agents is environment variables only. Problems are reported all at once at startup
(`invalid configuration`, exit 78). Sources of truth: `crates/adam-service/src/config.rs` (the common ones),
`bin/adam-coder/src/config.rs`, `bin/adam-agent/src/config.rs`, `crates/adam-env-kubernetes/src/settings.rs`,
`bin/adam-egress/src/config.rs`, `bin/run-signer/src/config.rs`, `crates/adam-git-sandbox/src/config.rs`, and for the one
row below `crates/adam-notify-postgres/src/lib.rs`.
The chart sets these from its values: [chart README](../../deploy/coder/README.md). Secrets are never in
files or logs; they are `SecretString`s and absent from `Debug`.

The roles that run workers (`all`, `worker`) read everything below except where a row says otherwise.
A `control-plane` reads only `ROLE`, `DATABASE_URL`, `ADAM_PG_POOL_SIZE`, `NOTIFY_LOCAL_ONLY`, `A2A_BEARER_TOKENS`, `PUBLIC_URL`, `LISTEN_ADDR`,
`ADAM_AGENT_DIR`, the `SESSION_*` limits and the optional `A2A_PUSH_*`, `A2A_CARD_SIGNING_*` and `A2A_DOCS`, and a chart may set the rest for every role.

### The store `DATABASE_URL` selects

`adam-service` chooses the adapter from the scheme ([ADR 0053](../decisions/0053-sqlite-a-store-service-a-database-per-agent-and-a-static-coder.md),
decision 3). Every adapter is built into the binaries; a library build can take only the one it
needs (`store-postgres`, `store-sqlite`, `store-mongodb` features of `adam-service`).

| `DATABASE_URL` | Store | Signals across processes | Roles |
|---|---|---|---|
| `postgres://…`, `postgresql://…` | `adam-store-postgres` | `LISTEN`/`NOTIFY` (`adam-notify-postgres`) | `all`, `control-plane`, `worker` |
| `mongodb://…`, `mongodb+srv://…` | `adam-store-mongodb`; the path is the database (`adam` when it names none) | none: workers poll | `all`, `control-plane`, `worker` |
| `sqlite://<path>` | `adam-store-sqlite`; one file, one process, WAL | in-process (`LocalNotifier`) | **`all` only** |
| anything else | — | — | exit 78, naming the scheme |

`sqlite:///var/lib/adam/adam.sqlite` is an absolute path; `sqlite://adam.sqlite` and
`sqlite://./adam.sqlite` are relative to the process's working directory; `sqlite://` with no path
is exit 78. The file's `<path>.lock` refuses a second process, and a file stamped with a schema
version newer than the binary's is refused ([store adapters](store-adapters.md#sqlite)).

## Both binaries (`adam-service`)

| Variable | Meaning | Default |
|---|---|---|
| `ROLE` | `all`, `control-plane` or `worker` | `all` |
| `DATABASE_URL` | the run store: `postgres://` or `postgresql://` (PostgreSQL **18 or later**; an older server is refused at startup with exit 78, before any DDL, [ADR 0082](../decisions/0082-the-sql-adapters-keep-their-rules-in-the-database.md)), `mongodb://` or `mongodb+srv://` (MongoDB; its path is the database, `adam` when absent), or `sqlite://<path>` (one SQLite file, one process). Any other scheme is exit 78, and so is `sqlite://` with `ROLE=worker` or `ROLE=control-plane` ([the store each scheme selects](#the-store-database_url-selects)) | required |
| `ADAM_PG_POOL_SIZE` | **Postgres only**: connections in the Postgres store's pool, **per process** (2 to 65535; one is refused because an `adam-notify-postgres` listener holds a connection for the process's lifetime, so a one-connection pool would leave none for a query; **two is the floor, not a size to run** — the default 16 is the intended size). Read by `adam-store-postgres` when it connects, not by `adam-service`'s own configuration: a value that is not an integer in range fails that connect at startup | `16` |
| `ADAM_PG_STATEMENT_TIMEOUT_MS` | **Postgres only**: the longest one statement of the store's pool may run, in milliseconds (0 to 86400000), set as the connections' `statement_timeout`. The server cancels a longer one (SQLSTATE `57014`, a transient error) and the pool stays usable, so one stuck statement cannot hold a pooled connection indefinitely. Migrations are exempt, and the notification listener's connection never gets it. Read by `adam-store-postgres` when it connects: a value that is not an integer in range fails that connect | unset (no bound) |
| `A2A_BEARER_TOKENS` | comma-separated accepted tokens; **none = no server** | required by `all`, `control-plane` |
| `PUBLIC_URL` | where clients reach the JSON-RPC endpoint (goes in the agent card) | required by `all`, `control-plane` |
| `LISTEN_ADDR` | the A2A server, or a worker's `/healthz`; also `GET /internal/activity` (behind the A2A tokens) while `ACTIVITY_LISTEN_ADDR` is unset, a form that is **deprecated** | `0.0.0.0:8080` |
| `ACTIVITY_LISTEN_ADDR` | **turns on the activity listener** (every role): `GET /internal/activity` alone, on this address, behind `ACTIVITY_BEARER_TOKENS`; every other path there is a 404, and the route is gone from `LISTEN_ADDR`. It must not take the port of `LISTEN_ADDR`; anything else is exit 78 ([ADR 0083](../decisions/0083-the-activity-route-has-a-listener-and-tokens-of-its-own.md)) | unset: the route stays on `LISTEN_ADDR` |
| `ACTIVITY_BEARER_TOKENS` | comma-separated tokens the activity listener accepts, and the only ones: an A2A token is refused there and these are refused on the A2A listener. Required with `ACTIVITY_LISTEN_ADDR` (none, or a token also in `A2A_BEARER_TOKENS`, is exit 78: fail closed), refused without it | unset |
| `WORKERS` | runs advanced concurrently | `4` |
| `MAX_PARALLEL_TOOL_CALLS` | the most tool calls of one model turn that run at the same time, 1 to 32 (`1` is one at a time); anything else is exit 78. An agent file's `limits.max_parallel` may lower it for that agent, never raise it. **The chart renders `config.maxParallelToolCalls` in every role**, the control plane included: the control plane reads it too, leniently (a bad value is the default there), to hold a person's `start.config` on a helper to the bound the workers' session tools use ([ADR 0054](../decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md)). Which calls may overlap is the tools' own declaration ([ADR 0049](../decisions/0049-the-tool-calls-of-a-turn-run-at-the-same-time.md)). It also bounds how many commands of one `run_checks` call (`commands`) run at the same time, each still taking a seat of the process pool | `10` |
| `MAX_RUN_CONTINUATIONS` | the most times a `run.ending` hook may keep one run going, 0 to 20; anything else is exit 78. Past it the run ends and its `end:<turn>` step records `capped` ([ADR 0050](../decisions/0050-hooks-around-the-agent-loop-compiled-first-and-programs-second.md)) | `3` |
| `ADAM_HOOKS_FILE` | the deployment's hook programs (ADR 0050, slice P2a): a JSON file with `programs`, each `{name, command, args?, env?, agents?, points?, deadlineMs?}`. `agents` is the filter (absent: every agent), `points.decide` names deciding points only, a relative `command` is resolved against the file's own directory, and a program's deadline, variables and observed events are validated with `adam-hook-exec`'s rules. A file that names a program is exit 78 unless `HOOKS_ALLOW_PROGRAMS=true`, and a file under a directory the agent works on (its folder, its workspace) is refused: a repository never declares a program. Nothing is discovered — no cwd, no agent folder, no message | unset: no program |
| `HOOKS_ALLOW_PROGRAMS` | `true` lets the process run hook programs at all; a file that names one without it is exit 78 | `false` |
| `MAX_TOOL_CALLS_IN_FLIGHT` | the most tool calls of the whole process that run at the same time, 1 to 256; anything else is exit 78. The per-turn `MAX_PARALLEL_TOOL_CALLS` sits inside it ([ADR 0065](../decisions/0065-resource-governance-admission-pools-and-os-limits.md)). A call waits up to a minute for a seat and then runs anyway with a warning: a run never fails because a slot was busy | `4` |
| `MAX_MODEL_CALLS` | the most model calls in flight in the process, 1 to 64; anything else is exit 78 ([ADR 0065](../decisions/0065-resource-governance-admission-pools-and-os-limits.md)). A call that waits five seconds for a seat parks the run at the refusal's `retry_after` | `2` |
| `MAX_PROCESSES` | the most processes of the whole process at once, 1 to 256; anything else is exit 78. A command takes a seat of this pool and waits up to a minute for one; past that the tool reports a retryable `RateLimited` error and the run tries again, so a busy pool never fails a run ([ADR 0065](../decisions/0065-resource-governance-admission-pools-and-os-limits.md)). `adam-coder` governs `run`, `run_command`, the checks, **background commands and language servers**: each holds its seat until it and its descendants are gone, so `BACKGROUND_COMMANDS_MAX` and `LANGUAGE_SERVERS_MAX` count inside this pool. Chart: `config.maxProcesses` | `4` |
| `MAX_GIT_PROCESSES` | the most git processes at once, 1 to 64; anything else is exit 78. Every git of a worker that runs in its own container (not in the git sandbox) takes a seat and waits up to a minute; past it the tool reports a retryable `RateLimited` error. Chart: `config.maxGitProcesses` | `2` |
| `COMMAND_MEMORY_MAX` | the memory one command may use: a number of bytes, or a size with `KB`, `MB`, `GB` (binary: `2GB` is 2 GiB); at least 64 MiB, anything else is exit 78. Over it, the operating system stops the command (a `MemoryMax` of a systemd scope, else `RLIMIT_AS`) and the tool says `stopped: memory limit 2 GB`; the run goes on | a quarter of the RAM, at most 8 GB |
| `COMMAND_CPU_MAX` | the CPU one command may use: cores (`2`, `1.5`) or millicores (`500m`), at least `100m`, anything else is exit 78. Enforced as a `CPUQuota` of a systemd scope; without one it is **not enforced** and `adam-coder` says so at start | half the cores, at least 1 |
| `MODEL_TOKENS_PER_MINUTE` | the model's tokens per minute, 1 to 100000000; anything else is exit 78. Charged to one bucket the whole process shares, an estimate of four characters a token plus `max_output_tokens`; unset or blank meters none | unset |
| `SESSION_MAX_OPEN` | the most sessions of one conversation open at once ([ADR 0045](../decisions/0045-a-subagent-can-run-in-the-background-as-a-session.md)), 1 to 100; the root agent's `limits.max_open_sessions` may lower it for that agent, never raise it; **every role reads it**: the card's `maxOpen` and the refusal of a person's start on a control plane, and the model's own session tools on a worker, enforce the same value, so set it (and the next two) **the same on every role** | `16` |
| `SESSION_MAX_WORKING` | the most rounds of one conversation working at once, 1 to 32 and at most `SESSION_MAX_OPEN` (when it is not set, the default is cut down to `SESSION_MAX_OPEN`, so a small `SESSION_MAX_OPEN` alone is fine; set both and conflicting is exit 78); a start, or a message that would start a round, past it is refused (by a control plane for a person's start, by a worker for the model's); `limits.max_working_sessions` may lower it; the same on every role | `4` |
| `SESSION_MAX_ROUNDS` | the most rounds one session may have, a close included, 2 to 200; `limits.max_session_rounds` may lower it | `20` |
| `SESSION_IDLE_DAYS` | days a kept-open session may sit idle (its latest round ended, nobody wrote to it) before it **reads closed**: it takes no message and frees its place under `SESSION_MAX_OPEN`; a round that works never expires. 1 to 365. Set it the same on every process that serves one database, and purge finished runs only after longer than this | `7` |
| `RUN_RETENTION_DAYS` | days a **finished** (done or failed) run is kept, with its journal, before a worker deletes it. 1 to 36500, or `off` to keep everything. **Destructive default:** unset deletes finished runs older than 90 days; a run that is open is never touched. Read by every role, swept by `all` and `worker` | `90` |
| `RUN_RETENTION_SWEEP_SECS` | seconds between two retention sweeps (one also runs at start). 60 to 86400. Several workers may sweep at once | `3600` |
| `WORKER_ID` | lease identity: 1 to 128 of letters, digits, `.`, `_`, `-`, not starting with `.` | random per process; required by `affinity`, `isolated` |
| `MODEL_BASE_URL`, `MODEL_API_KEY` | OpenAI-compatible gateway (with `/v1`) and key | required by `all`, `worker` (key may be empty; not with `MODEL_AUTH=device` or `injector`) |
| `MODEL_AUTH` | `key` (send `MODEL_API_KEY`) or `device`: sign in to an OAuth 2.0 issuer with the device grant and renew the token ([ADR 0058](../decisions/0058-the-models-credentials-come-from-a-provider-asked-for-each-call.md), [`adam-model-oauth`](../../crates/adam-model-oauth/README.md)). The code and the link are a `WARN` log line at startup (`sign in to use the model: open … and enter the code …`); a model call waits for the sign-in at most 120 s, then is retried. A denied or expired code, or a refused renewal, makes every call fail until a restart. With `device`, a non-empty `MODEL_API_KEY` is exit 78. `injector` ([ADR 0067](../decisions/0067-a-model-call-through-an-injector-names-its-run-and-a-refusal-parks-the-run.md)): no key at all; every model call carries `X-Run-Id: <root run id>` and no `Authorization`, for an injector in front of the model that adds the run's credential; a subagent's calls name the root of its task, and a session's round the task that sent it (the opener for round one); a non-empty `MODEL_API_KEY`, or any `MODEL_OAUTH_*`, beside it is exit 78. With `injector` (or a [turn key](#turn-signing)) the card also lists `prompt-statement/v1`, and a run whose message brought a prompt statement sends `Prompt-Statement` and `Prompt-User-Signature` on every model call ([ADR 0077](../decisions/0077-a-model-call-carries-the-prompt-statement-and-the-persons-envelope.md)); every role reads the word for the card, a control plane nothing else of the model. Anything but `key`, `device` or `injector` is exit 78 | `key` |
| `MODEL_CA_FILE` | a PEM file of certificates trusted for the model endpoint, beside the default roots (not for the device grant's issuer; ADR 0067); a file that cannot be read, is not PEM or holds no certificate is exit 78 | unset: the default roots |
| `MODEL_PARK_LIMIT` | how long a run may wait on the model on a timer, from the first park in a row, before it fails with the last reason ([ADR 0067](../decisions/0067-a-model-call-through-an-injector-names-its-run-and-a-refusal-parks-the-run.md)): a model call the gateway refused under a provider that parks (`MODEL_AUTH=injector`) does not fail the run, which waits 30 s, then twice as long at each park in a row up to 15 min, and makes the same call again; behind the injector a gateway that cannot be reached parks too, once the retries are spent. A park until woken (a usage limit reached) is not counted: it never fails on its own, and pauses the run's deadline. Whole seconds, or a number with `s`, `m`, `h` or `d` (`24h`), from 60 s to 30 d; anything else is exit 78. See [the A2A server](a2a-server.md#a-run-parked-on-the-model) | `24h` |
| `MODEL_OAUTH_ISSUER` | with `device`: the issuer; its `/.well-known/openid-configuration` names the device and token endpoints, which must be on its origin. `https`, or `http` on a loopback address (else exit 78) | required with `device` |
| `MODEL_OAUTH_CLIENT_ID`, `MODEL_OAUTH_SCOPE` | with `device`: the public client the issuer knows the process as, and the scope asked for | client required with `device`; scope `offline_access` |
| `MODEL` | the agent's model alias | required by `all`, `worker` |
| `MODEL_ALIASES_FILE` | the per-model profiles (ADR 0063, ADR 0064): **JSON only for now** (the workspace has no YAML parser), one object with `profiles` (by model id) and `presets` (by name). A profile declares the reasoning levels the model takes and how they are sent (`reasoningEfforts`, `reasoningWire`, `reasoningBudgets`, `reasoningEffort`), its output cap (`maxOutputTokens`, `maxOutputTokensLimit`), the members merged into its requests only (`extraBody`), the preset it starts from (`preset`), and the model a run continues on once when the endpoint refuses this one as locked (`fallback`: another profile of the file, also allowed in a preset; ADR 0064, amended 2026-11-23); `reasoningEfforts: []` says the model takes none. Read and validated at startup: a file that cannot be read, is not JSON, or breaks a rule (an unknown level or wire, a duplicate level, a budget that is not for `thinking_budget`, misses a level or is `0`, `none` with the `thinking_budget` wire, which has no disabled form, an effort outside the model's levels, an unknown preset, a cap of 0 or above the limit, a reserved `extraBody` member) is exit 78, naming the file and the key. The assembly resolves each agent's default effort from its folder's `reasoning_effort`, its `model_preset`, the profile and the profile's preset, and refuses a level the model does not take; the output cap is resolved the same way — the folder's `limits.max_output_tokens`, the preset's, the profile's, the profile's base preset, then 128,000 — and clamped to `maxOutputTokensLimit` and to the window less 20,000 and the history the agent keeps. The client applies the model's `reasoningWire` to each request's typed level, after `MODEL_EXTRA_BODY` and removing only the members the wire would duplicate (ADR 0063 decision 6); the profile's `extraBody` is a later slice (ADR 0064). A process whose `MODEL_ALIASES_FILE` declares no `maxOutputTokensLimit` for `MODEL` (no profile, or one without the key) warns at startup, naming the 128,000 fallback and the window; with no profiles file the same fallback is logged at `info`. The chart mounts it from `config.modelProfiles` | unset: no model has a profile |
| `MODEL_EXTRA_BODY` | JSON object merged into every chat request, e.g. `{"reasoning_effort":"medium"}`; **not a secret**; may not set `model`, `messages`, `tools`, `tool_choice`, `stream`. A request's typed reasoning level wins over the members its model's `reasoningWire` would duplicate (ADR 0063 decision 6) | unset |
| `MODEL_ECHO_REASONING` | `reasoning_content` or `reasoning`: send earlier reasoning back under that name (DeepSeek thinking mode with tools needs it) | unset |
| `MODEL_PROMPT_CACHE_KEY` | `off` or `conversation`: with `conversation` every request carries `prompt_cache_key`, the first 32 hex digits of the SHA-256 of the conversation id (never the id; an unsalted hash, so whoever can guess an id can confirm it, and it goes only to the model provider), so a gateway with a prompt cache routes the requests of one conversation together. A `MODEL_EXTRA_BODY` member of the same name wins. Off by default: that a gateway accepts the member is *unverified*; anything else: exit 78 | `off` |
| `MODEL_CONTEXT_WINDOW` | the context window of the model `MODEL` names, in tokens (1 to 9007199254740991): the `contextWindow` of each `usage/v1` call report on that alias, so a screen shows how full the context is ([ADR 0032](../decisions/0032-usage-per-model-call.md)). **It also sets when the history is compacted**: at the window less 20,000 tokens less `max_output_tokens`, or at `limits.max_history_tokens` when that is smaller ([Compaction](../architecture.md#compaction-a-long-history-is-summarised-once)); a subagent whose `model:` names another alias has none (reports nothing, compacts at its own `max_history_tokens`). **It also clamps the output cap**: no call asks for more than the window leaves after 20,000 tokens and the history the agent keeps (ADR 0064). Anything else is a startup problem (exit 78). The chart sets it from `config.modelContextWindow` | unset: reports carry no window |
| `MODEL_INPUT_MODALITIES` | `text` or `text,image`: what the model `MODEL` names can read. With `image`, an image the person sends or a tool returns reaches the model as a `data:` URL part, within the four limits below; without it the model is given one line of text in the image's place. What is sent follows these settings **now**, so changing one changes the prompt once for a conversation that holds images. Never guessed from a model name; it is the model `MODEL` names only, so a subagent on another alias stays text only ([images](a2a-server.md#images-in)) | `text` |
| `MODEL_IMAGE_MAX_BYTES` | the most bytes of one image sent (1 to 4194304) | `1048576` |
| `MODEL_IMAGE_MAX_COUNT` | the most images one run's history holds (1 to 32): the ninth is a stand-in, the first eight stay. `MODEL_IMAGE_MAX_BYTES` times this may be at most 32 MiB, since a request resends every admitted image; over that is exit 78 naming both | `8` |
| `MODEL_IMAGE_MAX_SIDE` | the longest side of an image, in pixels, read from its header (64 to 16384); nothing is decoded or resized | `4096` |
| `MODEL_IMAGE_SOURCES` | which images may reach the model: a comma-separated subset of `person`, `workspace` (the coder's `read_file`), `mcp` (an MCP server's result; opt-in because a server's call can carry expanded credentials, and **an image is never scrubbed**) | `person,workspace` |
| `MODEL_OUTPUT_MODALITIES` | `text`. `image` is refused at startup (exit 78): a model that makes images inline is not built for any provider yet; an image is made by an MCP server ([`painter`](agent-files.md#timeouts-instructions-and-prompts)) | `text` |
| `MCP_ALLOW_STDIO` | let `mcp.json` start local processes | `false` |
| `MCP_ALLOW_INSECURE` | allow plain `http` MCP servers on other machines; development only | `false` |
| `MCP_ALLOW_URL_VARS` | allow `${VAR}` in a server `url` (the SDK logs URLs) | `false` |
| `MCP_MAX_CALL_SECS` | the longest one MCP server is waited for, a call and a connection each: what `timeoutSecs` and `connectTimeoutSecs` ask for and the defaults (60 s and 30 s) are lowered to it, and a value the file asks for is at least 1 s (1 to 86400); a larger value is lowered to it with a warning at startup ([`mcp.json`](agent-files.md#timeouts-instructions-and-prompts)) | `900` |
| `THREAD_TOOLS_MAX_CALL_SECS` | longest wait for a thread-tool call (1 to 86400) | `3600` |
| `RUST_LOG` | log filter; JSON logs on stdout; replaces the default whole | `info,rmcp=warn` |

### Optional A2A features (roles that serve A2A: `all`, `control-plane`)

Nothing here is on by default but the docs; a role that does not serve A2A reads none of it. A bad value is a startup
problem (exit 78). [ADR 0030](../decisions/0030-a2a-push-notifications-list-tasks-extended-card-signatures.md),
[ADR 0031](../decisions/0031-swagger-ui-and-the-a2a-rest-binding.md), [what a client sees](a2a-server.md).

| Variable | Meaning | Default |
|---|---|---|
| `A2A_PUSH_ALLOWED_URLS` | **turns push notifications on** and names the webhooks they may reach: comma-separated URL prefixes (`https://hooks.example.com/a2a/`) or hosts (`hooks.example.com`, `*.example.com`, `host:8443`). A client's webhook that matches none, is not `https` or is a private address is refused | unset: off, the card says `pushNotifications: false` |
| `A2A_PUSH_ALLOW_PRIVATE` | also allow webhooks on loopback, private and link-local addresses, and `http` to loopback. **Development only** | `false` |
| `A2A_PUSH_GIVE_UP_AFTER_SECS` | how long one notification may keep failing before delivery to that webhook is abandoned (1 to 604800) | `3600` |
| `A2A_PUSH_REQUEST_TIMEOUT_SECS` | how long one request to a webhook may take (1 to 120) | `15` |
| `A2A_CARD_SIGNING_KEY_FILE` | a PKCS#8 PEM private key, ECDSA P-256 or Ed25519, that signs the public and the extended card; mount it from a Secret; must exist and be usable | unset: the card is unsigned |
| `A2A_CARD_SIGNING_KEY_ID` | the signature's `kid`; needs the key file | the key's RFC 7638 thumbprint |
| `A2A_CARD_SIGNING_JKU` | the `jku` in the signature's header (where clients fetch the key set; the server serves it at `/.well-known/jwks.json`); needs the key file | unset |
| `A2A_DOCS` | Swagger UI at `/docs` and the OpenAPI document at `/openapi.json`, both public (the calls still need a token); `false` (or `0`) turns them off | `true` |

### Turn signing

adam signs **every model request** with the turn key when one is given: each call carries `Turn-Signature`, adam's
signature of that request (its run, method, path and query, the SHA-256 of its exact body, the prompt's id, the time
and a nonce), beside `X-Run-Id` and the run's prompt headers ([ADR 0068](../decisions/0068-adam-signs-its-own-history.md),
*Amended 2026-11-25 (the request signature)*). Read with the model's configuration, so by every role that calls a
model (`all`, `worker`). A run with no prompt statement is signed with `prompt_jti` `null`; a call adam cannot sign is
not sent ([errors](errors.md)). Signing the turns of a run's history (slice T3) does not exist yet; slice T2 will make
the key required. The turn key is not the card's key.

| Variable | Meaning | Default |
|---|---|---|
| `TURN_SIGNING_KEY_FILE` | a PKCS#8 PEM Ed25519 private key (`-----BEGIN PRIVATE KEY-----`, `openssl genpkey -algorithm ed25519`), mounted from a Secret. Set: every model call is signed. A file that cannot be read, a public key or a key that is not Ed25519: exit 78 | unset: no call is signed |
| `TURN_SIGNING_KEY_ID` | the `kid` of its signatures: the vault reference `vault:<name>#v<n>`, or the id a registry knows the key by (printable ASCII, at most 256 bytes); without the key file: exit 78 | the key's RFC 7638 thumbprint |
| `TURN_VERIFY_KEYS_FILE` | a JWKS of the public keys of earlier versions, each with its `kid` (`kty` `OKP`, `crv` `Ed25519`; a private member `d` is refused), and `revoked`, a list of the ids no longer accepted; not secret (a ConfigMap). Read and checked with the key; used to verify a history (T3); without the key file: exit 78 | the signing key's own public key only |

## Cross-process notifications (`adam-notify-postgres`)

`adam-coder`, in every role (`all`, `control-plane`, `worker`): the `notify` component. Both binaries'
shared configuration parses the variable, so a bad value is exit 78 in either; `adam-agent` runs no
`notify` component, so the value has no effect there. It also has no effect on a store without a
cross-process listener: SQLite's notifier is always in-process, and MongoDB has none. Source:
`crates/adam-notify-postgres/src/lib.rs`, [the crate README](../../crates/adam-notify-postgres/README.md).

| Variable | Meaning | Default |
|---|---|---|
| `NOTIFY_LOCAL_ONLY` | `true`/`1` or `false`/`0` (any case): keep this process's events local. `emit` delivers them to the in-process sink and does not encode, queue or `NOTIFY` them; every process's listener drops an event of its own origin anyway. Signals (wake-ups, cancels) still cross. One **process** (`ROLE=all`, one pod): with `topology: split` (a front and its workers, even one replica each) it silences the live events between them, whose state changes still reach a subscriber's 2 s fallback poll. A deployment choice, never detected. Anything else: a startup problem, exit 78 | `false` |
| `UPLOADS_INLINE_MAX_BYTES` | the largest file the agent sends inline (`uploads/v1`: a larger one asks the orchestrator for an upload slot), a whole number of bytes from 1 to 268435456 (256 MiB); set it to the orchestrator's `artifacts.maxFileBytes`. The card lists the extension with no `params`. The agent folder's `limits.max_inline_upload_bytes` wins over it; the chart value is `uploads.inlineMaxBytes`. A malformed or out-of-range value: a startup problem, exit 78. Read by every role that serves A2A | 10 MiB |
| `DEFAULT_LOCALE` | the agent's default language, one well-formed BCP 47 tag (at most 64 bytes): the card lists `locale/v1` with it ([ADR 0069](../decisions/0069-the-interface-language-is-a-hint-and-a-hint-is-all-it-is.md)). Unset: the card lists nothing. A malformed tag: a startup problem, exit 78. Read by every role that serves A2A. In the chart: `config.extraEnv` | unset |

## Agent folder (both binaries)

| Variable | Meaning | Default |
|---|---|---|
| `ADAM_AGENT_DIR` | the folder of `agent/` files, read once at startup by every role; must exist (else exit 78). Required by `adam-agent` | `adam-coder`: its embedded copy |
| `ADAM_EXTRA_MCP_FILE` | a file of extra MCP servers in the shape of `mcp.json`, **added** to the agent's own (a name clash is exit 78) | unset |
| `ADAM_REFERENCES_FILE` | `adam-coder`, roles that run workers: a file `{"references": [{"name", "url", "branch"?, "description"}]}` of repositories the coder may **read** beside the one it works on and never write, **added** to the `references:` of the agent's files; it must be an existing file of valid references whose addresses the workspaces' policy accepts (an allowed host, an owner a GitHub App acts for), and a name the agent already declares is exit 78. See [References](workspace-and-environments.md#references) | unset |
| `A2A_ALLOW_INSECURE_REMOTES` | `adam-agent`, roles that run workers: let a remote subagent (`a2a:`) be at plain `http` on another machine, such as a service of the same cluster; without it such a URL is exit 78. **The messages and the bearer then cross the pod network in clear text**: protect it with a NetworkPolicy that admits only the caller, or mesh mTLS. Plain `http` goes only to the card's own host, and never from an `https` card. Its token is the environment's too: `auth: bearer:VAR` names the variable ([remote subagents](agent-files.md#remote-subagents-a2a)) | `false` |
| `SI_MEMORY` | the deployment's ceiling on the self-improvement memory (ADR 0081 decision 11), `on` or `off`; anything else is exit 78. It only **narrows** a folder's `si_memory:` key: `off` here is off whatever the folder says, and a folder's `off` is off whatever this says ([the key](agent-files.md#the-self-improvement-memory-si_memory)). Read by the roles that run workers | `on` |

## The coder (`adam-coder`)

| Variable | Meaning | Default |
|---|---|---|
| `GITHUB_TOKEN` | push and pull request token, sent only to `ALLOWED_REPO_HOSTS`. Unset in App mode | one of this or the App's |
| `GITHUB_APP_ID` | App mode: the App's id (the JWT's `iss`) | required in App mode |
| `GITHUB_APP_INSTALLATION_ID` | **pins** the App to one installation | this or `GITHUB_APP_OWNERS`, exactly one |
| `GITHUB_APP_OWNERS` | no pin: the accounts the App may act for (comma or space list, or `*`); the installation of each owner is found with the JWT | as above |
| `GITHUB_APP_PRIVATE_KEY_PATH`, `GITHUB_APP_PRIVATE_KEY` | the App's PEM key, a file or inline; exactly one; parsed at startup | one required in App mode |
| `ALLOW_COMMANDS_NEXT_TO_SECRETS` | roles that run workers: `true` accepts, `false` refuses at startup (**exit 78**), a GitHub App whose key is a **file** (`GITHUB_APP_PRIVATE_KEY_PATH`) while the commands of a run run in the coder's own container (`RUN_ENVIRONMENT=local`): a command can read the key, mint installation tokens and act as the App at any access tier. Accepted, the coder still logs a warning. Not needed with `RUN_ENVIRONMENT=kubernetes`, an inline key or a token. `true`/`1`/`false`/`0`; anything else is exit 78. [Upgrade note](../guides/deploy-the-coder.md#upgrading-an-app-deployment) | `false` |
| `ALLOWED_REPO_HOSTS` | hosts repositories may live on (`name` or `name:port`); the token is scoped to them; the first is what `owner/name` means | `github.com` |
| `GITHUB_API_URL` | GitHub REST API root (Enterprise: `https://<host>/api/v3`) | `https://api.github.com` |
| `GITHUB_MCP_URL` | origin of the GitHub MCP server (the sidecar); no credentials | `http://127.0.0.1:8082` |
| `CREATE_REPO_OWNERS` | owners `create_repository` may create for, after the person agrees (not under the `bypass` access tier). A list restricts to exactly these. Empty: a GitHub App that names its accounts (a pin, or `GITHUB_APP_OWNERS` as a list, **not** `*`) creates for them, a token creates nothing (the tool is off) | empty |
| `MAX_ACCESS_TIER` | the highest access tier a person may pick for a message: `plan`, `edits`, `auto` or `bypass`. The card lists the tiers up to it, a higher one is refused, the tools check it again. `bypass` (rename and delete repositories, no consent form) is opt-in. Read by every role; a word that is not a tier is exit 78 | `auto` |
| `ALLOW_LOCAL_REPOS` | accept local paths, `file://`, plain `http://`; development and tests only | `false` |
| `WORKSPACE_ROOT` | mirrors, run workspaces, run notes | `/work` |
| `GIT_SANDBOX_SOCKET` | the Unix socket of an `adam-git-sandbox` server (an absolute path; a relative one is exit 78). When it is set, the `git` commands that carry **no credential** (`status`, `add`, `diff`, `commit` and its plumbing, `worktree`, `config`, the `apply` of a patch, ...) run **there**, where no secret is readable, instead of in the coder's container; `fetch`, `push` and `ls-remote` stay in the coder, on the bare mirror, after its configuration has been put back to what the workspace writes (a key it did not write is removed). The sandbox's container must mount `WORKSPACE_ROOT` at the same path. A sandbox that cannot be reached makes those commands fail (retried as a transient failure): **they are never run in the coder's container instead**. The chart sets it, to the socket of its `git-sandbox` container, whenever `runPods.enabled` is on ([the chart's section](../../deploy/coder/README.md#the-git-sandbox)). Unset (the default, local mode, run pods off): git runs in the coder's container, which is **not** protected from the repositories a run controls ([the git sandbox](../../crates/adam-git-sandbox/README.md)) | unset |
| `WORKSPACE_PLACEMENT` | `shared`, `affinity` or `isolated` ([placement](workspace-and-environments.md#placement-which-worker-holds-a-runs-files)) | `shared` |
| `WORKSPACE_SWEEP_SECS` | how often the janitor removes finished runs' workspaces; `0` is off | `300` |
| `SCRATCH_IDLE_DAYS` | the idle time of a conversation: days after its last task before the janitor removes its images. A scratch project is never removed, it holds unpushed work ([thread-long scratch](workspace-and-environments.md#a-runs-workspace)) | `7` |
| `MAX_CHECK_CYCLES` | failed `run_checks` in a repository before the agent must stop | `3` |
| `SCRATCH_CHECK_CYCLES` | the same for a scratch project, counted apart | `5` |
| `CHECK_TIMEOUT_SECS`, `CHECK_OUTPUT_TAIL_BYTES` | limits of one `run_checks`, `run_command` or `run`: the time limit is also the most a call's `timeout` may ask for (a larger one is an error); an output over the size is shown as head and tail, the whole kept for `read_output` | `900`, `16384` |
| `BACKGROUND_COMMANDS_MAX` | commands of one run that keep running (`start_command`) at once, 1 to 16; `0` is refused: exit 78 naming the variable | `4` |
| `BACKGROUND_COMMAND_MAX_SECS` | how long such a command may run before it is stopped, 60 to 86400; `0` is refused (exit 78) | `3600` |
| `BACKGROUND_SWEEP_SECS` | how often the sweep of each worker looks at the commands it holds: it stops those of a run that is over (a cancel never calls the agent, so this is how long its commands can outlive it), carries out a stop another worker asked for (within one sweep), and writes the **beat** of the others. A record whose beat is older than the larger of three sweeps and 60 s is a dead worker's: set the same value on every worker, and keep the clocks of the workers within 60 s. 1 to 3600; `0` is refused (exit 78) | `15` |
| `LANGUAGE_SERVERS` | `on` offers `file_diagnostics`, `find_definition` and `find_references` (a language server of the run's environment, kept between calls: [The coder agent](coder-agent.md#language-servers)); `off` removes the three tools from the set; anything else is refused: exit 78 naming the variable. Chart: `config.languageServers` | `on` |
| `LANGUAGE_SERVER_IDLE_SECS` | how long a language server that no call uses is kept before it is stopped, 30 to 86400; `0` is refused (exit 78) | `600` |
| `LANGUAGE_SERVERS_MAX` | the most language servers this worker holds at once, for all its runs: the least recently used one that is not working is stopped for the next, and when every one is working the call says so. 1 to 32; `0` is refused (exit 78) | `4` |
| `GIT_AUTHOR_NAME`, `GIT_AUTHOR_EMAIL` | identity of the commits | `adam-coder`, `adam-coder@users.noreply.github.com` |
| `PR_DRAFT` | open pull requests as drafts | `false` |

## Where a run's processes run (`adam-coder`)

| Variable | Meaning | Default |
|---|---|---|
| `RUN_ENVIRONMENT` | `local` (this container: on a desktop, the person's own work sandbox) or `kubernetes` (a pod for each run). With a GitHub App key file, `local` needs `ALLOW_COMMANDS_NEXT_TO_SECRETS=true` (above) | `local` |
| `RUN_POD_TEMPLATE_FILE` | with `kubernetes`: the Pod every run pod is made from (the chart's ConfigMap). Its run container's `resources.requests` are also the shape a command's time is weighted by in the task's `sandbox` usage ([ADR 0043](../decisions/0043-sandbox-time-as-usage.md)); a request that is not a Kubernetes quantity stops the start | required |
| `RUN_POD_NAMESPACE` | the namespace of run pods | the ServiceAccount's namespace |
| `RUN_POD_INSTANCE` | the release's `app.kubernetes.io/instance`, which finds this deployment's pods | required |
| `RUN_POD_CONTAINER` | the template container commands run in | `run` |
| `RUN_POD_EXEC_BINARY` | the program that runs a command in a pod | `adam-kube-exec` |
| `RUN_POD_READY_TIMEOUT_SECS` | how long a pod may take to be ready | `300` (chart: `600`) |
| `RUN_POD_IDLE_SECS` | idle time before a pod is deleted; `0` keeps it until the run ends | `900` |
| `RUN_POD_WAIT_SECS` | how long a run retries for a pod the cluster cannot give now | `600` (chart: `1020`) |
| `RUN_POD_REPOSITORY_IMAGES` | with `kubernetes`: the image prefixes a repository's `devcontainer.json` may name for its run pod (`use_repository_environment`, [ADR 0052](../decisions/0052-no-podman-a-repositorys-own-environment-is-a-run-pod-of-its-image.md)), separated by commas or spaces. Each ends with `/` (a registry path such as `ghcr.io/acme/`) or `:` (a tag such as `ghcr.io/acme/tool:`), so that a bare host cannot admit a look-alike; no `*` and no `${...}`. Empty: the tool is never offered and the run pods use the template's image | empty (chart: `runPods.repositoryImages.allowedPrefixes`) |

### Removed, ignored with a warning

The coder no longer uses OpenCode ([ADR 0038](../decisions/0038-the-coder-drops-opencode-and-adam-acp-stays-a-library.md)),
nor a repository's devcontainer on a container engine ([ADR 0052](../decisions/0052-no-podman-a-repositorys-own-environment-is-a-run-pod-of-its-image.md),
a hard cutover). These variables are **not read and not refused**: a deployment rendered from an older chart still sets
one, and nothing is less safe without it. For each one that is set, the process logs one warning that names the variable
(never its value), in every role, once the configuration is valid: a start that fails validation (exit 78) does not warn.

| Variable | Was |
|---|---|
| `OPENCODE_MODEL` | model alias OpenCode used through the same gateway (default `MODEL`) |
| `OPENCODE_COMMAND` | the ACP program and arguments (default `opencode acp`) |
| `OPENCODE_BINARY` | the native OpenCode executable mounted into containers |
| `DEVCONTAINER_RUNTIME` | `off` or `podman`: where a run's commands ran. **The one exception**: the value `podman` is exit 78, with a message naming ADR 0052, because that deployment asked for the removed path. Every other value is ignored with a warning |
| `CONTAINER_HOST` | the address of the container engine's service |
| `DEVCONTAINER_DEFAULT_IMAGE` | the image of a repository with no `devcontainer.json` |
| `DEVCONTAINER_NETWORK` | `inherit` or `none` |
| `DEVCONTAINER_DEPLOYMENT_ID` | the label the orphan sweep found this deployment's containers by |
| `DEVCONTAINER_UP_TIMEOUT_SECS`, `DEVCONTAINER_SETUP_TIMEOUT_SECS` | pulling, building and creating; lifecycle commands |
| `DEVCONTAINER_PREPULL` | pull the default image at startup |
| `DEVCONTAINER_CLI`, `DEVCONTAINER_PODMAN`, `DEVCONTAINER_CACHE_DIR` | the devcontainer CLI, the container engine's client, its cache |

## The run signer (`run-signer`)

The sidecar that signs a run's model requests with the run's own certificate
([ADR 0086](../decisions/0086-a-run-signs-its-model-requests-with-a-certificate.md)): an agent points
`MODEL_BASE_URL` at it with `MODEL_AUTH=injector`, and it forwards each call to the model gateway with a
`Turn-Signature` added ([ADR 0068](../decisions/0068-adam-signs-its-own-history.md), *Amended 2026-11-25 (the request
signature)*). Source: `bin/run-signer/src/config.rs`. Problems are reported all at once (exit 78).

| Variable | Meaning | Default |
|---|---|---|
| `RUN_SIGNER_LISTEN` | the address to listen on. **Loopback only**: anything else is exit 78, because the proxy signs a caller's request with the run's key | `127.0.0.1:8443` |
| `RUN_SIGNER_UPSTREAM` | **required**: the model gateway's base URL, absolute `http` or `https` with a host, no query and no fragment. The request's own path and query are appended to it | required |
| `RUN_SIGNER_UPSTREAM_CA_FILE` | the PEM anchors the upstream client trusts beside the default roots. A file that cannot be read, is empty, is over 1 MiB or holds no certificate is exit 78 | `RUN_CERT_CA_FILE`'s bytes: the broker and the gateway are one trust domain by design today |
| `RUN_CERT_ISSUER`, `RUN_CERT_ISSUER_URL`, `RUN_CERT_TOKEN_FILE`, `RUN_CERT_CA_FILE`, `RUN_CERT_RENEW_FRACTION` | **required here**, unlike the agents: a signer with no issuer cannot sign, so every one of them unset is exit 78 naming the three required ones, and a half-set configuration is exit 78 with the same problems the agents report ([their rows](#turn-signing)). `RUN_CERT_CA_FILE` is trusted for **both** the broker and the gateway. The token file is re-read on every call, never cached and never logged | required |

A request it refuses is an HTTP status, not an exit: `400` for a missing or malformed `X-Run-Id` or `Prompt-Statement`
(a run certificate names the person, so the statement is required), `413` for a body over 16 MiB, `503` when the issuer
or the gateway cannot be reached, and `502` for any other signing failure.

## Egress proxy (`adam-egress`)

The HTTPS-only proxy of the coder image (`bin/adam-egress`, [its README](../../bin/adam-egress/README.md)). It reads
nothing of the rows above. Source: `bin/adam-egress/src/config.rs`. Problems are reported all at once (exit 78).

| Variable | Meaning | Default |
|---|---|---|
| `EGRESS_LISTEN` | the address to listen on | `0.0.0.0:3128` |
| `EGRESS_ALLOW_FILE` | a file of allowed hosts, one per line; `#` starts a comment; blank lines are ignored; must be a readable file | unset |
| `EGRESS_ALLOW` | allowed hosts, comma-separated; added to the file's. `example.com` is that host, `.example.com` every subdomain of it and not the apex. An entry that is neither (`*.example.com`, a URL, an IP address) is exit 78, and so is an empty list: **at least one host is required** | unset |
| `EGRESS_PORTS` | the ports a `CONNECT` may name, comma-separated | `443` |
| `EGRESS_DENY_CIDRS` | IPv4 ranges (`10.96.0.0/12`) never connected to, on top of the built-in private, loopback, link-local, shared, multicast, documentation and reserved ranges: the cluster's own. A bad range is exit 78; an IPv6 range is accepted and has no effect, since every IPv6 address is dropped | unset |
| `EGRESS_IDLE_SECS` | a tunnel that moves no byte for this long is closed (1 to 86400) | `300` |
| `EGRESS_MAX_CONNECTIONS` | connections served at once; the next gets `503` (1 to 1000000) | `1024` |
| `RUST_LOG` | log filter; JSON logs on stdout | `info` |

## Git sandbox (`adam-git-sandbox`, `adam-git`)

The server that runs the coder's local `git` where no secret is readable, and the relay that stands in for `git`
(`crates/adam-git-sandbox`, [its README](../../crates/adam-git-sandbox/README.md)). Both ship in the coder image. The
server reads nothing of the rows above; the coder reads `GIT_SANDBOX_SOCKET` (above). Source:
`crates/adam-git-sandbox/src/config.rs`. Problems are reported all at once (exit 78).

| Variable | Read by | Meaning | Default |
|---|---|---|---|
| `GIT_SANDBOX_LISTEN` | the server | the Unix socket to listen on: an absolute path, made with mode `0600`. A path that exists and is not a socket, or a socket a server answers on, is exit 71; a socket nobody answers on is replaced | required |
| `GIT_SANDBOX_ROOT` | the server | the work volume: an absolute path of an existing directory. A command runs in a directory below it (no link at any component) and nowhere else. It is the coder's `WORKSPACE_ROOT`, at the same path | required |
| `GIT_SANDBOX_GIT` | the server | the `git` to run: an absolute path of an executable file. The server runs this program and no other | the first `git` on `PATH`, found once at startup |
| `GIT_SANDBOX_MAX_COMMANDS` | the server | commands run at once (1 to 4096); the next waits | `64` |
| `RUST_LOG` | the server | log filter; JSON logs on stdout | `info` |
| `GIT_SANDBOX_SOCKET` | the relay `adam-git` | the server's socket (an absolute path). Unset: `adam-git` says so and ends with 78; it never runs git itself | required by the relay |

## Local stack (`compose.yaml`)

Put these in a `.env` next to `compose.yaml`.

| Variable | Meaning | Default |
|---|---|---|
| `POSTGRES_PORT`, `MONGODB_PORT` | host ports of the databases | `5432`, `27017` |
| `MOCK_OPENAI_PORT`, `MOCK_GITHUB_PORT`, `MOCK_GITHUB_MCP_PORT`, `MOCK_IMAGE_MCP_PORT`, `GIT_SERVER_PORT` | host ports of the mocks | `8081`, `8082`, `8085`, `8086`, `8083` |
| `CODER_PORT`, `AGENT_PORT` | host ports of the coder and the general agent | `8080`, `8084` |
| `ADAM_BUILD_REVISION` | build argument, not a runtime variable: the commit baked into both binaries, shown on the card as the version's `+<sha7>` and in `build/v1` ([ADR 0028](../decisions/0028-the-card-says-which-build-answers.md)). Unset says `unknown` | unset |
| `CODER_IMAGE` | image of both agents; with `--no-build` runs a prebuilt one | `adam-rs/coder:dev` |
| `CODER_MODEL`, `AGENT_MODEL` | the scripted models | `mock-coder`, `mock-assistant` |
| `AGENT_MCP_ALLOW_INSECURE` | `MCP_ALLOW_INSECURE` of the `agent` service: plain `http` to another container, for a folder whose `mcp.json` names a mock (the painter); development only | `false` |
| `MODEL_BASE_URL`, `MODEL`, `MODEL_API_KEY`, `MODEL_AUTH`, `MODEL_OAUTH_ISSUER`, `MODEL_OAUTH_CLIENT_ID`, `MODEL_OAUTH_SCOPE`, `MODEL_CONTEXT_WINDOW` | read only with `-f dev/compose.real-model.yaml`: the real model of the `agent` and the `coder` (`MODEL_BASE_URL` and `MODEL` required; a key, or `MODEL_AUTH=device` with an issuer and a client). Export the key in your shell rather than writing it in `.env` | unset; `key`; `131072` for the window |
| `CODER_AGENT_DIR`, `AGENT_FOLDER` | the agent folders mounted at `/etc/adam/agent` | `./bin/adam-coder/agent`, `./dev/agents/assistant/agent` |

## Tests

| Variable | Meaning |
|---|---|
| `ADAM_TEST_POSTGRES_URL`, `ADAM_TEST_MONGODB_URI` | the database suites; skipped when unset |
| `ADAM_TEST_TABLE_PREFIX`, `ADAM_TEST_MONGO_COLLECTION_PREFIX` | the Postgres table prefix (`adam_test_`) and the MongoDB collection prefix (`adam_`) the store suite and the transcript measurement write to; set the same value on `dev/p2-measure.sh` |
| `ADAM_TEST_REQUIRE_DB=1` | a suite whose variable is unset **fails** instead of skipping (CI sets it) |
| `ADAM_TEST_MOCK_OPENAI_URL`, `ADAM_TEST_MOCK_GITHUB_URL`, `ADAM_TEST_MOCK_GITHUB_MCP_URL` | the tests of the real clients against the compose mocks |
| `ADAM_TEST_KUBECONFIG`, `ADAM_TEST_REQUIRE_KUBERNETES=1` | the cluster test of `adam-env-kubernetes` (more `ADAM_TEST_KUBE_*`, among them `ADAM_TEST_KUBE_ISOLATION_MIRROR` and `_ADMIN` for the isolation scenario, in `crates/adam-env-kubernetes/tests/cluster.rs`) |
| `ADAM_TEST_LANGUAGE_SERVERS=1`, `ADAM_TEST_REQUIRE_LANGUAGE_SERVERS=1` | the suite of the real language servers (`bin/adam-coder/tests/real_language_servers.rs`); a language whose server is not on `PATH` is skipped, and the second fails instead of skipping (and turns the suite on). The `image` job of `.github/workflows/coder.yml` runs it in the built image with the second |
| `ADAM_TEST_STDIO_SILENCE_SECS` | seconds of silence with stdin open in the framed-stdio tests (local, run pod); default 20 |
| `ADAM_TEST_REQUIRE_SYSTEMD=1` | `adam-confine`'s scope tests (`crates/adam-confine/tests/conformance.rs`) **fail** instead of skipping when the machine has no systemd user manager that delegates the memory, cpu and pids controllers (the `confine` job of `.github/workflows/ci.yml` sets it) |
| `ADAM_TEST_REQUIRE_MOUNTS=1` | `deploy/coder/tests/git-in-run-pod.sh` (git under a run pod's mounts) fails instead of skipping when it cannot make a mount namespace (CI sets it) |
| `ADAM_TEST_OPENAI_BASE_URL`, `ADAM_TEST_OPENAI_API_KEY`, `ADAM_TEST_OPENAI_MODEL` | a live model for `adam-model-openai/tests/live.rs` |
| `ADAM_TEST_OPENCODE=1` | the live OpenCode test of `adam-acp` (a library no binary uses now) |
| `ADAM_ENGINE_TEST_RESUME_CHILD` | set by `crates/adam-engine/tests/resume_process.rs` on the child process it starts (the store's directory); never set it yourself |
| `ADAM_ENGINE_TEST_RESUME_FIXED_ID` | set by the same test on the child it starts when both halves use the fixed worker id; never set it yourself |
| `ADAM_TRYBUILD=1` | the rustc-worded half of the `#[tool]` compile tests |
| `ADAM_UPDATE_GOLDEN=1` | regenerate the codegen golden of `adam-agent-fs` |

See [Testing](../guides/testing.md).
