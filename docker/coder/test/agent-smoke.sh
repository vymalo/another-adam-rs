#!/bin/sh
# Container smoke test of adam-agent inside the coder image (the image carries both binaries; the
# entrypoint is adam-coder, so this runs `tini -- adam-agent` instead): it refuses to start without an
# agent folder (exit 78), and with the example folder mounted it starts as uid 10001 under tini, serves
# that folder's card with fail-closed auth, completes a task against a stub model, and exits 0 on
# SIGTERM within 15 s.
#
#   agent-smoke.sh <image>
#
# Needs docker, python3, curl and jq, and a Postgres reachable from the host at $DATABASE_URL (default
# postgres://postgres:postgres@127.0.0.1:5432/postgres; CI provides a service container). The container
# shares the host network, so 127.0.0.1 reaches the stub model, Postgres and the container's own port
# (MODEL_PORT, default 18080, and AGENT_PORT, default 8080, move them). AGENT_FOLDER names the folder to
# mount (default dev/agents/assistant/agent of this repository); it must be readable by uid 10001.
set -eu

if [ "$#" -ne 1 ]; then
  echo "usage: $0 <image>" >&2
  exit 2
fi
image=$1
here=$(cd "$(dirname "$0")" && pwd)
folder=${AGENT_FOLDER:-$(cd "$here/../../.." && pwd)/dev/agents/assistant/agent}
name=agent-smoke
model_port=${MODEL_PORT:-18080}
agent_port=${AGENT_PORT:-8080}
token=smoke-token
database_url=${DATABASE_URL:-postgres://postgres:postgres@127.0.0.1:5432/postgres}
fail=0
model_pid=""

cleanup() {
  status=$?
  if [ "$status" -ne 0 ] || [ "$fail" -ne 0 ]; then
    echo "--- container logs ---"
    docker logs "$name" 2>&1 | tail -n 80 || true
  fi
  docker rm -f "$name" >/dev/null 2>&1 || true
  if [ -n "$model_pid" ]; then kill "$model_pid" 2>/dev/null || true; fi
}
trap cleanup EXIT

ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fail=1; }

if [ ! -f "$folder/instructions.md" ]; then
  echo "FAIL $folder/instructions.md does not exist (AGENT_FOLDER is the agent/ folder)"
  exit 1
fi

# No folder, no agent: the binary has no embedded default and says what to set.
code=0
out=$(docker run --rm --entrypoint tini "$image" -- adam-agent 2>&1) || code=$?
if [ "$code" = 78 ]; then ok "adam-agent without ADAM_AGENT_DIR exits 78"; else bad "adam-agent without ADAM_AGENT_DIR exits $code, want 78"; fi
case "$out" in
  *"ADAM_AGENT_DIR is required"*) ok "it names the variable it needs" ;;
  *) bad "its failure does not say that ADAM_AGENT_DIR is required: $out" ;;
esac

python3 "$here/fake-model.py" "$model_port" &
model_pid=$!

docker rm -f "$name" >/dev/null 2>&1 || true
docker run -d --name "$name" --network host --entrypoint tini \
  -v "$folder:/etc/adam/agent:ro" \
  -e ADAM_AGENT_DIR=/etc/adam/agent \
  -e DATABASE_URL="$database_url" \
  -e MODEL_BASE_URL="http://127.0.0.1:$model_port/v1" \
  -e MODEL_API_KEY=smoke-key \
  -e MODEL=fake-model \
  -e A2A_BEARER_TOKENS="$token" \
  -e PUBLIC_URL="http://127.0.0.1:$agent_port/" \
  -e LISTEN_ADDR="127.0.0.1:$agent_port" \
  "$image" -- adam-agent >/dev/null

# Liveness, the card of the folder, 401 without a token, and a task the agent answers.
EXPECT_NAME=Assistant EXPECT_SKILL=conversation EXPECT_STATE=TASK_STATE_COMPLETED \
  sh "$here/http-smoke.sh" "http://127.0.0.1:$agent_port" "$token" || fail=1

# The runtime user and the binary on PATH.
uid=$(docker exec "$name" id -u)
if [ "$uid" = 10001 ]; then ok "runs as uid 10001"; else bad "runs as uid $uid, want 10001"; fi
if docker exec "$name" bash -lc 'command -v tini adam-agent' >/dev/null; then
  ok "tini and adam-agent are on PATH in a login shell"
else
  bad "tini or adam-agent is missing from PATH in a login shell"
fi
pid1=$(docker exec "$name" sh -c 'cat /proc/1/comm')
if [ "$pid1" = tini ]; then ok "tini is PID 1"; else bad "PID 1 is $pid1, want tini"; fi
if docker logs "$name" 2>&1 | grep -q '"message":"agent files"'; then
  ok "it logged which agent files it runs"
else
  bad "no \`agent files\` line in the logs"
fi

# SIGTERM through tini: a graceful exit, code 0, within 15 s.
started=$(date +%s)
docker stop --timeout 30 "$name" >/dev/null
elapsed=$(( $(date +%s) - started ))
code=$(docker inspect --format '{{.State.ExitCode}}' "$name")
if [ "$code" = 0 ]; then ok "SIGTERM exits 0"; else bad "exit code $code after SIGTERM, want 0"; fi
if [ "$elapsed" -le 15 ]; then ok "SIGTERM shutdown took ${elapsed}s (<= 15 s)"; else bad "SIGTERM shutdown took ${elapsed}s, want <= 15 s"; fi
if docker logs "$name" 2>&1 | grep -qi 'panicked'; then bad "the logs contain a panic"; else ok "no panic in the logs"; fi

if [ "$fail" -eq 0 ]; then echo "agent container smoke passed"; else echo "agent container smoke FAILED"; exit 1; fi
