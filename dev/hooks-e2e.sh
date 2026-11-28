#!/bin/sh
# End-to-end test of the deployment's hook programs (ADR 0050, slices P1 and P2, ANR-1301) against
# the compose stack: a program denies the coder's `commit_and_push`, so a task that would push stops
# at that call and nothing reaches git-server.
#
#   dev/hooks-e2e.sh                 # all steps
#
# Start the stack with the hook override first: `dev/hooks/hooks.json` declares the program
# `dev/hooks/no-git-push.sh` (mounted at /etc/adam/hooks), `ADAM_HOOKS_FILE` names it and
# `HOOKS_ALLOW_PROGRAMS=true` lets the coder run it.
#
#   docker compose -f compose.yaml -f dev/compose.hooks.yaml --profile app up -d --build --wait \
#     postgres mock-openai mock-github mock-github-mcp git-server coder
#
# The coder's model is the scripted `mock-coder` (dev/wiremock/mock-openai/mappings/coder-script.json):
# the default task prepares the workspace, writes hello.txt, runs the checks, then calls
# `commit_and_push` as the tool call `coder-call-4`. The script sends that task and checks:
#   * the step `tool:coder-call-4` ended `failed` and its output carries the program's reason
#     (`dev/hooks/no-git-push.sh`): the call was denied before it ran, not failed by git or the
#     checks. The request activates `steps/v1`, so the step is read as the report, not only as a line;
#   * `commit_and_push` never ran: git-server's heads of `local/sandbox` are the ones it had before
#     the task (a push would add one), and mock-github saw no `POST /repos/local/sandbox/pulls`.
# A stack started without the override fails the first check, and the message says how to start it.
# A task that runs out of time is a failure too. It prints one "ok" or "FAIL" line per check and
# exits 1 if any failed.
#
# Environment (defaults match compose.yaml on one machine):
#   CODER_URL        http://127.0.0.1:${CODER_PORT:-8080}   the A2A endpoint
#   CODER_TOKEN      dev-token
#   GIT_SERVER_URL   http://127.0.0.1:${GIT_SERVER_PORT:-8083}   from the host
#   MOCK_GITHUB_URL  http://127.0.0.1:${MOCK_GITHUB_PORT:-8082}   its request journal
#   REPO_BASE_URL    http://git-server:8080   where the coder (inside the compose network) finds git-server
#   TIMEOUT          300   seconds to wait for the task to stop
#
# Needs curl, jq and git. The owner's rule exempts the dev compose stacks from the heavy-job rule:
# this runs the stack this machine already has.
set -eu

cd "$(dirname "$0")/.."

coder=${CODER_URL:-http://127.0.0.1:${CODER_PORT:-8080}}
coder=${coder%/}
token=${CODER_TOKEN:-dev-token}
gitserver=${GIT_SERVER_URL:-http://127.0.0.1:${GIT_SERVER_PORT:-8083}}
gitserver=${gitserver%/}
github=${MOCK_GITHUB_URL:-http://127.0.0.1:${MOCK_GITHUB_PORT:-8082}}
github=${github%/}
repo_base=${REPO_BASE_URL:-http://git-server:8080}
repo_base=${repo_base%/}
timeout=${TIMEOUT:-300}

repo=local/sandbox
repo_url=$repo_base/$repo.git
# The extension that reports every tool call as a step with the call's id and result (`steps/v1`).
steps_ext=https://agents.vymalo.com/a2a/extensions/steps/v1
# The scripted call of `commit_and_push` (dev/wiremock/mock-openai/mappings/coder-script.json).
push_call='coder-call-4'
# The words `dev/hooks/no-git-push.sh` answers with: the denial is this, not a git or check failure.
reason="the deployment's hook program denies commit_and_push"

fail=0
ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fail=1; }

tmp=$(mktemp -d)
# shellcheck disable=SC2329 # invoked by the trap below, which shellcheck does not follow
cleanup() {
  rm -rf "$tmp"
}
trap cleanup EXIT

# --- what git-server has before the task ------------------------------------------
heads_before=$(git ls-remote --heads "$gitserver/$repo.git" 2>/dev/null | sort || true)
if [ -n "$heads_before" ]; then
  n_heads=$(printf '%s\n' "$heads_before" | wc -l | tr -d ' ')
  ok "git-server has $n_heads head(s) of $repo before the task"
else
  bad "git-server does not answer for $repo (is the stack up? is GIT_SERVER_URL right?)"
fi

# --- the task ---------------------------------------------------------------------
# Reset mock-github's journal, so "no pull request" is about this task and not an earlier one.
code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 30 -X DELETE "$github/__admin/requests" || true)
if [ "$code" = 200 ]; then ok "mock-github journal reset"; else bad "mock-github journal reset: HTTP $code"; fi

run_id=hook-$(date +%s)-$$
rpc=$(jq -n --arg id "$run_id" --arg text "In $repo_url (base branch main), add hello.txt containing hello." '{
  jsonrpc: "2.0", id: "1", method: "SendStreamingMessage",
  params: {message: {messageId: $id, role: "ROLE_USER", parts: [{text: $text}]}}}')
curl_rc=0
code=$(curl -sN --max-time "$timeout" -o "$tmp/stream.sse" -w '%{http_code}' \
  -X POST "$coder/" \
  -H "Authorization: Bearer $token" -H 'Content-Type: application/json' \
  -H "A2A-Extensions: $steps_ext" \
  -d "$rpc") || curl_rc=$?
if [ "$curl_rc" = 28 ]; then
  bad "the task did not end within ${timeout}s"
elif [ "$curl_rc" != 0 ]; then
  bad "SendStreamingMessage: curl exit $curl_rc"
elif [ "$code" != 200 ]; then
  bad "SendStreamingMessage: HTTP $code: $(head -c 300 "$tmp/stream.sse")"
else
  ok "the task was delivered (HTTP 200)"
fi

# One JSON-RPC response per `data:` line, then the steps each status message carries.
sed -n 's/^data: //p' "$tmp/stream.sse" 2>/dev/null | jq -c '.' > "$tmp/events.jsonl" 2>/dev/null || : > "$tmp/events.jsonl"
rpc_error=$(jq -r 'select(.error) | .error | "\(.code): \(.message)"' "$tmp/events.jsonl" 2>/dev/null | head -n 1 || true)
if [ -n "$rpc_error" ]; then bad "the server answered with a JSON-RPC error: $rpc_error"; fi
jq -c --arg u "$steps_ext" '.. | objects | (.metadata? // empty) | .[$u]? // empty' \
  "$tmp/events.jsonl" > "$tmp/steps.jsonl" 2>/dev/null || : > "$tmp/steps.jsonl"

# --- the step of the push was denied ---------------------------------------------
push_step=$(jq -c --arg id "tool:$push_call" 'select(.id == $id)' "$tmp/steps.jsonl" 2>/dev/null | tail -n 1 || true)
if [ -z "$push_step" ]; then
  bad "no step 'tool:$push_call' in the stream: the scripted commit_and_push was never called (is the stack up with the mock-coder model?)"
else
  state=$(printf '%s' "$push_step" | jq -r '.state // empty')
  if [ "$state" = failed ]; then
    ok "the step of commit_and_push failed instead of running"
  else
    bad "the step of commit_and_push is '${state:-absent}', want failed: the hook did not deny it (start the stack with -f dev/compose.hooks.yaml)"
  fi
  text=$(printf '%s' "$push_step" | jq -r '.output.text // empty')
  case $text in
    *"$reason"*)
      ok "the step carries the hook program's denial ('$reason')"
      ;;
    *)
      bad "the step's output does not carry the denial '$reason': ${text:-<none>}"
      ;;
  esac
fi

# --- nothing was pushed or opened -------------------------------------------------
# The scripted model's next call after the denied push is `open_pull_request`, and it is refused
# before any request leaves the coder: `publish::open_in` returns as soon as `notes.pushed_sha` is
# not HEAD (nothing was pushed). So "no POST /pulls" holds whatever the mock's script says next,
# and it would only change if that early return went away.
found=$tmp/pulls.json
curl -s --max-time 30 -X POST "$github/__admin/requests/find" \
  -H 'Content-Type: application/json' \
  -d "{\"method\":\"POST\",\"urlPath\":\"/repos/$repo/pulls\"}" > "$found" || true
posts=$(jq -r '.requests | length' "$found" 2>/dev/null || echo '?')
if [ "$posts" = 0 ]; then ok "mock-github saw no POST /repos/$repo/pulls"; else bad "mock-github saw $posts POST /repos/$repo/pulls, want none"; fi

heads_after=$(git ls-remote --heads "$gitserver/$repo.git" 2>/dev/null | sort || true)
if [ -n "$heads_before" ] && [ "$heads_before" = "$heads_after" ]; then
  ok "git-server's heads of $repo did not move: nothing was pushed"
elif [ -z "$heads_after" ]; then
  bad "git-server does not answer for $repo after the task"
else
  bad "git-server's heads of $repo changed: before [$(printf '%s' "$heads_before" | tr '\n' ' ')], after [$(printf '%s' "$heads_after" | tr '\n' ' ')]"
fi

if [ "$fail" != 0 ]; then
  echo "--- the stream (first 200 lines of text) ---"
  jq -r '.. | .text? // empty' "$tmp/events.jsonl" 2>/dev/null | head -n 200 || true
fi
exit "$fail"
