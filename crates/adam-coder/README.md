# adam-coder

The coder agent: a coding task in, a verified pull request out, over A2A.

Given "in repo X, do Y" it

1. prepares a git worktree of X (`adam-workspace`),
2. has OpenCode make the change over ACP (`adam-acp`),
3. runs the project's own checks, at most `MAX_CHECK_CYCLES` failing cycles,
4. commits, pushes and opens a pull request, and
5. streams progress throughout and reports the pull request as an artifact.

It is durable (every model and tool step is journaled by `adam-runtime`, so a
restarted worker replays instead of repeating a side effect) and addressable
(an A2A 1.0 server from `adam-a2a`, backed by `adam-a2a-runtime`). One process
serves A2A and runs the workers; replicas share one Postgres.

```mermaid
sequenceDiagram
  participant O as Orchestrator (A2A client)
  participant S as adam-coder (A2A server)
  participant R as Runtime + workers
  participant M as Model (OpenAI-compatible)
  participant W as Worktree / OpenCode (ACP)
  participant G as Git remote + GitHub
  O->>S: SendStreamingMessage "in repo X, do Y"
  S->>R: start run (task id = run id)
  loop until the model stops
    R->>M: next turn
    M-->>R: tool calls
    R->>W: prepare_workspace / delegate_to_opencode / run_checks
    R-->>O: progress (status updates)
  end
  R->>G: commit_and_push, open_pull_request
  R-->>O: artifact (branch, pull_request) then completed
```

## Tools

| Tool | Does |
|---|---|
| `prepare_workspace { repo_url, base_branch }` | `Workspaces::prepare` with the run id as the run key, so a restart reuses the worktree |
| `delegate_to_opencode { instructions }` | spawns the ACP agent in the worktree (`ClientPolicy { fs_root: worktree }`), streams its updates as progress, returns its summary and the changed files |
| `run_checks { command, cwd? }` | `sh -lc <command>` in the worktree (a `cwd` must stay inside it), timeout kills the process group, output tail capped, secrets hidden from the child |
| `commit_and_push { message }` | `commit_all` + `push`; artifact `branch` |
| `open_pull_request { title, body, accept_red_checks? }` | `CodeHost::open_pull_request`; artifact `pull_request` (`url`, `number` as a string, `branch`, `repository`) |
| `ask_user { question }` | `ToolError::NeedsInput`: the run parks, A2A reports `input-required` with the question |

### The rules, in code

The system prompt (`src/instructions.md`, templated with `MAX_CHECK_CYCLES`)
tells the model the rules. The tools make them hold:

* **Cycle limit.** Every failed `run_checks` costs a cycle, counted per tool call
  id (a replay never counts twice). At `MAX_CHECK_CYCLES` the tool tells the
  model to stop and refuses to run anything; `commit_and_push` and
  `open_pull_request` refuse too. The run then ends `failed` with the findings
  (the output of the last failing check) and no pull request.
* **No PR on red checks.** `open_pull_request` opens a pull request only if the
  last check run passed **on exactly the code the pull request contains** (the
  tree of the pushed `HEAD`, compared with the tree the check ran on), and the
  branch is pushed. The single override is `accept_red_checks: true`, which the
  prompt reserves for explicit user consent obtained with `ask_user`; a pull
  request opened that way says so in its body. It never overrides an exhausted
  cycle budget (a deliberate hardening: after the limit the run must stop).
* **Completion policy.** A run that ends with red checks and no pull request
  fails instead of completing, whatever the model says. So does a run that ends
  without a pull request because GitHub or git rejected the credentials (the
  model cannot fix a bad token): the error names `GITHUB_TOKEN`.

Per-run bookkeeping (cycles, last check, pushed sha, pull request) lives in
`<WORKSPACE_ROOT>/coder/<run>.json` next to the worktree, written atomically.

### Retry safety

Each tool's side effect runs inside `LlmAgent`'s journaled `tool:<call id>`
step, and each is also idempotent by construction, so a call that dies before
its result is journaled (or a transient retry, which starts at a fresh journal
position) does not duplicate anything: `commit_all` is a no-op without changes,
pushing a commit the remote already has is a no-op, and
`CodeHost::open_pull_request` returns the open pull request of the same head.

### Cancel and rate limits

* **CancelTask** fails the run as `cancelled: ...` (A2A `canceled`) at once and
  fires the step's `CancelToken`. `delegate_to_opencode` then sends ACP
  `session/cancel`, gives OpenCode two seconds to end its turn, kills it **and
  its process group** (the commands it started) and waits until it is reaped
  before returning, so nothing outlives the tool. `commit_and_push` and
  `open_pull_request` refuse to act once the token has fired, so a later call of
  the same model turn cannot publish a cancelled run. The child is started in
  its own process group, so a terminal Ctrl-C does not reach it; the coder's
  own shutdown and cancel paths do.
* **429 with `Retry-After`** from the model gateway is carried to the runtime
  (`ModelError::RateLimited` -> `AgentError::Transient` with a `retry_after`):
  the retry waits at least that long, whatever the backoff says.

## Configuration

Environment variables (`src/config.rs` is the reference; every problem is
reported at once at startup):

| Variable | Meaning | Default |
|---|---|---|
| `DATABASE_URL` | Postgres for the run store | required |
| `MODEL_BASE_URL`, `MODEL_API_KEY` | OpenAI-compatible gateway (with `/v1`) and its key | required (key may be empty) |
| `MODEL` | model alias of the agent | required |
| `OPENCODE_MODEL` | model alias OpenCode uses through the same gateway | `MODEL` |
| `GITHUB_TOKEN` | push and pull request token; only ever sent to the `ALLOWED_REPO_HOSTS` | required |
| `ALLOWED_REPO_HOSTS` | comma-separated hosts (`name` for any port, or `name:port`) repositories may live on; the token is scoped to them | `github.com` |
| `GITHUB_API_URL` | GitHub REST API root (GitHub Enterprise: `https://<host>/api/v3`; tests and `compose.yaml`: `mock-github`) | `https://api.github.com` |
| `ALLOW_LOCAL_REPOS` | also accept local paths, `file://` and plain `http://` repositories. **Development and tests only** | `false` |
| `WORKSPACE_ROOT` | mirrors, worktrees, run notes | `/work` |
| `A2A_BEARER_TOKENS` | comma-separated accepted tokens (fail closed: none = no server) | required |
| `PUBLIC_URL` | where clients reach the JSON-RPC endpoint (agent card) | required |
| `LISTEN_ADDR` | bind address | `0.0.0.0:8080` |
| `WORKERS` | runs advanced concurrently | `4` |
| `MAX_CHECK_CYCLES` | failed `run_checks` before the agent must stop | `3` |
| `CHECK_TIMEOUT_SECS`, `CHECK_OUTPUT_TAIL_BYTES` | limits of one `run_checks` | `900`, `16384` |
| `GIT_AUTHOR_NAME`, `GIT_AUTHOR_EMAIL` | identity of the commits | `adam-coder`, `adam-coder@users.noreply.github.com` |
| `PR_DRAFT` | open pull requests as drafts | `false` |
| `OPENCODE_COMMAND` | the ACP program and arguments | `opencode acp` |

OpenCode's configuration is generated at startup into
`OPENCODE_CONFIG_CONTENT` (custom `@ai-sdk/openai-compatible` provider at
`MODEL_BASE_URL`, model `OPENCODE_MODEL`, key by reference `{env:MODEL_API_KEY}`,
never inlined) together with `OPENCODE_DISABLE_AUTOUPDATE=1`; see
`src/opencode.rs` for what was verified against the OpenCode sources. The
OpenCode child does not see `GITHUB_TOKEN`, `DATABASE_URL` or
`A2A_BEARER_TOKENS`, and the checks do not see those or `MODEL_API_KEY`.

### Which repositories, and where the token goes

The repository URL comes from the model, which took it from the user, so it
is treated as hostile input. `GITHUB_TOKEN` is bound to `ALLOWED_REPO_HOSTS`
twice over:

1. `Workspaces::allow_hosts` refuses any other host before a process is
   spawned, a request is made or a credential is asked for (the model gets the
   reason as a tool error). URLs with embedded credentials, ssh/scp forms, and
   anything that is not `https://<host>/<owner>/<repo>` are refused by the
   parser, and git is handed the URL rebuilt from the parsed parts, never the
   raw string.
2. The credentials are a `ScopedToken` for the same hosts, which refuses
   every other host even if a caller forgot the check.

Local paths, `file://` and plain `http://` are refused unless
`ALLOW_LOCAL_REPOS=true`, which exists for development and tests; local
remotes never receive the token.

### Secrets in output

Text from things this process does not control (OpenCode's stderr tail in an
"ACP agent exited" error, a check's output, a provider's error body) reaches
clients as run errors, events and tool results. A `Redactor` built from the
configuration replaces the *values* of `MODEL_API_KEY`, `GITHUB_TOKEN`, every
`A2A_BEARER_TOKENS` entry and the `DATABASE_URL` password (and their Base64
forms) with `[redacted]` in tool results and errors, in OpenCode's and the
checks' progress lines, in the checks' findings, in the agent's final
failure message, and in the process's own `adam-coder failed` log line.
A failed step's error crosses one boundary (`boundary_error` in
`src/agent.rs`): its whole cause chain is flattened into the message, scrubbed,
and cut to `MAX_FAILURE_TEXT` (2048 bytes, ` [truncated]` appended) *after*
scrubbing, so a secret on the cut cannot leave its front half. The retry hint
survives; the `source` does not. It is exact-value replacement, not a detector: a secret that
was transformed (hashed, split) is not found, and values shorter than 4
characters are not registered.

SIGTERM stops accepting connections and lets in-flight steps finish and commit;
a step cut short by a hard kill is taken over by the next start when its lease
expires. Logs are JSON on stdout (`RUST_LOG` filters).

Deployment: `docker/coder/Dockerfile` and the chart in `deploy/coder/`.

## Errors

The library errors it composes are classified (see
[`adam-error`](../adam-error/README.md)); this crate adds `ConfigError`
(`Invalid`: the same environment never works; it lists every problem and never
a secret) and `StoppedUnexpectedly` (`Internal`: the server or the workers
stopped while still needed).

A failure ends the process with one structured log line, `adam-coder failed`
(JSON on stdout, fields `error`, the whole scrubbed cause chain, and `code`),
and nothing on stderr. The exit code (`src/exit.rs`, `exit_code`) comes from
walking the `anyhow` chain from the outside in and taking the first match:

| Exit code | Meaning | Root cause |
|---|---|---|
| 0 | clean shutdown after SIGTERM or Ctrl-C | not an error |
| 78 (`EX_CONFIG`) | configuration; do not restart | `ConfigError`, or an `Invalid` `StoreError`, `OpenAiConfigError`, `WorkspaceError` or `RuntimeError` |
| 69 (`EX_UNAVAILABLE`) | a dependency is unreachable; restart later | a `Transient`, `RateLimited` or `Conflict` one of those, such as Postgres at boot |
| 71 (`EX_OSERR`) | the OS refused something | an `io::Error` with no typed error above it: a listener that cannot bind |
| 70 (`EX_SOFTWARE`) | internal | `StoppedUnexpectedly`, a panicked task, or a `Corrupt` or `Internal` typed error (including `OpenAiConfigError::Client`) |
| 1 | anything else | for example `NotFound`, `Rejected`, `Unauthenticated` (a bad `GITHUB_TOKEN`) or an untyped error |

The typed errors it looks for are `StoreError`, `OpenAiConfigError`,
`WorkspaceError`, `RuntimeError` and `StoppedUnexpectedly`. Because the walk goes
outside in, an unreachable Postgres is 69 although an `io::Error` is at the
bottom of its chain. The values are BSD `sysexits.h`'s, *unverified* (from
memory).

At run time, tool failures are flattened once where they cross to the model:
a `WorkspaceError` or `AcpError` becomes `ToolError::Transient` when
`is_retryable()` and `ToolError::Permanent` otherwise, with the chain printed
once (`adam_error::report`) and then scrubbed.

## Tests

Everything is offline (`cargo test -p adam-coder`; with
`ADAM_TEST_POSTGRES_URL` set the Postgres variants run too, each case in a
database of its own, so the role needs `CREATEDB`):

* `tests/e2e.rs`: a real A2A client and server, the scripted `MockModel`, a
  local bare git repository as the remote, the `adam-acp` fake agent as OpenCode
  and a wiremock GitHub. **Every case runs once per store** (`memory::*`, and
  `postgres::*` when the variable is set): the happy path (working, progress,
  checks, artifacts, completed, branch on the remote, PR request at the mock),
  the `input-required` round trip, red checks N times (failed, findings, no PR),
  the explicit-acceptance path, ownership, the wrong bearer token (401 at the
  coder's own router; card and `/healthz` open), four crash points (inside
  `commit_and_push`, after it was journaled, inside `open_pull_request`, inside
  `delegate_to_opencode`) with a second worker taking over: one commit, one
  push, one pull request, the same worktree; OpenCode crashing on every attempt
  (run fails after the retry budget, with the child's stderr) and once
  (retried, completes); two concurrent tasks on one repository (two branches,
  two pull requests); a GitHub 401 (run fails and names `GITHUB_TOKEN`).
* `tests/binary.rs`: the `adam-coder` binary as a process. All problems of a
  bad configuration reported together with exit 78; Postgres unreachable at boot
  (a clear "connecting to Postgres: ..." chain in exactly one `adam-coder failed`
  line, exit 69, nothing on stderr, no password, no panic; sqlx retries the
  connection for its 30 s acquire timeout first); with Postgres: the card and
  `/healthz`, a clean exit 0 on SIGTERM, and
  SIGTERM in the middle of OpenCode's turn (the process waits for the step,
  commits it, exits 0; a second process over the same database and workspace
  finishes the run with one commit, one push and one pull request). The last
  one runs the whole binary against a wiremock model (`/chat/completions`) and
  a wiremock GitHub reached through `GITHUB_API_URL`, under the production
  repository policy.
* `tests/tools.rs`: each tool against real worktrees, including the hostile
  `repo_url` shapes against the production repository policy.
* `adam-workspace/tests/workspace.rs`: the host allowlist, local paths, scoped
  tokens, and a wiremock "evil" git host that must never be contacted.
* unit tests: configuration, prompt, OpenCode config, shell execution (timeout
  kills the process group, output tail, cwd confinement, hidden secrets), run
  notes, the exit code of each root cause (`src/exit.rs`), and the scrubbing
  and bounding of failure text (`src/agent.rs`, `src/redact.rs`).

The fake agent binary is built by the tests themselves (`CARGO_BIN_EXE_*`
exists only inside `adam-acp`): `tests/common/mod.rs` runs
`cargo build -p adam-acp --bin adam-acp-fake-agent` with the same profile and
target directory as the running test executable (derived from
`current_exe()`), through `$CARGO`.

## Live smoke test (manual, not run in CI)

Proves the real chain: gateway, OpenCode and GitHub. Needs a sandbox repository
you can push branches to and open pull requests in (for example
`you/adam-coder-sandbox`, with a `README.md` and a trivial check such as a
`justfile` with `test:` or a `cargo test`), a GitHub token with `contents` and
`pull requests` write on it, an OpenAI-compatible gateway with a model that can
call tools, and `opencode` on `PATH`.

```sh
docker compose up -d --wait postgres     # from the repository root
export DATABASE_URL=postgres://postgres:postgres@localhost:5432/adam_test
export MODEL_BASE_URL=https://your-gateway.example/v1
export MODEL_API_KEY=...
export MODEL=your-model-alias
export GITHUB_TOKEN=github_pat_...
export A2A_BEARER_TOKENS=dev-token
export PUBLIC_URL=http://127.0.0.1:8080/
export WORKSPACE_ROOT=$(mktemp -d)
cargo run -p adam-coder                  # JSON logs on stdout
```

In another terminal (the wire shape is A2A 1.0 JSON-RPC over SSE):

```sh
curl -N http://127.0.0.1:8080/ \
  -H 'Authorization: Bearer dev-token' -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":"1","method":"SendStreamingMessage","params":{"message":{
        "messageId":"m1","role":"ROLE_USER","parts":[{"text":
        "In https://github.com/you/adam-coder-sandbox (base branch main), add a file hello.txt containing hello. Run the repository checks before opening a pull request."}]}}}'
```

Expected: a stream of status updates whose messages include `opencode: ...`
lines and `running checks: ...`, then artifacts `branch` and `pull_request`,
then `TASK_STATE_COMPLETED`. Verify:

* the pull request URL from the artifact opens on GitHub, from a branch
  `agent/<run id prefix>` with one commit, and its body has a summary and a
  verification section;
* `git ls-remote` on the sandbox shows the branch;
* the logs never contain the tokens;
* kill the process mid-run (`kill -9`), start it again: the run continues, and
  no second pull request appears;
* re-run with a task whose check cannot pass (`... and make `false` pass`) and
  `MAX_CHECK_CYCLES=2`: the task ends `TASK_STATE_FAILED` with the findings and
  opens nothing.

For a run without a real gateway or GitHub, `compose.yaml` provides a mock
model, a mock GitHub API and a local git remote; see "Local development" in the
repository README. The models are scripted: a task ends in a branch on the git
remote and one pull request on the mock GitHub, and `dev/coder-e2e.sh` (run by
`.github/workflows/coder.yml` on the image it builds) checks exactly that.

Record the pull request URL and the log excerpts in the pull request that lands
this change. (Not run by the author of this crate: no gateway or GitHub access
in the environment it was written in.)
