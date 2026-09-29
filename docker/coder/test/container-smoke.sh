#!/bin/sh
# Container smoke test of the coder image: it starts as uid 10001 under tini
# with the tools on PATH, serves A2A with fail-closed auth, completes a task
# against a stub model, and exits 0 on SIGTERM within 15 s.
#
#   container-smoke.sh <image>
#
# Needs docker, python3, curl and jq, and a Postgres reachable from the host at
# $DATABASE_URL (default postgres://postgres:postgres@127.0.0.1:5432/postgres;
# CI provides a service container). The container shares the host network, so
# 127.0.0.1 reaches the stub model, Postgres and the container's own port
# (MODEL_PORT, default 18080, and CODER_PORT, default 8080, move them).
# Verified by CI only: the authoring environment has no docker daemon.
set -eu

if [ "$#" -ne 1 ]; then
  echo "usage: $0 <image>" >&2
  exit 2
fi
image=$1
here=$(cd "$(dirname "$0")" && pwd)
name=coder-smoke
model_port=${MODEL_PORT:-18080}
coder_port=${CODER_PORT:-8080}
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

python3 "$here/fake-model.py" "$model_port" &
model_pid=$!

docker rm -f "$name" >/dev/null 2>&1 || true
docker run -d --name "$name" --network host \
  -e DATABASE_URL="$database_url" \
  -e MODEL_BASE_URL="http://127.0.0.1:$model_port/v1" \
  -e MODEL_API_KEY=smoke-key \
  -e MODEL=fake-model \
  -e GITHUB_TOKEN=smoke-github-token \
  -e A2A_BEARER_TOKENS="$token" \
  -e PUBLIC_URL="http://127.0.0.1:$coder_port/" \
  -e LISTEN_ADDR="127.0.0.1:$coder_port" \
  "$image" >/dev/null

# Liveness, the card, 401 without a token, and a completed task.
sh "$here/http-smoke.sh" "http://127.0.0.1:$coder_port" "$token" || fail=1

# The runtime user and the tools the agent shells out to.
uid=$(docker exec "$name" id -u)
if [ "$uid" = 10001 ]; then ok "runs as uid 10001"; else bad "runs as uid $uid, want 10001"; fi
if docker exec "$name" bash -lc 'git --version && opencode --version && command -v tini adam-coder' >/dev/null; then
  ok "git, opencode, tini and adam-coder are on PATH in a login shell"
else
  bad "a tool is missing from PATH in a login shell"
fi
pid1=$(docker exec "$name" sh -c 'cat /proc/1/comm')
if [ "$pid1" = tini ]; then ok "tini is PID 1"; else bad "PID 1 is $pid1, want tini"; fi

# SIGTERM through tini: a graceful exit, code 0, within 15 s.
started=$(date +%s)
docker stop --timeout 30 "$name" >/dev/null
elapsed=$(( $(date +%s) - started ))
code=$(docker inspect --format '{{.State.ExitCode}}' "$name")
if [ "$code" = 0 ]; then ok "SIGTERM exits 0"; else bad "exit code $code after SIGTERM, want 0"; fi
if [ "$elapsed" -le 15 ]; then ok "SIGTERM shutdown took ${elapsed}s (<= 15 s)"; else bad "SIGTERM shutdown took ${elapsed}s, want <= 15 s"; fi
if docker logs "$name" 2>&1 | grep -qi 'panicked'; then bad "the logs contain a panic"; else ok "no panic in the logs"; fi

if [ "$fail" -eq 0 ]; then echo "container smoke passed"; else echo "container smoke FAILED"; exit 1; fi
