#!/bin/sh
# End-to-end test of the coder against the compose mocks: one A2A task that
# must end in a branch on git-server and one pull request on mock-github.
#
#   dev/coder-e2e.sh                  # OpenCode (model mock-opencode) makes the change
#   NO_OPENCODE=1 dev/coder-e2e.sh    # the check command makes it; OpenCode is not started
#   SCENARIO=files dev/coder-e2e.sh   # the coder reads and writes the files itself; no OpenCode
#
# Start the stack first (the coder's models are the scripts in
# dev/wiremock/mock-openai/mappings/coder-script.json and opencode-script.json):
#
#   docker compose --profile app up -d --build --wait \
#     postgres mock-openai mock-github git-server coder
#
# The script resets mock-github's request journal, sends the task with
# SendStreamingMessage (activating `text-stream/v1`, which the coder's card lists), reads the
# stream until the task ends, and checks:
#   * the task is TASK_STATE_COMPLETED (not FAILED, and it ends within TIMEOUT);
#   * the coder's last answer arrived as at least two `reply` chunks, before the status that ends
#     the task, each beginning where the one before ended (UTF-8 bytes), the last marked, that add
#     up to the text of that status, which names the stream (`metadata.streamId`);
#   * the `checks`, `branch` and `pull_request` artifacts are there, and the last `checks`
#     (bound to the pushed commit, emitted before `branch`) passed on exactly the
#     branch artifact's commit, with a 40-hex tree;
#   * mock-github saw exactly one POST /repos/local/sandbox/pulls, head = the
#     branch, base = main;
#   * git-server has the branch, and hello.txt on it is `hello`;
#   * SCENARIO=files also: the stream says `read README.md` and `wrote hello.txt` (the coder's file
#     tools ran) and never `starting OpenCode`.
# It prints one "ok" or "FAIL" line per check and exits 1 if any failed.
#
# Environment (defaults match compose.yaml on one machine):
#   CODER_URL        http://127.0.0.1:${CODER_PORT:-8080}
#   CODER_TOKEN      dev-token
#   MOCK_GITHUB_URL  http://127.0.0.1:${MOCK_GITHUB_PORT:-8082}
#   GIT_SERVER_URL   http://127.0.0.1:${GIT_SERVER_PORT:-8083}   (from the host)
#   TIMEOUT          300     seconds to wait for the task to end
#   NO_OPENCODE      unset   1 = the [mock:no-opencode] script (no OpenCode)
#   SCENARIO         default `default` (the script above) or `files` (the [mock:files] script:
#                    read_file, write_file; every check above holds, and so do the two lines)
#
# Needs curl, jq and git. Verified by CI only in the compose run of
# .github/workflows/coder.yml; the mock scripts were also run against the real
# adam-coder binary and OpenCode 1.18.33 without docker.
set -eu

coder=${CODER_URL:-http://127.0.0.1:${CODER_PORT:-8080}}
coder=${coder%/}
token=${CODER_TOKEN:-dev-token}
github=${MOCK_GITHUB_URL:-http://127.0.0.1:${MOCK_GITHUB_PORT:-8082}}
github=${github%/}
gitserver=${GIT_SERVER_URL:-http://127.0.0.1:${GIT_SERVER_PORT:-8083}}
gitserver=${gitserver%/}
timeout=${TIMEOUT:-300}
repo_path=local/sandbox
# The extension that makes the coder send its answer as it is written (its card lists it).
text_stream=https://agents.vymalo.com/a2a/extensions/text-stream/v1
# The address the coder (inside the compose network) uses for the repository.
repo_url=http://git-server:8080/$repo_path.git

fail=0
ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fail=1; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
stream=$tmp/stream.sse
: > "$stream"

scenario=${SCENARIO:-default}
case "$scenario" in
  default | files) ;;
  *) echo "SCENARIO must be default or files, not '$scenario'" >&2; exit 2 ;;
esac
if [ "$scenario" != default ] && [ "${NO_OPENCODE:-}" = 1 ]; then
  echo "NO_OPENCODE=1 and SCENARIO=$scenario are two different scripts: set one" >&2
  exit 2
fi

text="In $repo_url (base branch main), add hello.txt containing hello."
if [ "$scenario" = files ]; then
  text="$text [mock:files]"
  echo "variant: the coder edits the files itself ([mock:files])"
elif [ "${NO_OPENCODE:-}" = 1 ]; then
  text="$text [mock:no-opencode]"
  echo "variant: no OpenCode ([mock:no-opencode])"
else
  echo "variant: OpenCode makes the change"
fi

# --- reset the journal, then send the task ------------------------------------
code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 30 -X DELETE "$github/__admin/requests" || true)
if [ "$code" = 200 ]; then ok "mock-github journal reset"; else bad "mock-github journal reset: HTTP $code"; fi

# The message id names the task (same agent, no context: same id, same task), so
# it must differ between runs, including two variants started in one second.
message_id="e2e-$scenario-${NO_OPENCODE:-0}-$(date +%s)-$$"
rpc=$(jq -n --arg id "$message_id" --arg text "$text" '{
  jsonrpc: "2.0", id: "1", method: "SendStreamingMessage",
  params: {message: {messageId: $id, role: "ROLE_USER", parts: [{text: $text}]}}}')

# The server closes the stream when the task reaches a final state, so curl
# returns then; --max-time is the TIMEOUT.
curl_rc=0
code=$(curl -sN --max-time "$timeout" -o "$stream" -w '%{http_code}' \
  -X POST "$coder/" \
  -H "Authorization: Bearer $token" -H 'Content-Type: application/json' \
  -H "A2A-Extensions: $text_stream" \
  -d "$rpc") || curl_rc=$?
if [ "$curl_rc" = 28 ]; then
  bad "the task did not end within ${timeout}s"
elif [ "$curl_rc" != 0 ]; then
  bad "SendStreamingMessage: curl exit $curl_rc"
elif [ "$code" != 200 ]; then
  bad "SendStreamingMessage: HTTP $code: $(head -c 300 "$stream")"
fi

# --- read the stream -----------------------------------------------------------
# Every event is one `data: <json-rpc response>` line.
events=$tmp/events.jsonl
sed -n 's/^data: //p' "$stream" | jq -c '.' > "$events" 2>/dev/null || true

rpc_error=$(jq -r 'select(.error) | .error | "\(.code): \(.message)"' "$events" | head -n 1)
if [ -n "$rpc_error" ]; then bad "the server answered with a JSON-RPC error: $rpc_error"; fi

# The state of the last status update (or of the task itself).
state=$(jq -r '(.result.statusUpdate.status.state // .result.task.status.state) // empty' "$events" | tail -n 1)
case "$state" in
  TASK_STATE_COMPLETED) ok "the task ended TASK_STATE_COMPLETED" ;;
  TASK_STATE_FAILED)
    bad "the task ended TASK_STATE_FAILED: $(jq -r 'select(.result.statusUpdate.status.state == "TASK_STATE_FAILED") | [.. | .text? // empty] | join(" ")' "$events" | head -c 600)"
    ;;
  *) bad "the task did not complete (last state: '${state:-none}')" ;;
esac

# --- the answer, as it was written ---------------------------------------------------------
# The last answer of the script is a text the mock model dribbles over about two seconds: the
# coder sends it as `reply` chunks, and the status that ends the task says it whole.
all=$tmp/all.json
jq -s '.' "$events" > "$all" 2>/dev/null || echo '[]' > "$all"
chunks=$(jq '[.[] | select(.result.artifactUpdate.artifact.name == "reply") | .result.artifactUpdate]' "$all")
n_chunks=$(printf '%s' "$chunks" | jq 'length')
if [ "$n_chunks" -ge 2 ]; then ok "the answer arrived as $n_chunks chunks"; else bad "the answer arrived as $n_chunks chunk(s), want at least 2"; fi
done_text=$(jq -r '[.[] | select(.result.statusUpdate.status.state == "TASK_STATE_COMPLETED")] | last | .result.statusUpdate.status.message.parts[0].text // empty' "$all")
joined=$(printf '%s' "$chunks" | jq -r 'map(.artifact.parts[0].text) | join("")')
if [ -n "$done_text" ] && [ "$joined" = "$done_text" ]; then ok "the chunks add up to the text of the status that ends the task"; else bad "the chunks say '$joined', the status that ends the task says '$done_text'"; fi
# Each chunk begins where the one before ended, in UTF-8 bytes, and only the last one ends the stream.
chain=$(printf '%s' "$chunks" | jq -r '
  reduce .[] as $c ({at: 0, bad: 0};
    .bad += (if ($c.artifact.metadata["'"$text_stream"'"].offset == .at) then 0 else 1 end)
    | .at += ($c.artifact.parts[0].text | utf8bytelength)) | .bad')
if [ "$chain" = 0 ]; then ok "every chunk begins where the one before ended"; else bad "$chain chunk(s) do not begin where the one before ended"; fi
ends=$(printf '%s' "$chunks" | jq '[.[] | select(.lastChunk == true)] | length')
last_is_last=$(printf '%s' "$chunks" | jq '(last | .lastChunk) == true')
if [ "$ends" = 1 ] && [ "$last_is_last" = true ]; then ok "only the last chunk ends the stream"; else bad "$ends chunk(s) end the stream, and the last one does ${last_is_last}"; fi
stream_id=$(printf '%s' "$chunks" | jq -r '.[0].artifact.artifactId // empty')
said_id=$(jq -r --arg u "$text_stream" '[.[] | select(.result.statusUpdate.status.state == "TASK_STATE_COMPLETED")] | last | .result.statusUpdate.status.message.metadata[$u].streamId // empty' "$all")
if [ -n "$stream_id" ] && [ "$stream_id" = "$said_id" ]; then ok "the status that ends the task names the stream of the chunks"; else bad "the stream of the chunks is '$stream_id', the status that ends the task names '$said_id'"; fi
# All the chunks come before that status.
order=$(jq '([to_entries[] | select(.value.result.artifactUpdate.artifact.name == "reply") | .key] | max)
  < ([to_entries[] | select(.value.result.statusUpdate.status.state == "TASK_STATE_COMPLETED") | .key] | max)' "$all")
if [ "$order" = true ]; then ok "the chunks came before the end of the task"; else bad "a chunk came after the status that ends the task"; fi

# --- the file tools (SCENARIO=files) -------------------------------------------------
# A client that did not activate `steps/v1` reads a tool's progress as lines of text.
if [ "$scenario" = files ]; then
  lines=$tmp/lines.txt
  jq -r '.. | .text? // empty' "$events" > "$lines" 2>/dev/null || : > "$lines"
  if grep -qx 'read README.md' "$lines"; then ok "the coder read README.md itself"; else bad "no 'read README.md' line in the stream"; fi
  if grep -qx 'wrote hello.txt' "$lines"; then ok "the coder wrote hello.txt itself"; else bad "no 'wrote hello.txt' line in the stream"; fi
  if grep -q 'starting OpenCode' "$lines"; then bad "OpenCode was started, and this script does not delegate"; else ok "OpenCode was not started"; fi
fi

# --- artifacts ---------------------------------------------------------------------
artifact() { # artifact <name> <jq path under .parts[0].data> -> value ("" if absent)
  jq -r --arg n "$1" "select(.result.artifactUpdate.artifact.name == \$n) | .result.artifactUpdate.artifact.parts[0].data | $2 // empty" "$events" | tail -n 1
}
checks_passed=$(artifact checks '.passed | tostring')
checks_commit=$(artifact checks .commit)
checks_tree=$(artifact checks .tree)
branch=$(artifact branch .branch)
commit=$(artifact branch .commit)
pr_url=$(artifact pull_request .url)
pr_branch=$(artifact pull_request .branch)
if [ "$checks_passed" = true ]; then ok "checks artifact: passed"; else bad "checks artifact: passed is '${checks_passed:-absent}', want true"; fi
# The last checks artifact is the one bound to the pushed commit.
if printf '%s' "$checks_commit" | grep -Eq '^[0-9a-f]{40}$' && [ "$checks_commit" = "$commit" ]; then ok "the last checks artifact is bound to the pushed commit $(printf '%s' "$checks_commit" | cut -c1-10)"; else bad "the last checks artifact commit '$checks_commit' is not the pushed commit '$commit'"; fi
if printf '%s' "$checks_tree" | grep -Eq '^[0-9a-f]{40}$'; then ok "checks artifact names the tree $(printf '%s' "$checks_tree" | cut -c1-10)"; else bad "checks artifact tree '$checks_tree' is not a 40-hex tree id"; fi
if [ -n "$branch" ] && [ -n "$commit" ]; then ok "branch artifact: $branch at $(printf '%s' "$commit" | cut -c1-10)"; else bad "no branch artifact (with a commit)"; fi
if [ -n "$pr_url" ]; then ok "pull_request artifact: $pr_url"; else bad "no pull_request artifact (with a url)"; fi
if [ -n "$branch" ] && [ "$pr_branch" = "$branch" ]; then ok "the pull request is for the pushed branch"; else bad "pull_request branch '$pr_branch' is not the pushed branch '$branch'"; fi

# --- mock-github's journal -------------------------------------------------------------
found=$tmp/found.json
curl -s --max-time 30 -X POST "$github/__admin/requests/find" \
  -H 'Content-Type: application/json' \
  -d "{\"method\":\"POST\",\"urlPath\":\"/repos/$repo_path/pulls\"}" > "$found" || true
posts=$(jq -r '.requests | length' "$found" 2>/dev/null || echo '?')
if [ "$posts" = 1 ]; then ok "mock-github saw exactly one POST /repos/$repo_path/pulls"; else bad "mock-github saw $posts POST /repos/$repo_path/pulls, want exactly 1"; fi
head_ref=$(jq -r '.requests[0].body | fromjson | .head // empty' "$found" 2>/dev/null || true)
base_ref=$(jq -r '.requests[0].body | fromjson | .base // empty' "$found" 2>/dev/null || true)
# A head may be written `<owner>:<branch>`; only the branch matters.
head_branch=${head_ref##*:}
if [ -n "$branch" ] && [ "$head_branch" = "$branch" ]; then ok "the pull request head is $head_branch"; else bad "the pull request head is '$head_ref', want '$branch'"; fi
if [ "$base_ref" = main ]; then ok "the pull request base is main"; else bad "the pull request base is '$base_ref', want main"; fi

# --- git-server ---------------------------------------------------------------------
if [ -n "$branch" ]; then
  remote=$(git ls-remote --heads "$gitserver/$repo_path.git" "refs/heads/$branch" 2>/dev/null || true)
  if [ -n "$remote" ]; then ok "git-server has the branch $branch"; else bad "git-server does not have the branch $branch"; fi
  if [ -n "$remote" ] && [ -n "$commit" ] && [ "${remote%%[[:space:]]*}" != "$commit" ]; then
    bad "the branch is at ${remote%%[[:space:]]*}, the artifact says $commit"
  fi
  if git clone -q --depth 1 --branch "$branch" "$gitserver/$repo_path.git" "$tmp/clone" 2>"$tmp/clone.err"; then
    content=$(cat "$tmp/clone/hello.txt" 2>/dev/null || echo '<missing>')
    if [ "$content" = hello ]; then ok "hello.txt on the branch is 'hello'"; else bad "hello.txt on the branch is '$content', want 'hello'"; fi
  else
    bad "cannot clone the branch: $(head -c 300 "$tmp/clone.err")"
  fi
else
  bad "no branch to look for on git-server"
fi

if [ "$fail" -eq 0 ]; then echo "coder e2e passed"; else echo "coder e2e FAILED"; exit 1; fi
