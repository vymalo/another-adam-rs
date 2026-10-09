#!/bin/sh
# End-to-end test of the researcher answering with cards and a graph, against the compose mocks: the
# general agent (`adam-agent`, service `agent`) serving the researcher folder (dev/agents/researcher/agent) is
# asked a question that carries `[mock:cards]` and the screen's UI catalog (version 3, with `Cards` and
# `Mermaid`) in its metadata, and answers with one A2UI surface beside its words: a Text, three source
# cards and a mermaid graph, under the screen's own catalogId. A screen that cannot draw them (catalog
# version 2, or no catalog) gets the words only.
#
#   dev/agent-cards-e2e.sh                 # all steps
#   NO_RESTART=1 dev/agent-cards-e2e.sh    # the agent already serves the researcher on `mock-researcher`
#
# Start the stack first (the model `mock-researcher` is dev/wiremock/mock-openai/mappings/researcher-cards.json;
# no search server, repository or GitHub is needed: the script's question does not search):
#
#   docker compose --profile app up -d --build --wait postgres mock-openai agent
#
# Steps:
#   0. (unless NO_RESTART=1) a copy of the researcher folder, without its `mcp.json` (the web-search server it
#      names is the orchestration layer's mock, not part of this stack), is mounted in place of the default
#      folder, with the model `mock-researcher` (AGENT_FOLDER=<copy> AGENT_MODEL=mock-researcher docker compose
#      --profile app up -d --no-build --wait agent). The default folder and model are put back at the end,
#      also when a check failed (a trap).
#   1. The card is the researcher's, and lists the extensions that make this work: A2UI v0.9.1 (taking the
#      catalog inline), ui-catalog/v1 and thread-tools/v1.
#   2. SendStreamingMessage "[mock:cards] what is async rust?" with catalog version 3 in the message's
#      metadata (what the orchestrator sends: ui-catalog/v1 {version, digest, inline}, and the catalog in the
#      A2UI capabilities): the task ends TASK_STATE_COMPLETED, with a `ui` artifact (media type
#      application/a2ui+json): createSurface under the screen's catalogId, and a Column of a Text, a Cards of
#      three cards (each with an https link) and a Mermaid graph; the words name the three links.
#   3. The same question from a screen on catalog version 2, which has no `Cards`: `show` is refused, so the
#      task ends TASK_STATE_COMPLETED with the words and no `ui` artifact. The same with no catalog at all.
# It prints one "ok" or "FAIL" line per check and exits 1 if any failed.
#
# Environment (defaults match compose.yaml on one machine):
#   AGENT_URL        http://127.0.0.1:${AGENT_PORT:-8084}
#   AGENT_TOKEN      dev-token
#   RESEARCHER_DIR   dev/agents/researcher/agent                      the folder to serve
#   CATALOG_FILE     crates/adam-ui/tests/fixtures/catalog-v3.json     the screen's catalog (a copy of the web's)
#   CATALOG_LOCK     crates/adam-ui/tests/fixtures/catalog-v3.lock.json  its {version, digest}
#   OLD_CATALOG_FILE crates/adam-ui/tests/fixtures/catalog-v2.json     an older screen's catalog, without Cards
#   OLD_CATALOG_LOCK crates/adam-ui/tests/fixtures/catalog-v2.lock.json
#   TIMEOUT          120   seconds to wait for a task to stop
#   NO_RESTART       unset 1 = skip step 0 (no `docker compose`)
#
# Needs curl and jq (and, for step 0, docker compose and a coder image `--no-build` can use).
# Run from anywhere: the script moves to the repository root.
set -eu

cd "$(dirname "$0")/.."

agent=${AGENT_URL:-http://127.0.0.1:${AGENT_PORT:-8084}}
agent=${agent%/}
token=${AGENT_TOKEN:-dev-token}
timeout=${TIMEOUT:-120}
researcher_dir=${RESEARCHER_DIR:-dev/agents/researcher/agent}
catalog_file=${CATALOG_FILE:-crates/adam-ui/tests/fixtures/catalog-v3.json}
catalog_lock=${CATALOG_LOCK:-crates/adam-ui/tests/fixtures/catalog-v3.lock.json}
old_file=${OLD_CATALOG_FILE:-crates/adam-ui/tests/fixtures/catalog-v2.json}
old_lock=${OLD_CATALOG_LOCK:-crates/adam-ui/tests/fixtures/catalog-v2.lock.json}

for f in "$researcher_dir/instructions.md" "$catalog_file" "$catalog_lock" "$old_file" "$old_lock"; do
  if [ ! -f "$f" ]; then
    echo "FAIL $f does not exist"
    exit 1
  fi
done

fail=0
ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fail=1; }

tmp=$(mktemp -d)
copy=
restart_needed=0
original_dir=${AGENT_FOLDER-}
original_model=${AGENT_MODEL-}

# Start the agent on the folder and model the stack mounts without our overrides again (or on the ones
# the caller named in AGENT_FOLDER and AGENT_MODEL).
put_back() {
  (
    if [ -n "$original_dir" ]; then export AGENT_FOLDER="$original_dir"; else unset AGENT_FOLDER; fi
    if [ -n "$original_model" ]; then export AGENT_MODEL="$original_model"; else unset AGENT_MODEL; fi
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

ui_uri=https://agents.vymalo.com/a2a/extensions/ui-catalog/v1
tools_uri=https://agents.vymalo.com/a2a/extensions/thread-tools/v1
a2ui_uri=https://a2ui.org/a2a-extension/a2ui/v0.9.1
catalog_id=$(jq -r '.catalogId' "$catalog_file")

n=0
# send <rpc body> -> $tmp/events.jsonl, one JSON-RPC response per event.
send() {
  n=$((n + 1))
  stream=$tmp/stream.sse
  : > "$stream"
  curl_rc=0
  code=$(curl -sN --max-time "$timeout" -o "$stream" -w '%{http_code}' \
    -X POST "$agent/" \
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

# The message the orchestrator sends for a first turn, from a screen on <catalog file> / <lock>:
# the text, and what the screen tells the agent. With "none" for the file, no catalog at all.
# first_message <text> <catalog file|none> <lock file>
first_message() {
  if [ "$2" = none ]; then
    jq -n --arg id "cards-e2e-$(date +%s)-$$-$n" --arg ctx "cards-ctx-$(date +%s)-$$-$n" --arg text "$1" '{
      jsonrpc: "2.0", id: "1", method: "SendStreamingMessage",
      params: {message: {messageId: $id, contextId: $ctx, role: "ROLE_USER", parts: [{text: $text}]}}}'
    return
  fi
  jq -n --arg id "cards-e2e-$(date +%s)-$$-$n" --arg ctx "cards-ctx-$(date +%s)-$$-$n" \
    --arg text "$1" --arg ui "$ui_uri" --arg a2ui "$a2ui_uri" \
    --arg catalogId "$catalog_id" --slurpfile lock "$3" --slurpfile catalog "$2" '{
      jsonrpc: "2.0", id: "1", method: "SendStreamingMessage",
      params: {message: {
        messageId: $id, contextId: $ctx, role: "ROLE_USER", parts: [{text: $text}],
        extensions: [$a2ui, $ui],
        metadata: {
          ($ui): {catalogId: $catalogId, version: $lock[0].version, digest: $lock[0].digest, inline: true},
          a2uiClientCapabilities: {"v0.9.1": {supportedCatalogIds: [$catalogId], inlineCatalogs: [$catalog[0]]}}}}}}'
}

last_state() {
  jq -r '(.result.statusUpdate.status.state // .result.task.status.state) // empty' "$tmp/events.jsonl" | tail -n 1
}
# The words of the update that reached `state`.
words_of() {
  jq -r --arg s "$1" 'select((.result.statusUpdate.status.state // .result.task.status.state) == $s) | [.. | .text? // empty] | join(" ")' "$tmp/events.jsonl" | tail -n 1
}
# The `ui` artifacts the stream delivered, one JSON object a line: as artifact updates, and in the
# opening `task`, which carries what the run made before the subscription attached (the mocks
# answer fast, so `show` can be done by then) and is then not sent again as an update.
ui_artifacts() {
  jq -c '(.result.artifactUpdate.artifact // empty), (.result.task.artifacts[]?) | select(.name == "ui")' \
    "$tmp/events.jsonl"
}
# The A2UI messages of the `ui` artifact (the last one), as one JSON array, or nothing.
surface() {
  ui_artifacts | jq -c '[.parts[] | select(.mediaType == "application/a2ui+json") | .data] | first // empty' | tail -n 1
}
artifact_count() {
  ui_artifacts | jq -r '.artifactId' | sort -u | wc -l | tr -d ' '
}

# --- 0. The agent serves the researcher on the scripted model ----------------------------------
if [ "${NO_RESTART:-}" = 1 ]; then
  echo "step 0: skipped (NO_RESTART=1): the agent serves the researcher on mock-researcher"
elif ! command -v docker >/dev/null 2>&1 || ! docker compose version >/dev/null 2>&1; then
  echo "FAIL step 0: docker compose is not available here (NO_RESTART=1 skips it when the agent already serves the researcher)"
  exit 1
else
  echo "step 0: the researcher folder (without its search server) and the model mock-researcher, in place of the default"
  copy=$(mktemp -d)
  cp -R "$researcher_dir"/. "$copy"/
  rm -f "$copy/mcp.json"
  # The container runs as uid 10001: the copy must be readable by others.
  chmod -R a+rX "$copy"
  restart_needed=1
  if AGENT_FOLDER=$copy AGENT_MODEL=mock-researcher docker compose --profile app up -d --no-build --wait agent >/dev/null 2>&1; then
    ok "the agent restarted on the researcher folder"
  else
    bad "the agent did not come back with AGENT_FOLDER=$copy AGENT_MODEL=mock-researcher"
  fi
fi

# --- 1. The card ------------------------------------------------------------------------------------
echo "step 1: the card"
code=$(curl -s -o "$tmp/card.json" -w '%{http_code}' --max-time 30 "$agent/.well-known/agent-card.json" || true)
got=$(jq -r '.name // empty' "$tmp/card.json" 2>/dev/null || true)
if [ "$code" = 200 ] && [ "$got" = Researcher ]; then ok "the agent card names the researcher"; else bad "agent card: status $code, name '${got:-?}', want Researcher"; fi
for uri in "$ui_uri" "$tools_uri" "$a2ui_uri"; do
  if jq -e --arg u "$uri" '.capabilities.extensions[]? | select(.uri == $u)' "$tmp/card.json" >/dev/null 2>&1; then
    ok "the card lists $uri"
  else
    bad "the card does not list $uri"
  fi
done

# --- 2. Cards and a graph on a screen that draws them -------------------------------------------
echo "step 2: [mock:cards] with catalog version $(jq -r '.version' "$catalog_lock") inline"
send "$(first_message '[mock:cards] what is async rust?' "$catalog_file" "$catalog_lock")"
state=$(last_state)
if [ "$state" = TASK_STATE_COMPLETED ]; then ok "the task ended TASK_STATE_COMPLETED"; else bad "the task ended '${state:-none}', want TASK_STATE_COMPLETED"; fi
surface=$(surface)
if [ -n "$surface" ]; then ok "a ui artifact carries an application/a2ui+json part"; else bad "no ui artifact with an application/a2ui+json part"; fi
if [ "$(artifact_count)" = 1 ]; then ok "one ui artifact"; else bad "$(artifact_count) ui artifacts, want 1"; fi
if printf '%s' "${surface:-null}" | jq -e --arg id "$catalog_id" '.[0].createSurface.catalogId == $id' >/dev/null 2>&1; then
  ok "createSurface names the screen's catalog ($catalog_id)"
else
  bad "createSurface does not name $catalog_id: $surface"
fi
components='.[1].updateComponents.components'
if printf '%s' "${surface:-null}" | jq -e "$components | map(.component) == [\"Column\", \"Text\", \"Cards\", \"Mermaid\"]" >/dev/null 2>&1; then
  ok "the surface is a Column of a Text, Cards and a Mermaid"
else
  bad "the surface is not Column, Text, Cards, Mermaid: $surface"
fi
if printf '%s' "${surface:-null}" | jq -e "$components | map(select(.component == \"Cards\")) | .[0].cards | length == 3 and all(.[]; .url | startswith(\"https://\"))" >/dev/null 2>&1; then
  ok "the Cards holds three cards, each with an https link"
else
  bad "the Cards is not three cards with https links: $surface"
fi
if printf '%s' "${surface:-null}" | jq -e "$components | map(select(.component == \"Mermaid\")) | .[0].code | startswith(\"graph TD\")" >/dev/null 2>&1; then
  ok "the Mermaid is a graph TD"
else
  bad "the Mermaid is not a graph TD: $surface"
fi
said=$(words_of TASK_STATE_COMPLETED)
for i in 1 2 3; do
  case "$said" in
    *"https://example.org/mock-search/$i"*) ok "the words name source $i" ;;
    *) bad "the words do not name https://example.org/mock-search/$i: $said" ;;
  esac
done

# --- 3. A screen without Cards, and a screen without a catalog: words only -----------------------
echo "step 3: the same question from a screen on catalog version $(jq -r '.version' "$old_lock") (no Cards)"
send "$(first_message '[mock:cards] what is async rust?' "$old_file" "$old_lock")"
state=$(last_state)
if [ "$state" = TASK_STATE_COMPLETED ]; then ok "the task ended TASK_STATE_COMPLETED"; else bad "the task ended '${state:-none}', want TASK_STATE_COMPLETED"; fi
if [ -z "$(surface)" ]; then ok "no surface: the old screen cannot draw cards"; else bad "a surface was sent to a screen without Cards: $(surface)"; fi
case "$(words_of TASK_STATE_COMPLETED)" in
  *"https://example.org/mock-search/1"*) ok "the words still name the sources" ;;
  *) bad "the words do not name the sources: $(words_of TASK_STATE_COMPLETED)" ;;
esac

echo "step 3b: the same question from a screen that sent no catalog"
send "$(first_message '[mock:cards] what is async rust?' none none)"
state=$(last_state)
if [ "$state" = TASK_STATE_COMPLETED ]; then ok "the task ended TASK_STATE_COMPLETED"; else bad "the task ended '${state:-none}', want TASK_STATE_COMPLETED"; fi
if [ -z "$(surface)" ]; then ok "no surface: there is no screen to draw on"; else bad "a surface was sent with no catalog: $(surface)"; fi
case "$(words_of TASK_STATE_COMPLETED)" in
  *"https://example.org/mock-search/1"*) ok "the words still name the sources" ;;
  *) bad "the words do not name the sources: $(words_of TASK_STATE_COMPLETED)" ;;
esac

# --- put the default folder back ---------------------------------------------------------------
if [ "$restart_needed" = 1 ]; then
  restart_needed=0
  if put_back; then ok "the agent restarted on the default folder and model"; else bad "the agent did not come back on the default folder"; restart_needed=1; fi
fi

if [ "$fail" -eq 0 ]; then echo "agent cards e2e passed"; else echo "agent cards e2e FAILED"; exit 1; fi
