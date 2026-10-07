#!/bin/sh
# HTTP-level smoke test of a running adam-coder (or adam-agent): liveness, the public agent
# card, fail-closed authentication and one task whose model answers in text. Works against the
# container (container-smoke.sh, agent-smoke.sh) or a locally started binary.
#
#   http-smoke.sh <base-url> <bearer-token>
#
# The model behind the agent must be docker/coder/test/fake-model.py, which
# answers "Nothing to do." without calling a tool. For the coder, a run that stops with text
# and no pull request has delivered nothing, so the task waits for the person
# (input-required) with that text as the question; it does not complete. A general agent
# (adam-agent) answers: its task completes with that text.
#
# What the agent is says what is expected (the defaults are the coder's):
#   EXPECT_NAME   Adam                         the name on the agent card
#   EXPECT_SKILL  coding-task                  a skill id the card lists
#   EXPECT_STATE  TASK_STATE_INPUT_REQUIRED    the state the task reaches
#   EXPECT_REVISION  (unset)                   the commit the image was built from (its build argument
#                                              ADAM_BUILD_REVISION): the card's version must end with
#                                              +<its first 7 characters> and `build/v1` must say it whole.
#                                              Unset, the version only has to carry build metadata (+...).
# Needs curl and jq.
set -eu

if [ "$#" -ne 2 ]; then
  echo "usage: $0 <base-url> <bearer-token>" >&2
  exit 2
fi
base=${1%/}
token=$2
expect_name=${EXPECT_NAME:-Adam}
expect_skill=${EXPECT_SKILL:-coding-task}
expect_state=${EXPECT_STATE:-TASK_STATE_INPUT_REQUIRED}
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
if [ "$code" = 200 ] && [ "$(jq -r '.name' "$body")" = "$expect_name" ]; then
  ok "the agent card is public and names the agent $expect_name"
else
  bad "agent card: status $code, name $(jq -r '.name' "$body" 2>/dev/null || echo '?'), want $expect_name"
fi
if jq -e --arg skill "$expect_skill" '.skills[] | select(.id == $skill)' "$body" >/dev/null 2>&1; then
  ok "the agent card lists the $expect_skill skill"
else
  bad "the agent card lacks the $expect_skill skill"
fi
if jq -e '(.securitySchemes // .security_schemes // {}) | length > 0' "$body" >/dev/null 2>&1; then
  ok "the agent card declares a security scheme"
else
  bad "the agent card declares no security scheme"
fi

# The card says which build answers (the version's build metadata) and which agent files it runs
# (`build/v1`, ADR 0028): a thread export can then tell which build produced an answer.
version=$(jq -r '.version // empty' "$body" 2>/dev/null || true)
if [ -n "${EXPECT_REVISION:-}" ]; then
  short=$(printf '%s' "$EXPECT_REVISION" | cut -c1-7)
  case "$version" in
    *+"$short") ok "the card's version $version ends with the build's revision +$short" ;;
    *) bad "the card's version is '$version', want build metadata +$short" ;;
  esac
  got=$(jq -r '.capabilities.extensions[]? | select(.uri == "https://agents.vymalo.com/a2a/extensions/build/v1") | .params.revision' "$body" 2>/dev/null || true)
  if [ "$got" = "$EXPECT_REVISION" ]; then ok "build/v1 says the revision $got"; else bad "build/v1 says revision '$got', want $EXPECT_REVISION"; fi
else
  case "$version" in
    *+?*) ok "the card's version $version carries build metadata" ;;
    *) bad "the card's version is '$version', want semver build metadata (+<revision> or +unknown)" ;;
  esac
fi
if jq -e '.capabilities.extensions[]? | select(.uri == "https://agents.vymalo.com/a2a/extensions/build/v1") | .params.folderDigest | startswith("sha256:")' "$body" >/dev/null 2>&1; then
  ok "build/v1 says the digest of the agent files"
else
  bad "build/v1 does not say a sha256 folderDigest"
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

# A task with the token: the text-only model answers. The coder delivered nothing, so its task asks;
# a general agent is done.
code=$(status -X POST -H 'Content-Type: application/json' -H "Authorization: Bearer $token" -d "$rpc" "$base/")
state=$(jq -r '.result.task.status.state // .result.status.state // empty' "$body" 2>/dev/null || true)
text=$(jq -r '[.. | .text? // empty] | join(" ")' "$body" 2>/dev/null || true)
if [ "$code" = 200 ] && [ "$state" = "$expect_state" ]; then
  ok "SendMessage leaves the task in $expect_state"
else
  bad "SendMessage: status $code, state '$state', want $expect_state, body: $(head -c 400 "$body")"
fi
case "$text" in
  *"Nothing to do."*) ok "the task carries the model's text" ;;
  *) bad "the task does not carry the model's text: '$text'" ;;
esac

if [ "$fail" -eq 0 ]; then echo "http smoke passed"; else echo "http smoke FAILED"; exit 1; fi
