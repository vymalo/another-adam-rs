#!/bin/sh
# End-to-end test of the coder against the compose mocks: one A2A task that
# must end in a branch on git-server and one pull request on mock-github.
#
#   dev/coder-e2e.sh                  # OpenCode (model mock-opencode) makes the change
#   NO_OPENCODE=1 dev/coder-e2e.sh    # the check command makes it; OpenCode is not started
#   SCENARIO=files dev/coder-e2e.sh   # the coder reads and writes the files itself; no OpenCode
#   SCENARIO=scratch dev/coder-e2e.sh # no repository is named: a scratch project, then published
#   GITHUB_AUTH=app dev/coder-e2e.sh  # the stack runs the coder as a GitHub App (see below)
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
#   * SCENARIO=files also: the stream says `read README.md (sandbox)` and `wrote hello.txt (sandbox)`
#     (the coder's file tools ran, in the slot of the repository) and never `starting OpenCode`.
#   * SCENARIO=scratch is two messages. The task names no repository, so the coder builds `fib.sh`
#     in a scratch project and asks where to put it (the task is TASK_STATE_INPUT_REQUIRED, with the
#     question; nothing was pushed or opened, and git-server has not even heard of the repository).
#     The answer, "Publish it to <the repository>" (the scratch owner of git-server, which makes an
#     empty repository on first use, as a repository just created on GitHub is), is sent to the task;
#     then every check above holds for `scratch/fib-<id>` (the slot is called after it), and also:
#     `main` is one commit with the empty tree (the first commit the coder gave the empty
#     repository), `fib.sh` is on the branch, and the last `checks` artifact is bound to the pushed
#     commit with the tree the checks ran on in the scratch project (the code that was checked is
#     the code that was pushed).
#   * the coder's GitHub credentials, as the stack was started with them (GITHUB_AUTH):
#       token (default): every call the coder made to mock-github's `/repos/...` carried
#         `Authorization: Bearer dev-github-token` (MOCK_GITHUB_TOKEN, the compose file's dummy);
#       app: the stack was started with `-f dev/compose.github-app.yaml`, the coder holds a GitHub
#         App's key and no token: mock-github's journal holds at least one
#         `POST /app/installations/67890/access_tokens` (the trade of a signed JWT for a token), and
#         every call to `/repos/...`, the pull request's included, carried the installation token it
#         gave, `Bearer ghs_mockinstallationtoken...`, and never the JWT (`Bearer eyJ...`).
# It prints one "ok" or "FAIL" line per check and exits 1 if any failed.
#
# Environment (defaults match compose.yaml on one machine):
#   CODER_URL        http://127.0.0.1:${CODER_PORT:-8080}
#   CODER_TOKEN      dev-token
#   MOCK_GITHUB_URL  http://127.0.0.1:${MOCK_GITHUB_PORT:-8082}
#   GIT_SERVER_URL   http://127.0.0.1:${GIT_SERVER_PORT:-8083}   (from the host)
#   TIMEOUT          300     seconds to wait for the task to end
#   NO_OPENCODE      unset   1 = the [mock:no-opencode] script (no OpenCode)
#   SCENARIO         default `default` (the script above), `files` (the [mock:files] script:
#                    read_file, write_file; every check above holds, and so do the two lines) or
#                    `scratch` (the [mock:scratch] script, above)
#   REPO_BASE_URL    http://git-server:8080   where the coder (inside the compose network) finds git-server
#   GITHUB_AUTH      token   how the stack was started: `token` or `app` (see the assertions above)
#   MOCK_GITHUB_TOKEN dev-github-token   the token of `token` mode (compose.yaml's `${MOCK_GITHUB_TOKEN-dev-github-token}`)
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
# The address the coder (inside the compose network) uses for git-server, and for the repository.
repo_base=${REPO_BASE_URL:-http://git-server:8080}
repo_base=${repo_base%/}
repo_url=$repo_base/$repo_path.git

fail=0
ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fail=1; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

github_auth=${GITHUB_AUTH:-token}
case "$github_auth" in
  token | app) ;;
  *) echo "GITHUB_AUTH must be token or app, not '$github_auth'" >&2; exit 2 ;;
esac
scenario=${SCENARIO:-default}
case "$scenario" in
  default | files | scratch) ;;
  *) echo "SCENARIO must be default, files or scratch, not '$scenario'" >&2; exit 2 ;;
esac
if [ "$scenario" != default ] && [ "${NO_OPENCODE:-}" = 1 ]; then
  echo "NO_OPENCODE=1 and SCENARIO=$scenario are two different scripts: set one" >&2
  exit 2
fi

text="In $repo_url (base branch main), add hello.txt containing hello."
slot=${repo_path##*/}
if [ "$scenario" = scratch ]; then
  # A repository of this run's own (the stack keeps its repositories between runs, and the mock
  # model takes the name from the task text): `scratch` is the owner git-server makes repositories
  # for on first use. The task names none; the person names this one when the coder asks.
  name=fib-$(printf '%x%x' "$(date +%s)" "$$")
  repo_path=scratch/$name
  repo_url=$repo_base/$repo_path.git
  slot=$name
  text="Write a fib.sh that prints the first 7 Fibonacci numbers. I'll give you the repo later. [mock:scratch] $name"
  echo "variant: no repository is named, a scratch project is published to $repo_path ([mock:scratch])"
elif [ "$scenario" = files ]; then
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
run_id="e2e-$scenario-${NO_OPENCODE:-0}-$(date +%s)-$$"

# send_message <number> <text> [task id]: SendStreamingMessage, read until the server closes the
# stream (the task reached a final state, or it waits for the person: the --max-time is the
# TIMEOUT), into $tmp/stream-<number>.sse and, one JSON-RPC response per line, $tmp/events-<number>.jsonl.
# A message with a task id goes to that task: it is the answer to the question the task waits on.
send_message() {
  rpc=$(jq -n --arg id "$run_id-$1" --arg text "$2" --arg task "${3:-}" '{
    jsonrpc: "2.0", id: "1", method: "SendStreamingMessage",
    params: {message: ({messageId: $id, role: "ROLE_USER", parts: [{text: $text}]}
      + (if $task == "" then {} else {taskId: $task} end))}}')
  curl_rc=0
  code=$(curl -sN --max-time "$timeout" -o "$tmp/stream-$1.sse" -w '%{http_code}' \
    -X POST "$coder/" \
    -H "Authorization: Bearer $token" -H 'Content-Type: application/json' \
    -H "A2A-Extensions: $text_stream" \
    -d "$rpc") || curl_rc=$?
  if [ "$curl_rc" = 28 ]; then
    bad "the task did not end within ${timeout}s"
  elif [ "$curl_rc" != 0 ]; then
    bad "SendStreamingMessage: curl exit $curl_rc"
  elif [ "$code" != 200 ]; then
    bad "SendStreamingMessage: HTTP $code: $(head -c 300 "$tmp/stream-$1.sse")"
  fi
  # Every event is one `data: <json-rpc response>` line.
  sed -n 's/^data: //p' "$tmp/stream-$1.sse" 2>/dev/null | jq -c '.' > "$tmp/events-$1.jsonl" 2>/dev/null || : > "$tmp/events-$1.jsonl"
  rpc_error=$(jq -r 'select(.error) | .error | "\(.code): \(.message)"' "$tmp/events-$1.jsonl" | head -n 1)
  if [ -n "$rpc_error" ]; then bad "the server answered with a JSON-RPC error: $rpc_error"; fi
}

# listed <name.git>: how often git-server's listing of the scratch owner (`/__repos/scratch/`, JSON)
# has it; 0 when the owner has no repository at all yet (the listing is a 404 until one is made).
listed() {
  listing_code=$(curl -s -o "$tmp/listing.json" -w '%{http_code}' --max-time 30 "$gitserver/__repos/scratch/" || true)
  case "$listing_code" in
    404) echo 0 ;;
    200) jq -r --arg n "$1" '[.[]? | select(.name == $n)] | length' "$tmp/listing.json" 2>/dev/null || echo '?' ;;
    *) echo '?' ;;
  esac
}

# The state of the last status update (or of the task itself) of an event file.
state_of() {
  jq -r '(.result.statusUpdate.status.state // .result.task.status.state) // empty' "$1" | tail -n 1
}

# --- send the task ---------------------------------------------------------------
send_message 1 "$text"
events=$tmp/events-1.jsonl
every_event=$tmp/events-1.jsonl

if [ "$scenario" = scratch ]; then
  # The task names no repository: the coder builds the project, then asks where to put it, and the
  # task waits for the answer. Until then nothing may have left the coder: no push, no pull
  # request, and git-server has not even been asked for the repository.
  state=$(state_of "$events")
  if [ "$state" = TASK_STATE_INPUT_REQUIRED ]; then ok "the task waits for the person (TASK_STATE_INPUT_REQUIRED)"; else bad "the task is '${state:-none}', want TASK_STATE_INPUT_REQUIRED"; fi
  jq -r '.. | .text? // empty' "$events" > "$tmp/lines-1.txt" 2>/dev/null || : > "$tmp/lines-1.txt"
  if grep -q 'which repository should I publish it to' "$tmp/lines-1.txt"; then ok "the coder asks which repository to publish it to"; else bad "the stream has no question about where to publish"; fi
  # The lines of the first steps (`wrote fib.sh (fib)`) are progress, which is not kept: the stream
  # attaches to it after the task is made, and the first tool calls can be done before it does.
  # What they made is checked below, on the pushed branch.
  posts=$(curl -s --max-time 30 -X POST "$github/__admin/requests/find" -H 'Content-Type: application/json' \
    -d '{"method":"POST","urlPathPattern":"/repos/.*/pulls"}' | jq -r '.requests | length' 2>/dev/null || echo '?')
  if [ "$posts" = 0 ]; then ok "no pull request was opened before the person named a repository"; else bad "$posts pull request(s) were opened before the person named a repository"; fi
  known=$(listed "$name.git")
  if [ "$known" = 0 ]; then ok "git-server has not been asked for $repo_path yet"; else bad "git-server knows $repo_path before the person named it ($known)"; fi
  task_id=$(jq -r '(.result.task.id // .result.statusUpdate.taskId // .result.artifactUpdate.taskId) // empty' "$events" | head -n 1)
  if [ -z "$task_id" ]; then
    bad "the stream does not say which task it is"
  else
    # The person names the repository, in their own words: that is what lets the coder publish there.
    send_message 2 "Publish it to $repo_url" "$task_id"
    events=$tmp/events-2.jsonl
    every_event=$tmp/events-all.jsonl
    cat "$tmp/events-1.jsonl" "$tmp/events-2.jsonl" > "$every_event"
  fi
fi

state=$(state_of "$events")
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
if [ "$scenario" = files ] || [ "$scenario" = scratch ]; then
  lines=$tmp/lines.txt
  jq -r '.. | .text? // empty' "$every_event" > "$lines" 2>/dev/null || : > "$lines"
  if [ "$scenario" = files ]; then
    if grep -qx "read README.md ($slot)" "$lines"; then ok "the coder read README.md itself"; else bad "no 'read README.md ($slot)' line in the stream"; fi
    if grep -qx "wrote hello.txt ($slot)" "$lines"; then ok "the coder wrote hello.txt itself"; else bad "no 'wrote hello.txt ($slot)' line in the stream"; fi
  else
    # The repository was given its first commit, and the project's files were copied from the scratch
    # slot `fib` into the repository's slot, which is called after it.
    if grep -q "^giving .*$name.git its first commit on main\$" "$lines"; then ok "the empty repository was given its first commit on main"; else bad "no 'giving $repo_url its first commit on main' line in the stream"; fi
    if grep -qx "copying fib into $slot (.)" "$lines"; then ok "the project was copied into the slot of the repository"; else bad "no 'copying fib into $slot (.)' line in the stream"; fi
  fi
  if grep -q 'starting OpenCode' "$lines"; then bad "OpenCode was started, and this script does not delegate"; else ok "OpenCode was not started"; fi
fi

# --- artifacts ---------------------------------------------------------------------
artifact() { # artifact <name> <jq path under .parts[0].data> -> value ("" if absent)
  jq -r --arg n "$1" "select(.result.artifactUpdate.artifact.name == \$n) | .result.artifactUpdate.artifact.parts[0].data | $2 // empty" "$every_event" | tail -n 1
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

# --- the coder's GitHub credentials -----------------------------------------------------
# Every call the coder made to the repositories' API in this run (the journal was reset at the
# start): which `Authorization` it carried.
repo_calls=$tmp/repo-calls.json
curl -s --max-time 30 -X POST "$github/__admin/requests/find" \
  -H 'Content-Type: application/json' \
  -d '{"urlPathPattern":"/repos/.*"}' > "$repo_calls" || true
n_calls=$(jq -r '.requests | length' "$repo_calls" 2>/dev/null || echo 0)
auths=$(jq -r '.requests[] | .headers | with_entries(.key |= ascii_downcase) | .authorization // "none"' "$repo_calls" 2>/dev/null | sort | uniq -c | sed 's/^ *//' | tr '\n' ';')
case "$github_auth" in
  token)
    want="Bearer ${MOCK_GITHUB_TOKEN-dev-github-token}"
    wrong=$(jq -r --arg want "$want" '[.requests[] | .headers | with_entries(.key |= ascii_downcase) | select(.authorization != $want)] | length' "$repo_calls" 2>/dev/null || echo '?')
    if [ "$n_calls" -ge 1 ] && [ "$wrong" = 0 ]; then ok "all $n_calls call(s) to the repositories' API carried the token"; else bad "token mode: $wrong of $n_calls call(s) to /repos/... did not carry '$want' ($auths)"; fi
    ;;
  app)
    mints=$(curl -s --max-time 30 -X POST "$github/__admin/requests/find" \
      -H 'Content-Type: application/json' \
      -d '{"method":"POST","urlPath":"/app/installations/67890/access_tokens"}' | jq -r '.requests | length' 2>/dev/null || echo '?')
    if [ "$mints" != '?' ] && [ "$mints" -ge 1 ]; then ok "the coder traded a JWT for an installation token ($mints POST /app/installations/67890/access_tokens)"; else bad "mock-github saw $mints POST /app/installations/67890/access_tokens, want at least 1 (is the stack started with -f dev/compose.github-app.yaml?)"; fi
    wrong=$(jq -r '[.requests[] | .headers | with_entries(.key |= ascii_downcase) | select((.authorization // "") | startswith("Bearer ghs_mockinstallationtoken") | not)] | length' "$repo_calls" 2>/dev/null || echo '?')
    if [ "$n_calls" -ge 1 ] && [ "$wrong" = 0 ]; then ok "all $n_calls call(s) to the repositories' API carried the installation token"; else bad "app mode: $wrong of $n_calls call(s) to /repos/... did not carry 'Bearer ghs_mockinstallationtoken...' ($auths)"; fi
    ;;
esac

# --- git-server ---------------------------------------------------------------------
if [ -n "$branch" ]; then
  remote=$(git ls-remote --heads "$gitserver/$repo_path.git" "refs/heads/$branch" 2>/dev/null || true)
  if [ -n "$remote" ]; then ok "git-server has the branch $branch"; else bad "git-server does not have the branch $branch"; fi
  if [ -n "$remote" ] && [ -n "$commit" ] && [ "${remote%%[[:space:]]*}" != "$commit" ]; then
    bad "the branch is at ${remote%%[[:space:]]*}, the artifact says $commit"
  fi
  if git clone -q --depth 1 --branch "$branch" "$gitserver/$repo_path.git" "$tmp/clone" 2>"$tmp/clone.err"; then
    if [ "$scenario" = scratch ]; then
      content=$(cat "$tmp/clone/fib.sh" 2>/dev/null || echo '<missing>')
      if [ "$content" = 'echo 0 1 1 2 3 5 8' ]; then ok "fib.sh on the branch is the project's"; else bad "fib.sh on the branch is '$content', want 'echo 0 1 1 2 3 5 8'"; fi
      if [ "$(sh "$tmp/clone/fib.sh" 2>/dev/null)" = '0 1 1 2 3 5 8' ]; then ok "fib.sh prints the first 7 Fibonacci numbers"; else bad "fib.sh does not print '0 1 1 2 3 5 8'"; fi
    else
      content=$(cat "$tmp/clone/hello.txt" 2>/dev/null || echo '<missing>')
      if [ "$content" = hello ]; then ok "hello.txt on the branch is 'hello'"; else bad "hello.txt on the branch is '$content', want 'hello'"; fi
    fi
  else
    bad "cannot clone the branch: $(head -c 300 "$tmp/clone.err")"
  fi
else
  bad "no branch to look for on git-server"
fi

# The repository was empty: the coder gave it a first commit, of nothing, to be the base of the
# pull request, and that is all `main` holds (the work went to the branch above, behind the gate).
if [ "$scenario" = scratch ]; then
  if git clone -q --branch main "$gitserver/$repo_path.git" "$tmp/main" 2>"$tmp/main.err"; then
    root_tree=$(git -C "$tmp/main" rev-parse 'HEAD^{tree}' 2>/dev/null || true)
    n_commits=$(git -C "$tmp/main" rev-list --count HEAD 2>/dev/null || echo '?')
    if [ "$root_tree" = 4b825dc642cb6eb9a060e54bf8d69288fbee4904 ] && [ "$n_commits" = 1 ]; then ok "main is the one empty commit the coder gave the new repository"; else bad "main has $n_commits commit(s) and the tree '$root_tree', want the one empty commit"; fi
  else
    bad "cannot clone main: $(head -c 300 "$tmp/main.err")"
  fi
  # What was checked is what was pushed: the tree of the last `checks` artifact is the tree of the
  # pushed commit (the checks ran in the scratch project, and the files landed unchanged).
  if [ -n "$commit" ]; then
    pushed_tree=$(git -C "$tmp/clone" rev-parse 'HEAD^{tree}' 2>/dev/null || true)
    if [ -n "$pushed_tree" ] && [ "$pushed_tree" = "$checks_tree" ]; then ok "the tree that was checked in the scratch project is the tree that was pushed"; else bad "the pushed tree is '$pushed_tree', the checks ran on '$checks_tree'"; fi
  fi
  known=$(listed "$name.git")
  if [ "$known" = 1 ]; then ok "git-server lists $repo_path now"; else bad "git-server lists $repo_path $known time(s), want 1"; fi
fi

if [ "$fail" -eq 0 ]; then echo "coder e2e passed"; else echo "coder e2e FAILED"; exit 1; fi
