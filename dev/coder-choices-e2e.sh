#!/bin/sh
# End-to-end test of the coder asking with Choices, against the compose mocks: a task that carries
# `[mock:choices]` and the screen's UI catalog in its metadata makes the coder ask three questions at once
# as one form (an A2UI surface beside the question, under the screen's own catalogId); the person answers
# with one A2UI action; the coder reads the answers and goes on (its next words say what was chosen).
#
#   dev/coder-choices-e2e.sh
#
# Start the stack first (the model `mock-coder` is dev/wiremock/mock-openai/mappings/coder-script.json and
# coder-choices.json; no repository, GitHub or OpenCode is needed for this scenario):
#
#   docker compose --profile app up -d --build --wait postgres mock-openai mock-github git-server coder
#
# Steps:
#   1. The card lists the extensions that make this work: A2UI v0.9.1 (taking the catalog inline),
#      ui-catalog/v1 and thread-tools/v1.
#   2. SendStreamingMessage "[mock:choices] set up the project" with the catalog in the message's metadata
#      (what the orchestrator sends: ui-catalog/v1 {version, digest, inline}, and the catalog in the A2UI
#      capabilities): the task ends TASK_STATE_INPUT_REQUIRED, the status message is the question as text and
#      an `application/a2ui+json` data part: createSurface under the screen's catalogId, and one Choices
#      with the three questions (db, auth, deploy).
#   3. A follow-up on the same task with one A2UI action (answers db=pg, auth=keycloak, deploy=compose): the
#      coder's next words are "Going with Postgres, Keycloak and Compose." (the mock answers that only when the
#      tool result it was given holds `db: pg`, which is how the answers read to the model).
#   4. The same task in another conversation, where the message names a catalog this coder has never seen
#      (a digest it does not hold) and carries no way to read it (no inline catalog, no thread-tools grant):
#      the coder cannot draw the form, so the question carries the options as text and no A2UI part (the
#      tools degrade, the run goes on). A catalog the coder has already read is held by its digest, so step 4
#      must name another one.
# It prints one "ok" or "FAIL" line per check and exits 1 if any failed.
#
# Environment (defaults match compose.yaml on one machine):
#   CODER_URL     http://127.0.0.1:${CODER_PORT:-8080}
#   CODER_TOKEN   dev-token
#   CATALOG_FILE  crates/adam-ui/tests/fixtures/catalog-v2.json        the screen's catalog (a copy of the web's)
#   CATALOG_LOCK  crates/adam-ui/tests/fixtures/catalog-v2.lock.json   its {version, digest}
#   TIMEOUT       120   seconds to wait for a task to stop
#
# Needs curl and jq. Run from anywhere: the script moves to the repository root.
set -eu

cd "$(dirname "$0")/.."

coder=${CODER_URL:-http://127.0.0.1:${CODER_PORT:-8080}}
coder=${coder%/}
token=${CODER_TOKEN:-dev-token}
timeout=${TIMEOUT:-120}
catalog_file=${CATALOG_FILE:-crates/adam-ui/tests/fixtures/catalog-v2.json}
catalog_lock=${CATALOG_LOCK:-crates/adam-ui/tests/fixtures/catalog-v2.lock.json}

for f in "$catalog_file" "$catalog_lock"; do
  if [ ! -f "$f" ]; then
    echo "FAIL $f does not exist"
    exit 1
  fi
done

fail=0
ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fail=1; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

ui_uri=https://agents.vymalo.com/a2a/extensions/ui-catalog/v1
tools_uri=https://agents.vymalo.com/a2a/extensions/thread-tools/v1
a2ui_uri=https://a2ui.org/a2a-extension/a2ui/v0.9.1
catalog_id=$(jq -r '.catalogId' "$catalog_file")
version=$(jq -r '.version' "$catalog_lock")
digest=$(jq -r '.digest' "$catalog_lock")

n=0
# send <rpc body> -> $tmp/events.jsonl, one JSON-RPC response per event.
send() {
  n=$((n + 1))
  stream=$tmp/stream.sse
  : > "$stream"
  curl_rc=0
  code=$(curl -sN --max-time "$timeout" -o "$stream" -w '%{http_code}' \
    -X POST "$coder/" \
    -H "Authorization: Bearer $token" -H 'Content-Type: application/json' \
    -d "$1") || curl_rc=$?
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

# The message the orchestrator sends for a first turn: the text, and what the screen tells the agent.
# first_message <text> <inline: true|false> <digest the message names>
first_message() {
  jq -n --arg id "choices-e2e-$(date +%s)-$$-$n" --arg ctx "choices-ctx-$(date +%s)-$$-$n" \
    --arg text "$1" --argjson inline "$2" \
    --arg ui "$ui_uri" --arg tools "$tools_uri" --arg a2ui "$a2ui_uri" \
    --arg catalogId "$catalog_id" --argjson version "$version" --arg digest "$3" \
    --slurpfile catalog "$catalog_file" '{
      jsonrpc: "2.0", id: "1", method: "SendStreamingMessage",
      params: {message: {
        messageId: $id, contextId: $ctx, role: "ROLE_USER", parts: [{text: $text}],
        extensions: [$a2ui, $ui],
        metadata: ({
          ($ui): {catalogId: $catalogId, version: $version, digest: $digest, inline: $inline},
          a2uiClientCapabilities: {"v0.9.1": ({supportedCatalogIds: [$catalogId]}
            + (if $inline then {inlineCatalogs: [$catalog[0]]} else {} end))}})}}}'
}

# last_input_required -> the status of the last update that reached INPUT_REQUIRED.
last_input_required() {
  jq -c 'select((.result.statusUpdate.status.state // .result.task.status.state) == "TASK_STATE_INPUT_REQUIRED")
         | (.result.statusUpdate.status // .result.task.status)' "$tmp/events.jsonl" | tail -n 1
}
task_id() {
  jq -r '(.result.task.id // .result.statusUpdate.taskId) // empty' "$tmp/events.jsonl" | head -n 1
}
context_id() {
  jq -r '(.result.task.contextId // .result.statusUpdate.contextId) // empty' "$tmp/events.jsonl" | head -n 1
}

# --- 1. The card lists the extensions -----------------------------------------------------------
echo "step 1: the card"
code=$(curl -s -o "$tmp/card.json" -w '%{http_code}' --max-time 30 "$coder/.well-known/agent-card.json" || true)
if [ "$code" != 200 ]; then bad "agent card: HTTP $code"; fi
for uri in "$ui_uri" "$tools_uri" "$a2ui_uri"; do
  if jq -e --arg u "$uri" '.capabilities.extensions[]? | select(.uri == $u)' "$tmp/card.json" >/dev/null 2>&1; then
    ok "the card lists $uri"
  else
    bad "the card does not list $uri"
  fi
done
if jq -e --arg u "$a2ui_uri" '.capabilities.extensions[] | select(.uri == $u) | .params.acceptsInlineCatalogs == true' "$tmp/card.json" >/dev/null 2>&1; then
  ok "the A2UI entry accepts inline catalogs"
else
  bad "the A2UI entry does not say acceptsInlineCatalogs: true"
fi

# --- 2. Three questions at once, as one form ----------------------------------------------------
echo "step 2: [mock:choices] with the catalog inline"
send "$(first_message '[mock:choices] set up the project' true "$digest")"
status=$(last_input_required)
task=$(task_id)
context=$(context_id)
if [ -n "$status" ]; then ok "the task ended TASK_STATE_INPUT_REQUIRED"; else bad "the task did not end INPUT_REQUIRED"; fi
text=$(printf '%s' "${status:-null}" | jq -r '[.message.parts[]? | .text? // empty] | join(" ")')
if [ "$text" = "Three quick questions before I start" ]; then ok "the question is the text part"; else bad "the text part is '$text'"; fi
surface=$(printf '%s' "${status:-null}" | jq -c '[.message.parts[]? | select(.mediaType == "application/a2ui+json") | .data] | first // empty')
if [ -n "$surface" ]; then ok "the status carries an application/a2ui+json data part"; else bad "no application/a2ui+json part in the status"; fi
if printf '%s' "${surface:-null}" | jq -e --arg id "$catalog_id" '.[0].createSurface.catalogId == $id' >/dev/null 2>&1; then
  ok "createSurface names the screen's catalog ($catalog_id)"
else
  bad "createSurface does not name $catalog_id: $surface"
fi
if printf '%s' "${surface:-null}" | jq -e '.[1].updateComponents.components[0] | .component == "Choices" and (.questions | map(.id)) == ["db", "auth", "deploy"] and .action.event.name == "answer"' >/dev/null 2>&1; then
  ok "one Choices of three questions (db, auth, deploy) whose action is answer"
else
  bad "the surface is not one Choices of db, auth, deploy: $surface"
fi
surface_id=$(printf '%s' "${surface:-null}" | jq -r '.[0].createSurface.surfaceId // empty')

# --- 3. The person answers, and the coder goes on ---------------------------------------------
echo "step 3: the answers (db=pg, auth=keycloak, deploy=compose) as one A2UI action"
if [ -n "$task" ] && [ -n "$surface_id" ]; then
  answer=$(jq -n --arg id "choices-e2e-answer-$(date +%s)-$$" --arg task "$task" --arg ctx "$context" \
    --arg surface "$surface_id" --arg ts "$(date -u +%Y-%m-%dT%H:%M:%SZ)" '{
      jsonrpc: "2.0", id: "2", method: "SendStreamingMessage",
      params: {message: {
        messageId: $id, taskId: $task, contextId: $ctx, role: "ROLE_USER",
        parts: [{
          data: [{version: "v0.9.1", action: {
            name: "answer", surfaceId: $surface, sourceComponentId: "root", timestamp: $ts,
            context: {answers: [
              {id: "db", values: ["pg"]}, {id: "auth", values: ["keycloak"]}, {id: "deploy", values: ["compose"]}]}}}],
          mediaType: "application/a2ui+json",
          metadata: {mimeType: "application/a2ui+json"}}]}}}')
  send "$answer"
  said=$(jq -r 'select((.result.statusUpdate.status.state // .result.task.status.state) == "TASK_STATE_INPUT_REQUIRED") | [.. | .text? // empty] | join(" ")' "$tmp/events.jsonl" | tail -n 1)
  case "$said" in
    *"Going with Postgres, Keycloak and Compose."*) ok "the coder's next words quote the answers: Postgres, Keycloak and Compose" ;;
    *) bad "the coder's next words are '$said', not the ones the answers should bring" ;;
  esac
else
  bad "no task or no surface from step 2: the answer cannot be sent"
fi

# --- 4. A screen the coder cannot read: the options are text ------------------------------------
echo "step 4: [mock:choices] naming a catalog the coder does not hold (no inline catalog, no grant)"
unknown=sha256:$(printf '0%.0s' $(seq 1 64))
send "$(first_message '[mock:choices] set up the project' false "$unknown")"
status=$(last_input_required)
if [ -n "$status" ]; then ok "the task ended TASK_STATE_INPUT_REQUIRED"; else bad "the task did not end INPUT_REQUIRED"; fi
parts=$(printf '%s' "${status:-null}" | jq -r '[.message.parts[]?] | length')
if [ "$parts" = 1 ]; then ok "the question is text only: no A2UI part"; else bad "the status has $parts parts, want 1"; fi
text=$(printf '%s' "${status:-null}" | jq -r '[.message.parts[]? | .text? // empty] | join(" ")')
case "$text" in
  *"1. Which database?"*"a) Postgres"*"b) SQLite"*) ok "the options are in the text" ;;
  *) bad "the text does not list the options: $text" ;;
esac

if [ "$fail" -eq 0 ]; then echo "coder choices e2e passed"; else echo "coder choices e2e FAILED"; exit 1; fi
