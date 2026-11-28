#!/bin/sh
# End-to-end test of `uploads/v1`'s outgoing direction against the compose mocks: the coder writes a
# file over its inline cap, `share_file` asks the thread tools for a slot (`request_upload`), `PUT`s
# the bytes to the bucket, and the task's artifact names the `uploadId` with no bytes in it.
#
#   dev/uploads-e2e.sh
#
# It starts the stack itself, because the cap is a deployment value the coder reads at startup
# (`UPLOADS_INLINE_MAX_BYTES`, 1024 here; the compose default is the code's own 10 MiB):
#
#   UPLOADS_INLINE_MAX_BYTES=1024 docker compose --profile app up -d --wait \
#     postgres mock-openai mock-github mock-github-mcp git-server mock-uploads coder
#
# Steps:
#   1. The card lists `uploads/v1` (with no `params`, as the contract says).
#   2. SendStreamingMessage "[mock:upload] build a report and share it" activates `uploads/v1`
#      (limits in the message's metadata) and `thread-tools/v1` (a grant naming mock-uploads), so
#      the coder's scripted model drives start_scratch -> write_file (report.md, 3.4 KB) ->
#      share_file. Scratch work is deliberate: a shared file is delivery there, so the run
#      completes; in a repository the coder would wait for a pull request instead.
#   3. The task ends TASK_STATE_COMPLETED, and its artifacts hold one part whose metadata (under
#      the uploads/v1 URI) names the uploadId, with the filename and the media type — and no bytes:
#      the part's `data` is empty and it has no `file`.
#   4. The thread tools were asked for a slot with the file's own size and SHA-256.
#   5. The bucket (the same WireMock) saw exactly one PUT, to the slot's path, whose body is
#      byte-for-byte the file the model wrote.
#
# The red proof: `UPLOADS_E2E_TAMPER=1 sh dev/uploads-e2e.sh` appends one byte to the expected text,
# and the last check fails on the SHA-256 (the run itself still succeeds), which is how a check that
# compares the right thing is known to be able to fail.
#
# Environment (defaults match compose.yaml on one machine):
#   CODER_URL            http://127.0.0.1:${CODER_PORT:-8080}
#   CODER_TOKEN          dev-token
#   MOCK_UPLOADS_PORT    8087        the thread tools and the bucket, on 127.0.0.1
#   TIMEOUT              120         seconds to wait for the task to stop
#   UPLOADS_INLINE_MAX_BYTES  1024   the cap this scenario sets on the coder
#
# Needs curl, jq, sha256sum and base64. Run from anywhere: the script moves to the repository root.
set -eu

cd "$(dirname "$0")/.."

coder=${CODER_URL:-http://127.0.0.1:${CODER_PORT:-8080}}
coder=${coder%/}
token=${CODER_TOKEN:-dev-token}
timeout=${TIMEOUT:-120}
uploads_port=${MOCK_UPLOADS_PORT:-8087}
uploads=http://127.0.0.1:$uploads_port

uploads_uri=https://agents.vymalo.com/a2a/extensions/uploads/v1
tools_uri=https://agents.vymalo.com/a2a/extensions/thread-tools/v1

# The grant's token is read, not verified: the coder decodes its payload to learn who is asking
# (`caller`), and a token it cannot read makes the message an agent's — whose words name no
# repository, so prepare_workspace would be refused. A JWT-shaped token saying `main` is what the
# orchestrator sends; here it is unsigned, which is all this test needs.
b64url() { base64 | tr '+/' '-_' | tr -d '=\n'; }
thread_token=$(printf '{"alg":"none"}' | b64url).$(printf '{"caller":"main","depth":0}' | b64url).

fail=0
ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fail=1; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# --- the file the model writes, and the bytes the bucket must hold ------------------------------
# 40 numbered lines: 3,471 bytes, over the 1,024-byte cap the coder is started with.
expected() {
  i=1
  while [ "$i" -le 40 ]; do
    printf '%d: the coder shares this file by reference, and the bucket holds exactly these bytes.\n' "$i"
    i=$((i + 1))
  done
}
expected > "$tmp/expected.md"
if [ "${UPLOADS_E2E_TAMPER:-}" = "1" ]; then
  printf '!' >> "$tmp/expected.md"
fi
size=$(wc -c < "$tmp/expected.md" | tr -d ' ')
sha=$(sha256sum "$tmp/expected.md" | cut -d' ' -f1)

# --- 1. The stack, with the cap this scenario needs ---------------------------------------------
# The two mocks are recreated, not just started: their mappings are mounted files that WireMock
# reads at startup, so an edited script would otherwise be served from the old one.
if UPLOADS_INLINE_MAX_BYTES=${UPLOADS_INLINE_MAX_BYTES:-1024} \
    docker compose --profile app up -d --wait --force-recreate mock-openai mock-uploads \
      postgres mock-github mock-github-mcp git-server coder > "$tmp/up.log" 2>&1; then
  ok "the stack is up, the coder with UPLOADS_INLINE_MAX_BYTES=${UPLOADS_INLINE_MAX_BYTES:-1024}"
else
  bad "the stack did not come up"
  tail -n 20 "$tmp/up.log"
  echo "FAIL 1 check failed"
  exit 1
fi

# --- 2. The card lists uploads/v1 ---------------------------------------------------------------
code=$(curl -s -o "$tmp/card.json" -w '%{http_code}' --max-time 30 "$coder/.well-known/agent-card.json" || true)
if [ "$code" != 200 ]; then
  bad "the agent card: HTTP $code"
elif jq -e --arg u "$uploads_uri" '.capabilities.extensions[]? | select(.uri == $u)' "$tmp/card.json" >/dev/null 2>&1; then
  ok "the card lists uploads/v1"
else
  bad "the card does not list $uploads_uri"
fi

# The journal is this test's alone: an earlier run's PUT must not be read as this one's.
curl -s -o /dev/null -X DELETE "$uploads/__admin/requests" || true

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

message=$(jq -n --arg id "uploads-e2e-$(date +%s)-$$" --arg ctx "uploads-ctx-$(date +%s)-$$" \
  --arg uploads "$uploads_uri" --arg tools "$tools_uri" \
  --arg url "http://mock-uploads:8080/mcp" --arg token "$thread_token" '{
    jsonrpc: "2.0", id: "1", method: "SendStreamingMessage",
    params: {message: {
      messageId: $id, contextId: $ctx, role: "ROLE_USER",
      parts: [{text: "[mock:upload] Build a short report and share it with me as a file."}],
      extensions: [$uploads, $tools],
      metadata: {
        ($uploads): {maxBytes: 1048576, ttlSecs: 300},
        ($tools): {url: $url, token: $token, expiresAt: "2099-01-01T00:00:00Z"}}}}}')

send "$message"

# --- 3. The task completed, and its artifact names the uploadId with no bytes -------------------
state=$(jq -r 'select((.result.statusUpdate.status.state // .result.task.status.state) == "TASK_STATE_COMPLETED") | "completed"' "$tmp/events.jsonl" | tail -n 1)
if [ "$state" = "completed" ]; then
  ok "the task completed"
else
  bad "the task did not complete: $(jq -c 'select(.result.statusUpdate.status.state) | .result.statusUpdate.status' "$tmp/events.jsonl" | tail -n 1)"
fi

# `-s`: the events file is a stream of values, one per line, not an array.
# The A2A part is a file part: the name and the media type beside an empty `data`, and the uploadId
# under the extension's URI. No bytes are anywhere in it.
parts=$(jq -sc --arg ext "$uploads_uri" '[.[] | (.result.artifactUpdate.artifact.parts // [])[] | select(.metadata[$ext].uploadId)]' "$tmp/events.jsonl")
count=$(printf '%s' "$parts" | jq 'length')
if [ "$count" = "1" ]; then
  ok "one upload artifact part"
else
  bad "expected one upload artifact part, got $count"
fi
if [ "$count" = "1" ] &&
   printf '%s' "$parts" | jq -e --arg ext "$uploads_uri" '.[0].metadata[$ext].uploadId == "up_e2e_0001"' >/dev/null 2>&1; then
  ok "the artifact names up_e2e_0001"
else
  bad "the artifact does not name the upload: $(printf '%s' "$parts" | jq -c --arg ext "$uploads_uri" '.[0].metadata[$ext]' 2>/dev/null)"
fi
if [ "$count" = "1" ] &&
   printf '%s' "$parts" | jq -e '.[0].filename == "report.md" and .[0].mediaType == "text/markdown"' >/dev/null 2>&1; then
  ok "the artifact is report.md, text/markdown"
else
  bad "the artifact is $(printf '%s' "$parts" | jq -c '[.[0].filename, .[0].mediaType]' 2>/dev/null)"
fi
if [ "$count" = "1" ] &&
   printf '%s' "$parts" | jq -e '.[0].data == {} and (.[0] | has("file") | not)' >/dev/null 2>&1; then
  ok "the artifact carries no bytes: data is empty and there is no file"
else
  bad "the artifact carries bytes: $(printf '%s' "$parts" | jq -c '.[0] | {data, file}' 2>/dev/null)"
fi

# --- 4. The thread tools were asked for the file's own size and SHA-256 -------------------------
journal=$(curl -sS --max-time 10 "$uploads/__admin/requests" || true)
asked=$(printf '%s' "$journal" | jq -c '[.requests[] | select(.request.method == "POST") | select((.request.body // "") | contains("request_upload"))] | last // empty')
if [ -n "$asked" ]; then
  asked_size=$(printf '%s' "$asked" | jq -r '.request.body | fromjson | .params.arguments.size')
  asked_sha=$(printf '%s' "$asked" | jq -r '.request.body | fromjson | .params.arguments.sha256')
  if [ "$asked_size" = "$size" ] && [ "$asked_sha" = "$sha" ]; then
    ok "request_upload asked for $size bytes with the file's SHA-256"
  else
    bad "request_upload asked for size=$asked_size sha=$asked_sha, expected size=$size sha=$sha"
  fi
else
  bad "the thread tools were never asked for a slot"
fi

# --- 5. The bucket holds exactly the bytes the model wrote --------------------------------------
puts=$(printf '%s' "$journal" | jq '[.requests[] | select(.request.method == "PUT")]')
put_count=$(printf '%s' "$puts" | jq 'length')
if [ "$put_count" = "1" ]; then
  ok "the bucket saw one PUT"
else
  bad "the bucket saw $put_count PUTs, expected one"
fi
if [ "$put_count" = "1" ]; then
  put_path=$(printf '%s' "$puts" | jq -r '.[0].request.url')
  case "$put_path" in
    /bucket/*) ok "the PUT went to the slot's bucket path: $put_path" ;;
    *) bad "the PUT went to $put_path" ;;
  esac
  printf '%s' "$puts" | jq -r '.[0].request.bodyAsBase64' | base64 -d > "$tmp/put.bin"
  put_sha=$(sha256sum "$tmp/put.bin" | cut -d' ' -f1)
  put_size=$(wc -c < "$tmp/put.bin" | tr -d ' ')
  if [ "$put_sha" = "$sha" ] && [ "$put_size" = "$size" ]; then
    ok "the bucket holds exactly the file: $put_size bytes, SHA-256 $sha"
  else
    bad "the bucket holds $put_size bytes (SHA-256 $put_sha), expected $size bytes (SHA-256 $sha)"
  fi
fi

if [ "$fail" = 0 ]; then
  echo "uploads e2e: all checks passed"
  exit 0
fi
echo "FAIL uploads e2e"
exit 1
