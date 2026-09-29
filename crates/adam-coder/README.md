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
  fails instead of completing, whatever the model says.

Per-run bookkeeping (cycles, last check, pushed sha, pull request) lives in
`<WORKSPACE_ROOT>/coder/<run>.json` next to the worktree, written atomically.

### Retry safety

Each tool's side effect runs inside `LlmAgent`'s journaled `tool:<call id>`
step, and each is also idempotent by construction, so a call that dies before
its result is journaled (or a transient retry, which starts at a fresh journal
position) does not duplicate anything: `commit_all` is a no-op without changes,
pushing a commit the remote already has is a no-op, and
`CodeHost::open_pull_request` returns the open pull request of the same head.

## Configuration

Environment variables (`src/config.rs` is the reference; every problem is
reported at once at startup):

| Variable | Meaning | Default |
|---|---|---|
| `DATABASE_URL` | Postgres for the run store | required |
| `MODEL_BASE_URL`, `MODEL_API_KEY` | OpenAI-compatible gateway (with `/v1`) and its key | required (key may be empty) |
| `MODEL` | model alias of the agent | required |
| `OPENCODE_MODEL` | model alias OpenCode uses through the same gateway | `MODEL` |
| `GITHUB_TOKEN` | push and pull request token | required |
| `WORKSPACE_ROOT` | mirrors, worktrees, run notes | `/work` |
| `A2A_BEARER_TOKENS` | comma-separated accepted tokens (fail closed: none = no server) | required |
| `PUBLIC_URL` | where clients reach the JSON-RPC endpoint (agent card) | required |
| `LISTEN_ADDR` | bind address | `0.0.0.0:8080` |
| `WORKERS` | runs advanced concurrently | `4` |
| `MAX_CHECK_CYCLES` | failed `run_checks` before the agent must stop | `3` |
| `CHECK_TIMEOUT_SECS`, `CHECK_OUTPUT_TAIL_BYTES` | limits of one `run_checks` | `900`, `16384` |
| `GIT_AUTHOR_NAME`, `GIT_AUTHOR_EMAIL` | identity of the commits | `adam-coder` |
| `PR_DRAFT` | open pull requests as drafts | `false` |
| `OPENCODE_COMMAND` | the ACP program and arguments | `opencode acp` |

OpenCode's configuration is generated at startup into
`OPENCODE_CONFIG_CONTENT` (custom `@ai-sdk/openai-compatible` provider at
`MODEL_BASE_URL`, model `OPENCODE_MODEL`, key by reference `{env:MODEL_API_KEY}`,
never inlined) together with `OPENCODE_DISABLE_AUTOUPDATE=1`; see
`src/opencode.rs` for what was verified against the OpenCode sources. The
OpenCode child does not see `GITHUB_TOKEN`, `DATABASE_URL` or
`A2A_BEARER_TOKENS`, and the checks do not see those or `MODEL_API_KEY`.

SIGTERM stops accepting connections and lets in-flight steps finish and commit;
a step cut short by a hard kill is taken over by the next start when its lease
expires. Logs are JSON on stdout (`RUST_LOG` filters).

Deployment: `docker/coder/Dockerfile` and the chart in `deploy/coder/`.

## Tests

Everything is offline (`cargo test -p adam-coder`):

* `tests/e2e.rs`: a real A2A client and server, the scripted `MockModel`, a
  local bare git repository as the remote, the `adam-acp` fake agent as OpenCode
  and a wiremock GitHub. Covers the happy path (working, progress, checks,
  artifacts, completed, branch on the remote, PR request at the mock), the
  `input-required` round trip, red checks N times (failed, findings, no PR),
  the explicit-acceptance path, ownership, and three crash points (inside
  `commit_and_push`, after it was journaled, inside `open_pull_request`) with a
  second worker taking over: one commit, one push, one pull request.
* `tests/tools.rs`: each tool against real worktrees.
* unit tests: configuration, prompt, OpenCode config, shell execution (timeout
  kills the process group, output tail, cwd confinement, hidden secrets), run
  notes.

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
docker compose up -d postgres            # from the repository root
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

Record the pull request URL and the log excerpts in the pull request that lands
this change. (Not run by the author of this crate: no gateway or GitHub access
in the environment it was written in.)
