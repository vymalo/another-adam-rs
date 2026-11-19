#!/bin/sh
# Chat with any adam agent from a terminal: what you type is an A2A message, the agent's steps are
# printed as they come and its answer when the task stops. One conversation (one contextId) per run.
#
#   dev/chat.sh                          # the chat-only Assistant of the compose stack (:8084)
#   dev/chat.sh http://127.0.0.1:8080    # Adam, the coder
#   dev/chat.sh URL "hi"                 # send one message, print the answer, exit
#
# A task that asks you something (input-required) gets your next line as its answer; a finished task is
# continued by a new one in the same conversation (referenceTaskIds), so the agent keeps the context.
# Type /new for a new conversation, /quit (or Ctrl-D) to leave.
#
# Environment:
#   TOKEN    dev-token   the A2A bearer token (the compose stack's)
#   TIMEOUT  600         seconds to wait for one task to stop
#
# Needs curl and jq. The wire is A2A 1.0 JSON-RPC over SSE (docs/reference/a2a-server.md).
set -eu

url=${1:-http://127.0.0.1:${AGENT_PORT:-8084}}
url=${url%/}
token=${TOKEN:-dev-token}
timeout=${TIMEOUT:-600}

for tool in curl jq; do
  command -v "$tool" >/dev/null 2>&1 || { echo "dev/chat.sh needs $tool" >&2; exit 69; }
done

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

card=$(curl -sf --max-time 10 "$url/.well-known/agent-card.json") || {
  echo "no agent card at $url/.well-known/agent-card.json: is the agent running?" >&2
  exit 69
}
name=$(printf '%s' "$card" | jq -r '.name // "agent"')

context=
task=
state=

# send <text>: one SendStreamingMessage, printing steps as they arrive and the answer at the end.
send() {
  message_id="chat-$(date +%s)-$$-$(od -An -N4 -tx4 /dev/urandom | tr -d ' ')"
  rpc=$(jq -n --arg id "$message_id" --arg text "$1" --arg ctx "$context" --arg task "$task" --arg state "$state" '
    {messageId: $id, role: "ROLE_USER", parts: [{text: $text}]}
    + (if $ctx != "" then {contextId: $ctx} else {} end)
    + (if $state == "TASK_STATE_INPUT_REQUIRED" then {taskId: $task}
       elif $task != "" then {referenceTaskIds: [$task]} else {} end)
    | {jsonrpc: "2.0", id: "1", method: "SendStreamingMessage", params: {message: .}}')
  : > "$tmp/events.jsonl"
  # One JSON object per `data:` line, printed as it comes: the steps of a working task.
  curl -sN --max-time "$timeout" -X POST "$url/" \
    -H "Authorization: Bearer $token" -H 'Content-Type: application/json' -d "$rpc" |
  while IFS= read -r line; do
    case "$line" in
      data:*) event=${line#data:} ;;
      *) continue ;;
    esac
    printf '%s\n' "$event" >> "$tmp/events.jsonl"
    printf '%s' "$event" | jq -r '
      if .error then "error \(.error.code): \(.error.message)"
      elif (.result.statusUpdate.status.state // "") == "TASK_STATE_WORKING" then
        ([.result.statusUpdate.status.message.parts[]?.text // empty] | join(" ")) as $t
        | if $t == "" then empty else "  · \($t | split("\n")[0])" end
      elif .result.artifactUpdate then
        "  · artifact: \(.result.artifactUpdate.artifact.name // "?")"
      else empty end' 2>/dev/null || true
  done
  last=$(jq -c 'select(.result.statusUpdate or .result.task)' "$tmp/events.jsonl" 2>/dev/null | tail -n 1)
  if [ -z "$last" ]; then
    echo "(no answer: $(head -c 300 "$tmp/events.jsonl"))"
    return
  fi
  context=$(printf '%s' "$last" | jq -r '.result.statusUpdate.contextId // .result.task.contextId // empty')
  task=$(printf '%s' "$last" | jq -r '.result.statusUpdate.taskId // .result.task.id // empty')
  state=$(printf '%s' "$last" | jq -r '.result.statusUpdate.status.state // .result.task.status.state // empty')
  printf '\n%s: ' "$name"
  printf '%s' "$last" | jq -r '[(.result.statusUpdate.status.message // .result.task.status.message).parts[]?.text // empty] | join("\n")'
  case "$state" in
    TASK_STATE_COMPLETED) ;;
    TASK_STATE_INPUT_REQUIRED) echo "  (waiting for your answer)" ;;
    *) echo "  ($state)" ;;
  esac
  echo
}

if [ $# -ge 2 ]; then
  send "$2"
  exit 0
fi

echo "Chatting with $name at $url. /new starts a new conversation, /quit leaves."
while printf 'you: ' && IFS= read -r line; do
  case "$line" in
    "") continue ;;
    /quit) break ;;
    /new) context=''; task=''; state=''; echo "(new conversation)"; continue ;;
  esac
  send "$line"
done
