#!/bin/sh
# HTTP-level smoke test of a running adam-coder: liveness, the public agent
# card, fail-closed authentication and one task whose model answers in text. Works against the
# container (container-smoke.sh) or a locally started binary.
#
#   http-smoke.sh <base-url> <bearer-token>
#
# The model behind the coder must be docker/coder/test/fake-model.py, which
# answers "Nothing to do." without calling a tool. A run that stops with text and
# no pull request has delivered nothing, so the task waits for the person
# (input-required) with that text as the question; it does not complete.
# Needs curl and jq.
set -eu

if [ "$#" -ne 2 ]; then
  echo "usage: $0 <base-url> <bearer-token>" >&2
  exit 2
fi
base=${1%/}
token=$2
fail=0
body=$(mktemp)
trap 'rm -f "$body"' EXIT

ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fail=1; }

status() { # status <curl args...> -> HTTP status code (body in $body)
  curl -s -o "$body" -w '%{http_code}' --max-time 30 "$@" || true
}

# Liveness: 200 within 60 s of start.
deadline=$(( $(date +%s) + 60 ))
code=000
while [ "$(date +%s)" -lt "$deadline" ]; do
  code=$(status "$base/healthz")
  [ "$code" = 200 ] && break
  sleep 1
done
if [ "$code" = 200 ]; then ok "GET /healthz is 200"; else bad "GET /healthz is $code, want 200 within 60 s"; fi

# The agent card is public (no token) and names the agent and its skill.
code=$(status "$base/.well-known/agent-card.json")
if [ "$code" = 200 ] && [ "$(jq -r '.name' "$body")" = adam-coder ]; then
  ok "the agent card is public and names adam-coder"
else
  bad "agent card: status $code, name $(jq -r '.name' "$body" 2>/dev/null || echo '?')"
fi
if jq -e '.skills[] | select(.id == "coding-task")' "$body" >/dev/null 2>&1; then
  ok "the agent card lists the coding-task skill"
else
  bad "the agent card lacks the coding-task skill"
fi
if jq -e '(.securitySchemes // .security_schemes // {}) | length > 0' "$body" >/dev/null 2>&1; then
  ok "the agent card declares a security scheme"
else
  bad "the agent card declares no security scheme"
fi

rpc='{"jsonrpc":"2.0","id":1,"method":"SendMessage","params":{"message":{"messageId":"smoke-1","role":"ROLE_USER","parts":[{"text":"Say nothing."}]}}}'

# Fail closed: no token, a wrong token, a token that is a prefix of the real one.
code=$(status -X POST -H 'Content-Type: application/json' -d "$rpc" "$base/")
if [ "$code" = 401 ]; then ok "POST / without a token is 401"; else bad "POST / without a token is $code, want 401"; fi
code=$(status -X POST -H 'Content-Type: application/json' -H 'Authorization: Bearer wrong-token' -d "$rpc" "$base/")
if [ "$code" = 401 ]; then ok "POST / with a wrong token is 401"; else bad "POST / with a wrong token is $code, want 401"; fi
code=$(status -X POST -H 'Content-Type: application/json' -H "Authorization: Bearer $(printf '%s' "$token" | cut -c1-1)" -d "$rpc" "$base/")
if [ "$code" = 401 ]; then ok "POST / with a token prefix is 401"; else bad "POST / with a token prefix is $code, want 401"; fi
code=$(status "$base/some/other/path")
if [ "$code" = 401 ] || [ "$code" = 404 ]; then ok "an unknown route is not served ($code)"; else bad "an unknown route is $code"; fi

# A task with the token: the text-only model stops with nothing delivered, so the task asks.
code=$(status -X POST -H 'Content-Type: application/json' -H "Authorization: Bearer $token" -d "$rpc" "$base/")
state=$(jq -r '.result.task.status.state // .result.status.state // empty' "$body" 2>/dev/null || true)
text=$(jq -r '[.. | .text? // empty] | join(" ")' "$body" 2>/dev/null || true)
if [ "$code" = 200 ] && [ "$state" = TASK_STATE_INPUT_REQUIRED ]; then
  ok "SendMessage leaves the task waiting for the person (input-required), not completed"
else
  bad "SendMessage: status $code, state '$state', body: $(head -c 400 "$body")"
fi
case "$text" in
  *"Nothing to do."*) ok "the task asks with the model's text" ;;
  *) bad "the task does not carry the model's text: '$text'" ;;
esac

if [ "$fail" -eq 0 ]; then echo "http smoke passed"; else echo "http smoke FAILED"; exit 1; fi
