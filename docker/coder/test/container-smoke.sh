#!/bin/sh
# Container smoke test of the coder image: it starts as uid 10001 under tini
# with the tools on PATH, serves A2A with fail-closed auth, takes a task to
# input-required against a stub model, and exits 0 on SIGTERM within 15 s.
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
  -e MCP_ALLOW_STDIO=true \
  -e PUBLIC_URL="http://127.0.0.1:$coder_port/" \
  -e LISTEN_ADDR="127.0.0.1:$coder_port" \
  "$image" >/dev/null

# Liveness, the card, 401 without a token, and a task that waits for the person.
sh "$here/http-smoke.sh" "http://127.0.0.1:$coder_port" "$token" || fail=1

# The runtime user and the tools the agent shells out to.
uid=$(docker exec "$name" id -u)
if [ "$uid" = 10001 ]; then ok "runs as uid 10001"; else bad "runs as uid $uid, want 10001"; fi
if docker exec "$name" bash -lc 'git --version && opencode --version && github-mcp-server --version && command -v tini adam-coder' >/dev/null; then
  ok "git, opencode, github-mcp-server, tini and adam-coder are on PATH in a login shell"
else
  bad "a tool is missing from PATH in a login shell"
fi
# The devcontainer client side (ADR 0010): the CLI that makes a repository's devcontainer and
# Podman's client it drives. Nothing in this image runs containers.
if docker exec "$name" bash -lc 'test "$(devcontainer --version)" = 0.89.0 && podman-remote --version | grep -q "5\\.8\\.7"' >/dev/null; then
  ok "the devcontainer CLI is 0.89.0 and podman-remote is 5.8.7, the service's release"
else
  bad "the devcontainer CLI or podman-remote is missing or not the pinned version (0.89.0, 5.8.7)"
fi
# DEVCONTAINER_RUNTIME=podman mounts the coder's OpenCode into every devcontainer, so the
# configuration checks that it is a native executable (not the npm package's script) and resolves
# the link of the PATH entry to the file: with only the runtime set, the problems listed must not
# name OpenCode (they name the database, the model and so on).
out=$(docker run --rm -e DEVCONTAINER_RUNTIME=podman -e CONTAINER_HOST=unix:///run/podman/podman.sock "$image" 2>&1 || true)
if echo "$out" | grep -q "invalid configuration" && ! echo "$out" | grep -qi "opencode"; then
  ok "DEVCONTAINER_RUNTIME=podman finds the image's OpenCode as a native executable"
else
  bad "DEVCONTAINER_RUNTIME=podman does not accept the image's OpenCode: $out"
fi
# Local-process MCP servers are the coder's alone: the image sets no MCP_ALLOW_STDIO (an `adam-agent`
# run from it refuses such servers unless its own deployment opts in), and the coder's deployment sets it,
# which this script does for the container above.
if [ -z "$(docker run --rm --entrypoint sh "$image" -c 'printf %s "${MCP_ALLOW_STDIO:-}"' 2>/dev/null)" ]; then
  ok "the image does not allow local-process MCP servers: only the coder's deployment does"
else
  bad "the image sets MCP_ALLOW_STDIO: every agent of the image would allow local-process MCP servers"
fi
# The GitHub MCP server of the shipped `mcp.json`: the coder (embedded agent files, MCP_ALLOW_STDIO=true as
# its deployment sets it) started it as a child process and connected it (the check above already needed it
# to start), and it lists the twelve read tools and no write tool.
if docker logs "$name" 2>&1 | grep -q 'connected to the MCP server.*github'; then
  ok "the coder connected the GitHub MCP server"
else
  bad "the coder did not connect the GitHub MCP server"
fi
if docker exec -i "$name" sh -s < "$here/github-mcp-tools.sh" >/dev/null; then
  ok "the GitHub MCP server lists the tools of the allow-list and none that writes"
else
  bad "the GitHub MCP server's tool list is not the allow-list"
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
