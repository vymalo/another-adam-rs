#!/bin/sh
# End-to-end test of the painter folder (dev/agents/painter/agent) against the compose mocks: the general
# agent (`adam-agent`, service `agent`) serves a folder whose `mcp.json` names an image MCP server
# (`mock-image-mcp`, WireMock), on the scripted model `mock-painter`.
#
#   dev/painter-e2e.sh                 # all steps
#   NO_RESTART=1 dev/painter-e2e.sh    # the agent already serves the painter on `mock-painter`
#
# Start the stack first (the model is dev/wiremock/mock-openai/mappings/painter-script.json, the image server
# dev/wiremock/mock-image-mcp/mappings/mcp.json; no repository or GitHub is needed):
#
#   docker compose --profile app up -d --build --wait postgres mock-openai mock-image-mcp agent
#
# Steps:
#   0. (unless NO_RESTART=1) the painter folder is mounted in place of the default folder, with the model
#      `mock-painter` and MCP_ALLOW_INSECURE (the image server is plain http on the compose network):
#      AGENT_FOLDER=<copy> AGENT_MODEL=mock-painter AGENT_MCP_ALLOW_INSECURE=true docker compose --profile app
#      up -d --no-build --wait agent. The default folder and model are put back at the end, also when a
#      check failed (a trap).
#   1. The card is the painter's and lists the commands extension with the image server's prompt as the
#      command `images__scene`, with the hint `<subject>`.
#   2. SendStreamingMessage "[mock:paint] a red dot": the scripted model draws only if the image server's
#      instructions are in its prompt (ADR 0044 slice 14), so a task that ends TASK_STATE_COMPLETED with a
#      file artifact (one `image/png` part, named draw-<hash>.png) and words that name that file proves the
#      instructions reached the model, the call went to the server and its image was shared as a file.
#   3. SendStreamingMessage "/images__scene a red dot": the prompt of the image server is expanded when the
#      message arrives (`prompts/get`, the subject filled in), so the same picture is made; without the
#      expansion the message would be plain text and the model would not draw.
# It prints one "ok" or "FAIL" line per check and exits 1 if any failed.
#
# Environment (defaults match compose.yaml on one machine):
#   AGENT_URL     http://127.0.0.1:${AGENT_PORT:-8084}
#   AGENT_TOKEN   dev-token
#   PAINTER_DIR   dev/agents/painter/agent   the folder to serve
#   FILE_NAME     draw-b1ff9c8e.png          the name the shared picture has (the first 8 hex digits of its SHA-256)
#   TIMEOUT       120   seconds to wait for a task to stop
#   NO_RESTART    unset 1 = skip step 0 (no `docker compose`)
#
# Needs curl and jq (and, for step 0, docker compose and a coder image `--no-build` can use).
# Run from anywhere: the script moves to the repository root.
set -eu

cd "$(dirname "$0")/.."

agent=${AGENT_URL:-http://127.0.0.1:${AGENT_PORT:-8084}}
agent=${agent%/}
token=${AGENT_TOKEN:-dev-token}
timeout=${TIMEOUT:-120}
painter_dir=${PAINTER_DIR:-dev/agents/painter/agent}
file_name=${FILE_NAME:-draw-b1ff9c8e.png}
commands_uri=https://agents.vymalo.com/a2a/extensions/commands/v1

if [ ! -f "$painter_dir/instructions.md" ]; then
  echo "FAIL $painter_dir/instructions.md does not exist"
  exit 1
fi

fail=0
ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fail=1; }

tmp=$(mktemp -d)
copy=
restart_needed=0
original_dir=${AGENT_FOLDER-}
original_model=${AGENT_MODEL-}
original_insecure=${AGENT_MCP_ALLOW_INSECURE-}

# Start the agent on the folder and model the stack mounts without our overrides again (or on the ones
# the caller named in AGENT_FOLDER, AGENT_MODEL and AGENT_MCP_ALLOW_INSECURE).
put_back() {
  (
    if [ -n "$original_dir" ]; then export AGENT_FOLDER="$original_dir"; else unset AGENT_FOLDER; fi
    if [ -n "$original_model" ]; then export AGENT_MODEL="$original_model"; else unset AGENT_MODEL; fi
    if [ -n "$original_insecure" ]; then export AGENT_MCP_ALLOW_INSECURE="$original_insecure"; else unset AGENT_MCP_ALLOW_INSECURE; fi
    docker compose --profile app up -d --no-build --wait agent >/dev/null 2>&1
  )
}

restore() {
  if [ "$restart_needed" = 1 ]; then
    restart_needed=0
    put_back || echo "could not restore the default agent folder: run: docker compose --profile app up -d agent"
  fi
  rm -rf "$tmp"
  if [ -n "$copy" ]; then rm -rf "$copy"; fi
}
trap restore EXIT

n=0
# send <text>: SendStreamingMessage on a context of its own -> $tmp/events.jsonl, one JSON-RPC response per event.
send() {
  n=$((n + 1))
  body=$(jq -n --arg id "painter-e2e-$(date +%s)-$$-$n" --arg ctx "painter-ctx-$(date +%s)-$$-$n" --arg text "$1" '{
    jsonrpc: "2.0", id: "1", method: "SendStreamingMessage",
    params: {message: {messageId: $id, contextId: $ctx, role: "ROLE_USER", parts: [{text: $text}]}}}')
  stream=$tmp/stream.sse
  : > "$stream"
  curl_rc=0
  code=$(curl -sN --max-time "$timeout" -o "$stream" -w '%{http_code}' \
    -X POST "$agent/" \
    -H "Authorization: Bearer $token" -H 'Content-Type: application/json' \
    -d "$body") || curl_rc=$?
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
# The image parts of the artifacts the stream delivered (as updates, and in the opening `task`), one JSON object a line.
image_parts() {
  jq -c '(.result.artifactUpdate.artifact // empty), (.result.task.artifacts[]?) | .parts[]? | select(.mediaType == "image/png")' \
    "$tmp/events.jsonl"
}

# Check what a task that drew the picture looks like.
check_picture() {
  state=$(last_state)
  if [ "$state" = TASK_STATE_COMPLETED ]; then ok "the task ended TASK_STATE_COMPLETED"; else bad "the task ended '${state:-none}', want TASK_STATE_COMPLETED"; fi
  names=$(image_parts | jq -r '.filename // empty' | sort -u)
  if [ "$names" = "$file_name" ]; then ok "one image/png file was shared, named $file_name"; else bad "the shared image files are '${names:-none}', want $file_name"; fi
  if image_parts | jq -e '.raw // .url // empty' >/dev/null 2>&1; then ok "the image part carries the bytes"; else bad "the image part carries no bytes"; fi
  said=$(words_of TASK_STATE_COMPLETED)
  case "$said" in
    *"$file_name"*) ok "the words name the file" ;;
    *) bad "the words do not name $file_name: $said" ;;
  esac
}

# --- 0. The agent serves the painter on the scripted model -------------------------------------
if [ "${NO_RESTART:-}" = 1 ]; then
  echo "step 0: skipped (NO_RESTART=1): the agent serves the painter on mock-painter"
elif ! command -v docker >/dev/null 2>&1 || ! docker compose version >/dev/null 2>&1; then
  echo "FAIL step 0: docker compose is not available here (NO_RESTART=1 skips it when the agent already serves the painter)"
  exit 1
else
  echo "step 0: the painter folder and the model mock-painter, in place of the default"
  copy=$(mktemp -d)
  cp -R "$painter_dir"/. "$copy"/
  # The container runs as uid 10001: the copy must be readable by others.
  chmod -R a+rX "$copy"
  restart_needed=1
  if docker compose --profile app up -d --no-build --wait mock-image-mcp >/dev/null 2>&1 \
    && AGENT_FOLDER=$copy AGENT_MODEL=mock-painter AGENT_MCP_ALLOW_INSECURE=true docker compose --profile app up -d --no-build --wait agent >/dev/null 2>&1; then
    ok "the agent restarted on the painter folder"
  else
    bad "the agent did not come back with AGENT_FOLDER=$copy AGENT_MODEL=mock-painter AGENT_MCP_ALLOW_INSECURE=true"
  fi
fi

# --- 1. The card ------------------------------------------------------------------------------------
echo "step 1: the card"
code=$(curl -s -o "$tmp/card.json" -w '%{http_code}' --max-time 30 "$agent/.well-known/agent-card.json" || true)
got=$(jq -r '.name // empty' "$tmp/card.json" 2>/dev/null || true)
if [ "$code" = 200 ] && [ "$got" = Painter ]; then ok "the agent card names the painter"; else bad "agent card: status $code, name '${got:-?}', want Painter"; fi
hint=$(jq -r --arg u "$commands_uri" '.capabilities.extensions[]? | select(.uri == $u) | .params.commands[]? | select(.name == "images__scene") | .argumentHint // empty' "$tmp/card.json" 2>/dev/null || true)
if [ "$hint" = "<subject>" ]; then ok "the card lists the prompt of the image server as the command images__scene (<subject>)"; else bad "the card does not list images__scene with the hint <subject> (hint '${hint:-none}')"; fi

# --- 2. The picture, drawn because the server's instructions reached the model -----------------
echo "step 2: [mock:paint] a red dot"
send "[mock:paint] a red dot"
check_picture

# --- 3. The same picture from the prompt of the image server, as a command ----------------------
echo "step 3: /images__scene a red dot"
send "/images__scene a red dot"
check_picture

# --- put the default folder back ---------------------------------------------------------------
if [ "$restart_needed" = 1 ]; then
  restart_needed=0
  if put_back; then ok "the agent restarted on the default folder and model"; else bad "the agent did not come back on the default folder"; restart_needed=1; fi
fi

if [ "$fail" -eq 0 ]; then echo "painter e2e passed"; else echo "painter e2e FAILED"; exit 1; fi
