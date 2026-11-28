#!/bin/sh
# What adam reports for each model call, and what the model gateway itself reports for one call of
# the same model and effort: the ANR-75 human test, ready to run.
#
#   # Against the compose mocks first: green before anyone touches the real gateway.
#   docker compose --profile app up -d --wait postgres mock-openai agent
#   dev/gateway-usage-check.sh
#
#   # The ANR-75 run, against a deployment on the real gateway. The key comes from the environment
#   # and is never printed, never written to a file and never in a command line another process reads.
#   GATEWAY_URL=https://<the deployment's gateway> GATEWAY_KEY=... \
#   MODEL=deepseek-v4.1-flash EFFORT=high dev/gateway-usage-check.sh
#
# What it does:
#   1. reads the agent's card and refuses to guess: `usage/v1` must be listed (every agent lists it),
#      and when EFFORT is set the card must list `reasoning/v1` with that level;
#   2. sends one A2A message with both extensions activated (the `A2A-Extensions` header and
#      `metadata[reasoning/v1] = {"effort": ...}`) and collects adam's per-call report from the
#      stream's `working` status updates: the call id, input/output/total tokens, the reasoning and
#      cached counts when the provider gave them, the context window and the effort the call named;
#   3. when GATEWAY_URL and GATEWAY_KEY are set, makes ONE direct `POST /v1/chat/completions` on the
#      same model and effort and prints the gateway's own `usage` object beside adam's numbers;
#   4. prints exactly what to compare in the gateway's usage log: the request ids, the token counts
#      and the effort sent.
#
# Environment (never echoed):
#   AGENT_URL    http://127.0.0.1:${AGENT_PORT:-8084}   the agent to ask (the `agent` service of the
#                                                       compose stack by default)
#   TOKEN        dev-token                             the A2A bearer token (the stack's dummy)
#   MODEL        (the card's model)                    the alias the message names; with
#                                                      `model-pick/v1` it must be one the card lists
#   EFFORT       high                                  the reasoning level to pick; `none` picks the
#                                                      level `none`, `-` sends no reasoning extension
#   TEXT         In one sentence: what is a durable agent?
#   GATEWAY_URL  (unset)                               the OpenAI-compatible base of the gateway
#   GATEWAY_KEY  (unset)                               its key, sent as a bearer token
#   TIMEOUT      180                                   seconds to wait for the task to end
#
# Needs curl and jq. The wire is A2A 1.0 JSON-RPC over SSE (docs/reference/a2a-server.md), and the
# two extensions are documented there: `usage/v1` under *Usage* and `reasoning/v1` under *The
# reasoning effort*.
set -eu

agent=${AGENT_URL:-http://127.0.0.1:${AGENT_PORT:-8084}}
agent=${agent%/}
token=${TOKEN:-dev-token}
effort=${EFFORT:-high}
text=${TEXT:-In one sentence: what is a durable agent?}
gateway_url=${GATEWAY_URL:-}
gateway_key=${GATEWAY_KEY:-}
model=${MODEL:-}
timeout=${TIMEOUT:-180}

usage_uri=https://agents.vymalo.com/a2a/extensions/usage/v1
reasoning_uri=https://agents.vymalo.com/a2a/extensions/reasoning/v1
model_pick_uri=https://agents.vymalo.com/a2a/extensions/model-pick/v1

for tool in curl jq; do
  command -v "$tool" >/dev/null 2>&1 || {
    echo "dev/gateway-usage-check.sh needs $tool" >&2
    exit 69
  }
done

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

fail() {
  echo "FAIL: $1" >&2
  exit 1
}
ok() { echo "ok: $1"; }

# The card: what this agent offers. A card that cannot be read is a failure, not a skip.
card=$(curl -sf --max-time 15 "$agent/.well-known/agent-card.json") || {
  echo "no agent card at $agent/.well-known/agent-card.json: is the agent running?" >&2
  exit 69
}
name=$(printf '%s' "$card" | jq -r '.name // "agent"')
printf '%s' "$card" >"$tmp/card.json"

has_extension() {
  jq -e --arg u "$1" '[.capabilities.extensions[]?.uri] | index($u) != null' "$tmp/card.json" \
    >/dev/null 2>&1
}

has_extension "$usage_uri" || fail "$name's card does not list usage/v1"
ok "$name lists usage/v1"

# The levels the card offers, and the model choices, when the deployment profiles them.
levels=$(jq -r --arg u "$reasoning_uri" \
  '(.capabilities.extensions[]? | select(.uri == $u) | .params.efforts // []) | join(",")' \
  "$tmp/card.json")
choices=$(jq -r --arg u "$model_pick_uri" \
  '(.capabilities.extensions[]? | select(.uri == $u) | .params.choices // [] | map(.id)) | join(",")' \
  "$tmp/card.json")

metadata='{}'
activate="$usage_uri"
if [ "$effort" = "-" ]; then
  ok "no reasoning level picked (EFFORT=-): the reasoning extension is not activated"
else
  if [ -z "$levels" ]; then
    fail "$name's card lists no reasoning/v1: this deployment profiles no level for its model.
  The real run needs MODEL_ALIASES_FILE with reasoningEfforts for MODEL (docs/reference/environment.md)."
  fi
  case ",$levels," in
    *",$effort,"*) ;;
    *)
      fail "$effort is not one of the levels $name's card lists ($levels)"
      ;;
  esac
  metadata=$(jq -cn --arg u "$reasoning_uri" --arg e "$effort" '{($u): {effort: $e}}')
  activate="$usage_uri,$reasoning_uri"
  ok "$name lists reasoning/v1 with $effort (levels: $levels)"
fi

# The model a message picks (`model-pick/v1`) is sent only when the card offers it and MODEL names
# one of its choices; an agent with one model lists no extension and takes no entry.
if [ -n "$model" ] && [ -n "$choices" ]; then
  case ",$choices," in
    *",$model,"*) ;;
    *) fail "$model is not one of the models $name's card offers ($choices)" ;;
  esac
  metadata=$(jq -cn --argjson m "$metadata" --arg u "$model_pick_uri" --arg model "$model" \
    '$m + {($u): {model: $model}}')
  activate="$activate,$model_pick_uri"
  ok "$name offers $model (choices: $choices)"
fi

message_id="usage-check-$(date +%s)-$$"
rpc=$(jq -cn --arg id "$message_id" --arg text "$text" --argjson metadata "$metadata" \
  '{jsonrpc: "2.0", id: "1", method: "SendStreamingMessage",
    params: {message: {messageId: $id, role: "ROLE_USER", parts: [{text: $text}], metadata: $metadata}}}')

echo
echo "asking $name (message $message_id)…"
: >"$tmp/events.jsonl"
code=$(curl -sN --max-time "$timeout" -o "$tmp/stream.sse" -w '%{http_code}' -X POST "$agent/" \
  -H "Authorization: Bearer $token" \
  -H 'Content-Type: application/json' \
  -H "A2A-Extensions: $activate" \
  -d "$rpc") || {
  echo "the request failed before the task ended (curl exit $?)" >&2
  exit 1
}
if [ "$code" != 200 ]; then
  fail "the agent answered HTTP $code: $(head -c 300 "$tmp/stream.sse")"
fi
sed -n 's/^data: //p' "$tmp/stream.sse" | jq -c '.' >"$tmp/events.jsonl" 2>/dev/null || true

rpc_error=$(jq -r 'select(.error) | .error | "\(.code): \(.message)"' "$tmp/events.jsonl" | head -n 1)
[ -z "$rpc_error" ] || fail "the agent answered a JSON-RPC error: $rpc_error"

# adam's report: one `working` status update with no message per completed model call, the call's
# usage under the extension's URI. A call that named no level has no `effort` (not the same as
# `none`).
jq -c --arg u "$usage_uri" '
  (.result.statusUpdate // empty) as $s
  | select($s.metadata[$u] != null and ($s.metadata[$u].call != null))
  | $s.metadata[$u]' "$tmp/events.jsonl" >"$tmp/calls.jsonl" 2>/dev/null || true
calls=$(wc -l <"$tmp/calls.jsonl" | tr -d ' ')
[ "$calls" -ge 1 ] || fail "adam reported no model call: no usage/v1 status update arrived"

echo
echo "adam's report, per model call:"
jq -r '"  call \(.call)  model \(.model)  in \(.inputTokens | round)  out \(.outputTokens | round)  total \(.totalTokens | round)"
  + (if .reasoningTokens != null then "  reasoning \(.reasoningTokens | round)" else "  reasoning (not reported)" end)
  + (if .cachedInputTokens != null then "  cached \(.cachedInputTokens | round)" else "  cached (not reported)" end)
  + (if .contextWindow != null then "  window \(.contextWindow | round)" else "" end)
  + (if .effort != null then "  effort \(.effort)" else "  effort (none named)" end)' "$tmp/calls.jsonl"

# The level each call names, when one was picked: a call that ignored the metadata is a failure of
# the run, not a provider quirk. (`totalTokens` is `input + output` by construction in
# `adam-a2a-runtime`'s `token_usage`, so re-adding them here would only restate the producer.)
if [ "$effort" != "-" ]; then
  wrong=$(jq -r --arg e "$effort" 'select(.effort != $e) | "\(.call): \(.effort // "none")"' "$tmp/calls.jsonl")
  [ -z "$wrong" ] || fail "the call did not name $effort: $wrong"
  ok "every call names the level picked ($effort)"
fi

state=$(jq -r '(.result.statusUpdate.status.state // .result.task.status.state) // empty' "$tmp/events.jsonl" | tail -n 1)
[ -n "$state" ] || fail "the stream ended without a task state"
case "$state" in
  TASK_STATE_COMPLETED) ok "the task ended $state" ;;
  TASK_STATE_FAILED | TASK_STATE_CANCELED | TASK_STATE_REJECTED)
    fail "the task ended $state: the comparison below would be about a run that did not finish"
    ;;
  *) ok "the task ended $state" ;;
esac

# The gateway's own answer for one call of the same model and effort, when it is given: the other
# side of the comparison ANR-75 asks for (does it return cached and reasoning tokens at all?).
if [ -n "$gateway_url" ] && [ -n "$gateway_key" ]; then
  if [ -z "$model" ]; then
    echo
    echo "not compared with the gateway: MODEL is not set, and the gateway call needs the alias."
  else
    request=$(jq -cn --arg model "$model" --arg text "$text" --arg effort "$effort" '
      {model: $model, messages: [{role: "user", content: $text}], max_tokens: 64}
      + (if $effort == "-" then {} else {reasoning_effort: $effort} end)')
    echo
    echo "one direct call to the gateway ($model, effort $effort)…"
    gcode=$(curl -sS --max-time "$timeout" -o "$tmp/gateway.json" -w '%{http_code}' \
      -X POST "${gateway_url%/}/v1/chat/completions" \
      -H "Authorization: Bearer $gateway_key" \
      -H 'Content-Type: application/json' \
      -d "$request") || fail "the gateway call failed (curl exit $?)"
    if [ "$gcode" != 200 ]; then
      fail "the gateway answered HTTP $gcode: $(head -c 300 "$tmp/gateway.json")"
    fi
    # A gateway that answers 200 with no usage block would otherwise print `null` everywhere and
    # pass, and the owner would read it as "the call cost nothing".
    if [ "$(jq -r '.usage.prompt_tokens // .usage.total_tokens // empty' "$tmp/gateway.json")" = "" ]; then
      fail "the gateway returned no usage block: $(head -c 300 "$tmp/gateway.json")"
    fi
    echo "the gateway's own usage for that call:"
    jq -r '"  in \(.usage.prompt_tokens)  out \(.usage.completion_tokens)  total \(.usage.total_tokens)"
      + (if .usage.prompt_tokens_details.cached_tokens != null then "  cached \(.usage.prompt_tokens_details.cached_tokens)" else "  cached (not returned)" end)
      + (if .usage.completion_tokens_details.reasoning_tokens != null then "  reasoning \(.usage.completion_tokens_details.reasoning_tokens)" else "  reasoning (not returned)" end)' \
      "$tmp/gateway.json"
  fi
else
  echo
  echo "no gateway call: set GATEWAY_URL and GATEWAY_KEY for the other half of the comparison."
fi

cat <<'COMPARE'

What to compare in the gateway's usage log (the ANR-75 question):
  1. the request ids of adam's calls (the `call` above) must appear in the log, one entry each;
  2. the log's prompt/completion tokens for a call against adam's in/out for it — the numbers the
     owner is charged are the log's;
  3. whether the log returns `cached_tokens` and `reasoning_tokens` at all, and whether they match
     adam's `cached`/`reasoning` (adam reports only what the provider sent, never an estimate);
  4. the `effort` above against the reasoning effort the log records as sent for that request.
COMPARE
