# Run it locally

Everything runs on your machine with Docker Compose, against **mocks**: a scripted model, a fake GitHub and a
local git server. No credential is needed and every one in the file is a dummy. The result is deterministic: the
same task takes the same steps and ends in a pull request on the mock GitHub.

## Prerequisites

Docker with Compose v2. The first `--build` compiles the Rust workspace into an image, which takes several
minutes; later starts take seconds. To skip the build, set `CODER_IMAGE` to a published
`ghcr.io/vymalo/another-adam-rs/coder` tag (the one the chart uses is `image.tag` in
`deploy/coder/values.yaml`) and pass `--no-build`.

## Start it

```sh
docker compose --profile app up -d --build --wait   # postgres, the mocks, a git server, the coder, the general agent
docker compose up -d --wait                         # or only the databases and mocks, to run `cargo test`
docker compose down -v                              # stop and forget all state (volumes included)
```

| Service | Where | What |
|---|---|---|
| `coder` | `http://127.0.0.1:8080/` | the coder agent, bearer token `dev-token` |
| `agent` | `http://127.0.0.1:8084/` | the general agent (`adam-agent`), same image, same token |
| `postgres`, `mongodb` | `127.0.0.1:5432`, `:27017` | the databases |
| `mock-openai`, `mock-github`, `mock-github-mcp`, `git-server` | `:8081`, `:8082`, `:8085`, `:8083` | the mocks |

All services and mocks: [the local stack](../reference/dev-stack.md).

## Send the coder a task

```sh
curl -N http://127.0.0.1:8080/ \
  -H 'Authorization: Bearer dev-token' -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":"1","method":"SendStreamingMessage","params":{"message":{
        "messageId":"m1","role":"ROLE_USER","parts":[{"text":
        "In http://git-server:8080/local/sandbox.git (base branch main), add hello.txt containing hello."}]}}}'
```

You get an SSE stream of status updates, steps and artifacts, ending in `TASK_STATE_COMPLETED` with a
`pull_request` artifact. The coder's model is scripted, so the run goes: prepare a worktree, ask the mock
GitHub MCP for the branches, let OpenCode (also scripted) create `hello.txt`, run the repository's checks,
push a branch to the git server, open a pull request on the mock GitHub. The agent card is at
`http://127.0.0.1:8080/.well-known/agent-card.json`, public like `/healthz`.

Say "hi" to either agent and it greets you from its own instructions. The other scenarios (a scratch project,
a second repository, a repository created on request, the devcontainer, a GitHub App) are scripts:
[Testing](testing.md#end-to-end-scenarios).

## Change what an agent says

Both agents read their folder (instructions, card, skills, `mcp.json`) once, at startup, from
`ADAM_AGENT_DIR`. Compose mounts `bin/adam-coder/agent` for the coder and `dev/agents/assistant/agent` for the
general agent, read-only. Edit `instructions.md`, or copy the folder and set `CODER_AGENT_DIR` or
`AGENT_FOLDER`, then restart without a rebuild:

```sh
docker compose --profile app up -d coder        # the container is recreated and reads the folder again
```

The startup log has one `agent files` line (`source=folder`, the path, the digest). A folder with a mistake
stops the container with exit 78 and every finding as `path:line: error: ...`. The folder must be readable by
uid 10001 (`chmod -R a+rX`). A new agent is a folder and about twelve lines of `compose.yaml`
([Write an agent](write-an-agent.md)).

## Run a binary on the host

Start only the databases and mocks, then:

```sh
export DATABASE_URL=postgres://postgres:postgres@127.0.0.1:5432/adam_test
export MODEL_BASE_URL=http://127.0.0.1:8081/v1 MODEL_API_KEY=mock-api-key MODEL=mock-model
export GITHUB_API_URL=http://127.0.0.1:8082 GITHUB_TOKEN=dev-github-token
export A2A_BEARER_TOKENS=dev-token PUBLIC_URL=http://127.0.0.1:8080/
cargo run -p adam-coder
```

`MODEL=mock-model` gives canned text answers (no tool call, so no pull request); use `mock-coder` for the script.
`ROLE=control-plane` and `ROLE=worker` over the same `DATABASE_URL` (different `LISTEN_ADDR`) split the two
halves. All variables: [Environment](../reference/environment.md). Against a real model and GitHub, see the
live smoke test in the [coder README](../../bin/adam-coder/README.md#live-smoke-test-manual-not-run-in-ci).

How a *live* model behaves with these instructions is *unverified*: the mocks prove what the model is sent, not
what it says.

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| the container exits 78 | a configuration or agent-folder problem; the log names the variable or `path:line` |
| the container exits 69 | Postgres is not reachable yet or at all |
| `401` on every call | a missing or wrong `Authorization: Bearer dev-token` (no token means no server, by design) |
| the model mock answers `404 off_script` | a request left the scripted model's script, on purpose, so a run that wanders fails loudly; see [scripted models](../reference/dev-stack.md#scripted-models) |
| Podman scenarios fail on Ubuntu 24.04 | `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0` |
