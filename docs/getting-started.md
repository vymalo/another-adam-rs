# Get started: a folder agent answers in minutes

One page, one success: the local stack up, a folder agent served, one message over A2A, and its answer. Every command
below is the one `dev/agent-e2e.sh` runs in CI, so a change that breaks this page breaks that script.

## Before you start

- Docker with Compose v2 (`docker compose version`), and `curl` with `jq`
- About 3 GB of disk for the image the first run builds
- Ports 8084 (the agent) and 5432 (Postgres) free on `127.0.0.1`

## 1. Start the stack

```sh
docker compose --profile app up -d --build --wait postgres mock-openai agent
```

`--build` builds the image the first time (it carries both `adam-agent` and `adam-coder`); `--wait` returns when the
agent answers. The agent serves the folder in `dev/agents/assistant/agent`, and its model is the scripted
`mock-assistant`: no key, no network, a fixed greeting.

## 2. Read the card

```sh
curl -s http://127.0.0.1:8084/.well-known/agent-card.json | jq -r '.name, .skills[].id'
```

You should see `Assistant` and `conversation`. The card is public; the calls are not.

## 3. Say hi

```sh
curl -sN http://127.0.0.1:8084/ \
  -H 'Authorization: Bearer dev-token' -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"SendStreamingMessage","params":{"message":{"messageId":"first-1","role":"ROLE_USER","parts":[{"text":"hi"}]}}}' \
  | sed -n 's/^data: //p' \
  | jq -r 'select(.result.statusUpdate.status.state == "TASK_STATE_COMPLETED") | [.. | .text? // empty] | join(" ")'
```

The answer is the greeting the folder's persona asks for: the agent's name and its one-sentence summary. A call without
the token is refused; the same message sent to `POST /` is the JSON-RPC binding.

## 4. Stop it

```sh
docker compose --profile app down
```

## Next

| Page | When |
|---|---|
| [Run it locally](guides/run-locally.md) | you want the coder and the general agent on your machine, and to know what each service is |
| [Try the agents](guides/try-the-agents.md) | you want to chat with the Assistant, give Adam a task, and watch the steps and the files |
| [Write your own agent](guides/write-an-agent.md) | the next folder, in Markdown, with your instructions and tools |
| [Run the coder](guides/deploy-the-coder.md) | a task to a verified pull request, on your own machine or a cluster |
| [Embed adam](guides/embed-adam.md) | an agent inside your own Rust program |
