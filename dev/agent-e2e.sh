#!/bin/sh
# End-to-end test of the general agent (`adam-agent`, service `agent`) against the compose mocks: "hi"
# is answered in role by the folder it serves, and a change to the folder it reads at startup changes the
# answer after a restart, without a rebuild.
#
#   dev/agent-e2e.sh                 # all steps
#   NO_RESTART=1 dev/agent-e2e.sh    # steps 1 and 2 only (no `docker compose`)
#
# Start the stack first (the model `mock-assistant` is the mapping in
# dev/wiremock/mock-openai/mappings/agent-script.json; it greets from the first two lines of the
# agent's instructions, `Your name is <name>.` and `In one sentence: <summary>.`):
#
#   docker compose --profile app up -d --build --wait postgres mock-openai agent
#
# Steps:
#   1. The agent card is public and is the folder's (the name and the `conversation` skill), and a call
#      without the token is refused.
#   2. SendStreamingMessage "hi": the task ends TASK_STATE_COMPLETED (a chat agent answers, it does not
#      wait), and the answer is a greeting that says the agent's name and its one-sentence summary, both
#      read here from the folder the stack mounts (dev/agents/assistant/agent). No tool ran: there is
#      no artifact.
#   3. (unless NO_RESTART=1) a copy of the folder with another name and summary is mounted in its place
#      (AGENT_FOLDER=<copy> docker compose --profile app up -d --no-build --wait agent), and the card and
#      "hi" say the new words: the same image, a restart, no rebuild. Then the default folder is put back
#      (also when a check failed: a trap) and "hi" gets the first name again. A step 3 without
#      `docker compose` on PATH is skipped with a line saying so.
# It prints one "ok" or "FAIL" line per check and exits 1 if any failed.
#
# Environment (defaults match compose.yaml on one machine):
#   AGENT_URL     http://127.0.0.1:${AGENT_PORT:-8084}
#   AGENT_TOKEN   dev-token
#   AGENT_DIR     dev/agents/assistant/agent   the folder the stack mounts; the name and the summary are read from it
#   TIMEOUT       120   seconds to wait for a task to stop
#   NO_RESTART    unset 1 = skip step 3
#
# Needs curl and jq (and, for step 3, docker compose and a coder image `--no-build` can use).
# Run from anywhere: the script moves to the repository root.
set -eu

cd "$(dirname "$0")/.."

agent=${AGENT_URL:-http://127.0.0.1:${AGENT_PORT:-8084}}
agent=${agent%/}
token=${AGENT_TOKEN:-dev-token}
timeout=${TIMEOUT:-120}
agent_dir=${AGENT_DIR:-dev/agents/assistant/agent}

fail=0
ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fail=1; }

tmp=$(mktemp -d)
copy=
restart_needed=0
original_dir=${AGENT_FOLDER-}

# Start the agent on the default folder again (the one the stack mounts without AGENT_FOLDER, or the
# one the caller named with it).
put_back() {
  if [ -n "$original_dir" ]; then
    AGENT_FOLDER=$original_dir docker compose --profile app up -d --no-build --wait agent >/dev/null 2>&1
  else
    env -u AGENT_FOLDER docker compose --profile app up -d --no-build --wait agent >/dev/null 2>&1
  fi
}

# Put the default folder back when step 3 changed it, whatever happens.
restore() {
  if [ "$restart_needed" = 1 ]; then
    restart_needed=0
    put_back || echo "could not restore the default agent folder: run: docker compose --profile app up -d agent"
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
# The persona the folder gives: `display_name: <name>` under `vars` and `name: <name>` under `card`, and the
# line `In one sentence: <summary>.` that opens the body.
name=$(sed -n 's/^[[:space:]]*display_name:[[:space:]]*//p' "$instructions" | head -n 1)
summary=$(sed -n 's/^In one sentence: \(.*\)\.[[:space:]]*$/\1/p' "$instructions" | head -n 1)
if [ -z "$name" ] || [ -z "$summary" ]; then
  echo "FAIL $instructions has no 'display_name:' var or no 'In one sentence: <summary>.' line"
  exit 1
fi

n=0
# send <text> -> $tmp/events.jsonl, one JSON-RPC response per event.
send() {
  n=$((n + 1))
  message_id="agent-e2e-$(date +%s)-$$-$n"
  rpc=$(jq -n --arg id "$message_id" --arg text "$1" '{
    jsonrpc: "2.0", id: "1", method: "SendStreamingMessage",
    params: {message: {messageId: $id, role: "ROLE_USER", parts: [{text: $text}]}}}')
  stream=$tmp/stream.sse
  : > "$stream"
  curl_rc=0
  code=$(curl -sN --max-time "$timeout" -o "$stream" -w '%{http_code}' \
    -X POST "$agent/" \
    -H "Authorization: Bearer $token" -H 'Content-Type: application/json' \
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
# The words of the update that reached `state`.
words_of() {
  jq -r --arg s "$1" 'select((.result.statusUpdate.status.state // .result.task.status.state) == $s) | [.. | .text? // empty] | join(" ")' "$tmp/events.jsonl" | tail -n 1
}
# The names of the artifacts the run made, as updates or in the opening `task` (what the run made
# before the subscription attached).
artifact_names() {
  jq -r '(.result.artifactUpdate.artifact // empty), (.result.task.artifacts[]?) | .name' "$tmp/events.jsonl" | sort -u | tr '\n' ' '
}

# card_is <name>: the public card names the agent and its skill.
card_is() {
  code=$(curl -s -o "$tmp/card.json" -w '%{http_code}' --max-time 30 "$agent/.well-known/agent-card.json" || true)
  got=$(jq -r '.name // empty' "$tmp/card.json" 2>/dev/null || true)
  if [ "$code" = 200 ] && [ "$got" = "$1" ]; then
    ok "the agent card is public and names the agent $1"
  else
    bad "agent card: status $code, name '${got:-?}', want $1"
  fi
}

# greets <name> <summary>: the last `send` was "hi" and the answer is the greeting of that persona.
greets() {
  state=$(last_state)
  if [ "$state" = TASK_STATE_COMPLETED ]; then ok "\"hi\" ends TASK_STATE_COMPLETED: a chat agent answers"; else bad "\"hi\" ended '${state:-none}', want TASK_STATE_COMPLETED"; fi
  said=$(words_of TASK_STATE_COMPLETED)
  case "$said" in
    *"I'm $1"*) ok "the answer says the name: I'm $1" ;;
    *) bad "the answer does not say \"I'm $1\": $said" ;;
  esac
  case "$said" in
    *"$2"*) ok "the answer says what the agent does: $2" ;;
    *) bad "the answer does not say \"$2\": $said" ;;
  esac
  names=$(artifact_names)
  if [ -z "$names" ]; then ok "no tool ran: no artifact"; else bad "a greeting produced artifacts: $names"; fi
}

# --- 1. The card is the folder's, and the token is required -----------------------------------
echo "step 1: the card of $name, and a call without a token"
card_is "$name"
if jq -e '.skills[] | select(.id == "conversation")' "$tmp/card.json" >/dev/null 2>&1; then
  ok "the agent card lists the conversation skill"
else
  bad "the agent card lacks the conversation skill"
fi
code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 30 -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"SendMessage","params":{}}' "$agent/" || true)
if [ "$code" = 401 ]; then ok "POST / without a token is 401"; else bad "POST / without a token is $code, want 401"; fi

# --- 2. "hi" is answered in role ---------------------------------------------------------------
echo "step 2: \"hi\" is answered in role by $name"
send "hi"
greets "$name" "$summary"

# --- 3. A changed folder changes the answer after a restart ------------------------------------
if [ "${NO_RESTART:-}" = 1 ]; then
  echo "step 3: skipped (NO_RESTART=1)"
elif ! command -v docker >/dev/null 2>&1 || ! docker compose version >/dev/null 2>&1; then
  echo "step 3: SKIPPED, docker compose is not available here"
else
  echo "step 3: a copy of the folder with another name and summary, mounted in its place"
  copy=$(mktemp -d)
  cp -R "$agent_dir"/. "$copy"/
  sed -e "s/^\([[:space:]]*display_name:\)[[:space:]]*.*/\1 Cody/" \
      -e "s/^\([[:space:]]*name:\)[[:space:]]*${name}[[:space:]]*\$/\1 Cody/" \
      -e "s/^In one sentence: .*/In one sentence: I only fix typos./" \
      "$instructions" > "$copy/instructions.md"
  # The container runs as uid 10001: the copy must be readable by others.
  chmod -R a+rX "$copy"
  restart_needed=1
  if AGENT_FOLDER=$copy docker compose --profile app up -d --no-build --wait agent >/dev/null 2>&1; then
    ok "the agent restarted on the edited folder"
    card_is Cody
    send "hello there"
    greets Cody "I only fix typos"
  else
    bad "the agent did not come back with AGENT_FOLDER=$copy"
  fi
  restart_needed=0
  if put_back; then
    ok "the agent restarted on the default folder"
    card_is "$name"
    send "hi"
    greets "$name" "$summary"
  else
    bad "the agent did not come back on the default folder"
    restart_needed=1
  fi
fi

if [ "$fail" -eq 0 ]; then echo "agent e2e passed"; else echo "agent e2e FAILED"; exit 1; fi
