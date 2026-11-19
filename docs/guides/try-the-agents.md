# Try the agents

Talk to every adam agent on your own machine: the chat-only Assistant, Adam the coder, an agent with tools
from an MCP server, and one you write yourself. First on the scripted mocks (no credential), then on a real
model. Everything runs in Docker; nothing leaves the machine except, on a real model, the model calls.

You need Docker with Compose v2, `curl` and `jq`. The first build compiles the workspace: allow several minutes.

## 1. Start the stack

```sh
docker compose --profile app up -d --build --wait
```

| Agent | Binary | Where | What it is |
|---|---|---|---|
| **Assistant** | `adam-agent` serving [`dev/agents/assistant`](../../dev/agents/assistant/agent/instructions.md) | `:8084` | chat only: answers, asks when it needs to know more, no tools but the screen's |
| **Adam** | `adam-coder` | `:8080` | answers, and takes a coding task to a checked pull request (here on the local git server and the mock GitHub) |

Every agent publishes its card at `/.well-known/agent-card.json` and a Swagger UI at `/docs` (press
**Authorize**, paste `dev-token`). The token is the stack's dummy; never reuse it.

## 2. Chat with the Assistant

```sh
dev/chat.sh                    # the Assistant on :8084; /new for a new conversation, /quit to leave
```

Type `hi`. On the mocks the model is scripted: it greets with the name and summary the folder gives, and
little else. The real conversation starts at [step 6](#6-on-a-real-model).

## 3. Give Adam a coding task

```sh
dev/chat.sh http://127.0.0.1:8080
```

```text
you: In http://git-server:8080/local/sandbox.git (base branch main), add hello.txt containing hello.
```

The lines starting with `·` are Adam's steps (files written, checks run). The task ends with a `branch` and
a `pull_request` artifact: the branch is on the local git server (`git clone http://127.0.0.1:8083/local/sandbox.git`),
the pull request on the mock GitHub. Say `hi` instead and Adam greets you and asks what to do.

## 4. An agent with tools: the Painter

An agent is a folder. The painter's [`mcp.json`](../../dev/agents/painter/agent/mcp.json) gives it an
image server (a mock here):

```sh
AGENT_FOLDER=./dev/agents/painter/agent AGENT_MODEL=mock-painter AGENT_MCP_ALLOW_INSECURE=true \
  docker compose --profile app up -d --wait agent
dev/chat.sh http://127.0.0.1:8084 "[mock:paint] a red dot"
```

The task ends with a picture shared as a file artifact. `docker compose --profile app up -d agent` (without the
variables) puts the Assistant back.

## 5. Write your own agent

```sh
cp -R dev/agents/assistant/agent /tmp/my-agent && chmod -R a+rX /tmp/my-agent
$EDITOR /tmp/my-agent/instructions.md      # change display_name, card.name, the "In one sentence" line, the rules
AGENT_FOLDER=/tmp/my-agent docker compose --profile app up -d --wait agent
dev/chat.sh http://127.0.0.1:8084 hi
```

A restart, no rebuild: the folder is read when the process starts. Skills, subagents, commands and MCP servers
are more files in the same folder: [Write an agent](write-an-agent.md), [the agent files](../reference/agent-files.md).

## 6. On a real model

Point both agents at any OpenAI-compatible endpoint with a model that can call tools. The key comes from your
environment; never write it in a file of this repository.

```sh
export MODEL_BASE_URL=https://your-gateway.example/v1 MODEL=your-model
export MODEL_API_KEY=...                   # read from your shell, a password manager, ...
docker compose -f compose.yaml -f dev/compose.real-model.yaml --profile app up -d --wait
dev/chat.sh                                # now a real conversation
```

**An endpoint that takes tokens from a sign-in** instead of a key (an OAuth 2.0 issuer with the device grant): set
`MODEL_AUTH=device` and the issuer instead of the key. Each agent asks for its own code; open the link in its log
line, sign in and allow it. The token is renewed by itself; a restart asks for a new code.

```sh
export MODEL_BASE_URL=https://your-gateway.example/v1 MODEL=your-model
export MODEL_AUTH=device MODEL_OAUTH_ISSUER=https://your-issuer.example MODEL_OAUTH_CLIENT_ID=your-client
docker compose -f compose.yaml -f dev/compose.real-model.yaml --profile app up -d --wait
docker compose logs agent coder | grep "sign in"   # one code and link per agent
```

[`dev/compose.real-model.yaml`](../../dev/compose.real-model.yaml) changes only the model: Adam still pushes to
the local git server and opens pull requests on the mock GitHub, so try real tasks on `local/sandbox.git` or ask
it to start a new project (it creates repositories for the owner `scratch` after asking). The Painter keeps its
mock image server. Things worth trying with Adam: a function with a test, a fix to the sandbox's failing check, a
follow-up on the same pull request ("now rename it"), a question with options (it asks with a form, which the
terminal shows as text).

Leaving the override out (`docker compose --profile app up -d --wait`) goes back to the mocks.

## 7. Stop

```sh
docker compose --profile app down          # keep the database and the git server's repositories
docker compose --profile app down -v       # forget everything
```

## When something goes wrong

| You see | Why | Fix |
|---|---|---|
| `no agent card at ...` | the service is not up, or still starting | `docker compose ps`, `docker compose logs agent` (or `coder`) |
| `required variable MODEL ... is missing a value` | the real-model override without its variables | export `MODEL_BASE_URL`, `MODEL`, and a key or `MODEL_AUTH=device` with its issuer |
| every call fails with `Auth` after a sign-in | the code was denied or expired, or the issuer refused a renewal | restart the agent for a new code: `docker compose --profile app up -d --force-recreate agent` |
| the task fails at once on a real model | wrong endpoint, key or model name | `docker compose logs agent` (or `coder`) says which call failed and why |
| the task fails with `404 off_script` | the scripted model got a message it has no script for | expected on the mocks: free conversation needs a real model |

More: [Run it locally](run-locally.md#troubleshooting), [the local stack](../reference/dev-stack.md),
[the environment variables](../reference/environment.md).
