#!/bin/sh
# End-to-end test of the coder's name, its greeting and its mounted agent folder against the
# compose mocks (adam-rs#55): "hi" gets a greeting, not a request for a task, and a change to
# the folder the coder reads at startup changes the answer without a rebuild.
#
#   dev/greeting-e2e.sh                 # all three steps
#   NO_RESTART=1 dev/greeting-e2e.sh    # steps 1 and 2 only (no `docker compose`)
#
# Start the stack first (the model `mock-coder` is the script in
# dev/wiremock/mock-openai/mappings/coder-script.json; its greeting is built from the first two
# lines of the coder's instructions, `Your name is <name>.` and `In one sentence: <summary>.`):
#
#   docker compose --profile app up -d --build --wait \
#     postgres mock-openai mock-github git-server coder
#
# Steps:
#   1. SendStreamingMessage "Hi": the task ends TASK_STATE_INPUT_REQUIRED (it waits for the
#      person), and the question it asks is a greeting that says the agent's name and the
#      one-sentence summary, both read here from the folder the stack mounts (bin/adam-coder/agent),
#      and asks which repository and what to change. No tool ran: there is no artifact. The greeting
#      was written as it arrived (`text-stream/v1`, which the script activates): at least two `reply`
#      chunks that add up to the question, and the question names the stream of the chunks. The
#      `reply` chunks are the answer's words, not a tool's artifact: they do not count as one.
#   2. A message to the same task names http://git-server:8080/local/sandbox.git: the run goes on
#      with the script and ends TASK_STATE_COMPLETED with a `branch` and a `pull_request` artifact.
#   3. (unless NO_RESTART=1) a copy of the folder with `display_name: Cody` is mounted in its place
#      (CODER_AGENT_DIR=<copy> docker compose --profile app up -d --no-build --wait coder), and
#      "Hi" gets "I'm Cody": the same image, a restart, no rebuild. Then the default folder is put
#      back (also when a check failed: a trap) and "Hi" gets the first name again. A step 3 without
#      `docker compose` on PATH is skipped with a line saying so.
# It prints one "ok" or "FAIL" line per check and exits 1 if any failed.
#
# Environment (defaults match compose.yaml on one machine):
#   CODER_URL     http://127.0.0.1:${CODER_PORT:-8080}
#   CODER_TOKEN   dev-token
#   AGENT_DIR     bin/adam-coder/agent   the folder the stack mounts; the name and the summary are read from it
#   TIMEOUT       300   seconds to wait for a task to stop
#   NO_RESTART    unset 1 = skip step 3
#
# Needs curl and jq (and, for step 3, docker compose and a coder image `--no-build` can use).
# Run from anywhere: the script moves to the repository root.
set -eu

cd "$(dirname "$0")/.."

coder=${CODER_URL:-http://127.0.0.1:${CODER_PORT:-8080}}
coder=${coder%/}
token=${CODER_TOKEN:-dev-token}
timeout=${TIMEOUT:-300}
agent_dir=${AGENT_DIR:-bin/adam-coder/agent}
repo_url=http://git-server:8080/local/sandbox.git
# The extension that makes the coder send its answer as it is written (its card lists it).
text_stream=https://agents.vymalo.com/a2a/extensions/text-stream/v1

fail=0
ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fail=1; }

tmp=$(mktemp -d)
copy=
restart_needed=0
original_dir=${CODER_AGENT_DIR-}

# Start the coder on the default folder again (the one the stack mounts without CODER_AGENT_DIR,
# or the one the caller named with it).
put_back() {
  if [ -n "$original_dir" ]; then
    CODER_AGENT_DIR=$original_dir docker compose --profile app up -d --no-build --wait coder >/dev/null 2>&1
  else
    env -u CODER_AGENT_DIR docker compose --profile app up -d --no-build --wait coder >/dev/null 2>&1
  fi
}

# Put the default folder back when step 3 changed it, whatever happens.
restore() {
  if [ "$restart_needed" = 1 ]; then
    restart_needed=0
    put_back || echo "could not restore the default agent folder: run: docker compose --profile app up -d coder"
  fi
  rm -rf "$tmp"
  if [ -n "$copy" ]; then rm -rf "$copy"; fi
}
trap restore EXIT

instructions=$agent_dir/instructions.md
if [ ! -f "$instructions" ]; then
  echo "FAIL $instructions does not exist (AGENT_DIR is the folder that holds instructions.md)"
  exit 1
fi
# The persona the folder gives: `display_name: <name>` under `vars`, and the line
# `In one sentence: <summary>.` that opens the body.
name=$(sed -n 's/^[[:space:]]*display_name:[[:space:]]*//p' "$instructions" | head -n 1)
summary=$(sed -n 's/^In one sentence: \(.*\)\.[[:space:]]*$/\1/p' "$instructions" | head -n 1)
if [ -z "$name" ] || [ -z "$summary" ]; then
  echo "FAIL $instructions has no 'display_name:' var or no 'In one sentence: <summary>.' line"
  exit 1
fi

n=0
# send <text> [task id] [context id] -> $tmp/events.jsonl, one JSON-RPC response per event.
send() {
  n=$((n + 1))
  message_id="greeting-e2e-$(date +%s)-$$-$n"
  rpc=$(jq -n --arg id "$message_id" --arg text "$1" --arg task "${2:-}" --arg ctx "${3:-}" '{
    jsonrpc: "2.0", id: "1", method: "SendStreamingMessage",
    params: {message: ({messageId: $id, role: "ROLE_USER", parts: [{text: $text}]}
      + (if $task != "" then {taskId: $task} else {} end)
      + (if $ctx != "" then {contextId: $ctx} else {} end))}}')
  stream=$tmp/stream.sse
  : > "$stream"
  curl_rc=0
  code=$(curl -sN --max-time "$timeout" -o "$stream" -w '%{http_code}' \
    -X POST "$coder/" \
    -H "Authorization: Bearer $token" -H 'Content-Type: application/json' \
    -H "A2A-Extensions: $text_stream" \
    -d "$rpc") || curl_rc=$?
  if [ "$curl_rc" = 28 ]; then
    bad "the task did not stop within ${timeout}s"
  elif [ "$curl_rc" != 0 ]; then
    bad "SendStreamingMessage: curl exit $curl_rc"
  elif [ "$code" != 200 ]; then
    bad "SendStreamingMessage: HTTP $code: $(head -c 300 "$stream")"
  fi
  sed -n 's/^data: //p' "$stream" | jq -c '.' > "$tmp/events.jsonl" 2>/dev/null || : > "$tmp/events.jsonl"
  rpc_error=$(jq -r 'select(.error) | .error | "\(.code): \(.message)"' "$tmp/events.jsonl" | head -n 1)
  if [ -n "$rpc_error" ]; then bad "the server answered with a JSON-RPC error: $rpc_error"; fi
}

last_state() {
  jq -r '(.result.statusUpdate.status.state // .result.task.status.state) // empty' "$tmp/events.jsonl" | tail -n 1
}
# The words of the update that reached `state` (the question, for TASK_STATE_INPUT_REQUIRED).
words_of() {
  jq -r --arg s "$1" 'select((.result.statusUpdate.status.state // .result.task.status.state) == $s) | [.. | .text? // empty] | join(" ")' "$tmp/events.jsonl" | tail -n 1
}
# The names of the artifacts the run made. The `reply` chunks are not among them: they are the
# words of the answer sent as they are written (`text-stream/v1`), not something a tool made.
artifact_names() {
  jq -r 'select(.result.artifactUpdate and .result.artifactUpdate.artifact.name != "reply") | .result.artifactUpdate.artifact.name' "$tmp/events.jsonl" | sort -u | tr '\n' ' '
}

# greets <name> <summary>: the last `send` was "Hi" and the answer is the greeting of that persona.
greets() {
  state=$(last_state)
  if [ "$state" = TASK_STATE_INPUT_REQUIRED ]; then ok "\"Hi\" ends TASK_STATE_INPUT_REQUIRED: the agent waits for the person"; else bad "\"Hi\" ended '${state:-none}', want TASK_STATE_INPUT_REQUIRED"; fi
  said=$(words_of TASK_STATE_INPUT_REQUIRED)
  case "$said" in
    *"I'm $1"*) ok "the greeting says the name: I'm $1" ;;
    *) bad "the greeting does not say \"I'm $1\": $said" ;;
  esac
  case "$said" in
    *"$2"*) ok "the greeting says what the agent does: $2" ;;
    *) bad "the greeting does not say \"$2\": $said" ;;
  esac
  case "$said" in
    *"Which repository"*) ok "the greeting asks which repository and what to change" ;;
    *) bad "the greeting does not ask which repository: $said" ;;
  esac
  names=$(artifact_names)
  if [ -z "$names" ]; then ok "no tool ran: no artifact"; else bad "a greeting produced artifacts: $names"; fi
  written_as_it_arrived "$said"
}

# written_as_it_arrived <the question>: the greeting came as `reply` chunks that add up to the
# question the task waits on, and that question names the stream of the chunks.
written_as_it_arrived() {
  chunks=$(jq -s '[.[] | select(.result.artifactUpdate.artifact.name == "reply") | .result.artifactUpdate]' "$tmp/events.jsonl")
  n_chunks=$(printf '%s' "$chunks" | jq 'length')
  if [ "$n_chunks" -ge 2 ]; then ok "the greeting arrived as $n_chunks chunks"; else bad "the greeting arrived as $n_chunks chunk(s), want at least 2"; fi
  joined=$(printf '%s' "$chunks" | jq -r 'map(.artifact.parts[0].text) | join("")')
  if [ "$joined" = "$1" ]; then ok "the chunks add up to the question"; else bad "the chunks say '$joined', the question says '$1'"; fi
  stream_id=$(printf '%s' "$chunks" | jq -r '.[0].artifact.artifactId // empty')
  said_id=$(jq -r --arg u "$text_stream" 'select(.result.statusUpdate.status.state == "TASK_STATE_INPUT_REQUIRED") | .result.statusUpdate.status.message.metadata[$u].streamId // empty' "$tmp/events.jsonl" | tail -n 1)
  if [ -n "$stream_id" ] && [ "$stream_id" = "$said_id" ]; then ok "the question names the stream of the chunks"; else bad "the stream of the chunks is '$stream_id', the question names '$said_id'"; fi
}

# --- 1. "Hi" gets a greeting ---------------------------------------------------------
echo "step 1: \"Hi\" gets a greeting from $name"
send "Hi"
greets "$name" "$summary"
task=$(jq -r '(.result.task.id // .result.statusUpdate.taskId // .result.artifactUpdate.taskId) // empty' "$tmp/events.jsonl" | head -n 1)
context=$(jq -r '(.result.task.contextId // .result.statusUpdate.contextId // .result.artifactUpdate.contextId) // empty' "$tmp/events.jsonl" | head -n 1)
if [ -n "$task" ]; then ok "the task is $task"; else bad "no task id in the stream"; fi

# --- 2. The person names a repository: the run goes on -----------------------------------
echo "step 2: the same task goes on when a repository is named"
if [ -n "$task" ]; then
  send "In $repo_url (base branch main), add hello.txt containing hello." "$task" "$context"
  state=$(last_state)
  if [ "$state" = TASK_STATE_COMPLETED ]; then
    ok "the task ended TASK_STATE_COMPLETED"
  else
    bad "the task ended '${state:-none}', want TASK_STATE_COMPLETED: $(words_of "$state" | head -c 400)"
  fi
  names=$(artifact_names)
  for want in branch pull_request; do
    case " $names" in
      *" $want "*) ok "the $want artifact is there" ;;
      *) bad "no $want artifact (artifacts: ${names:-none})" ;;
    esac
  done
else
  bad "step 2 needs the task of step 1"
fi

# --- 3. A changed folder changes the answer after a restart -----------------------------
if [ "${NO_RESTART:-}" = 1 ]; then
  echo "step 3: skipped (NO_RESTART=1)"
elif ! command -v docker >/dev/null 2>&1 || ! docker compose version >/dev/null 2>&1; then
  echo "step 3: SKIPPED, docker compose is not available here"
else
  echo "step 3: a copy of the folder with another display_name, mounted in its place"
  copy=$(mktemp -d)
  cp -R "$agent_dir"/. "$copy"/
  sed "s/^\([[:space:]]*display_name:\)[[:space:]]*.*/\1 Cody/" "$instructions" > "$copy/instructions.md"
  # The container runs as uid 10001: the copy must be readable by others.
  chmod -R a+rX "$copy"
  restart_needed=1
  if CODER_AGENT_DIR=$copy docker compose --profile app up -d --no-build --wait coder >/dev/null 2>&1; then
    ok "the coder restarted on the edited folder"
    send "Hi"
    greets Cody "$summary"
  else
    bad "the coder did not come back with CODER_AGENT_DIR=$copy"
  fi
  restart_needed=0
  restore_ok=0
  if put_back; then restore_ok=1; fi
  if [ "$restore_ok" = 1 ]; then
    ok "the coder restarted on the default folder"
    send "Hi"
    greets "$name" "$summary"
  else
    bad "the coder did not come back on the default folder"
    restart_needed=1
  fi
fi

if [ "$fail" -eq 0 ]; then echo "greeting e2e passed"; else echo "greeting e2e FAILED"; exit 1; fi
