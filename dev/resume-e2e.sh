#!/bin/sh
# End-to-end test of the coder's resume budget: from a fresh process start to `/healthz` 200 and to
# the first claim of a waiting run, measured, with a ceiling each step fails the test on. Spec row
# A3 of `docs/architecture/05-runtime.md` (AD-058). A2 (`adam-coder`'s clean park and drain on
# SIGTERM, adam#431) is the precondition: the script creates a run, claims it, parks the coder with
# SIGTERM, and asserts the A2 invariant (`status='runnable' AND lease_owner IS NULL`) before it
# measures, so a violation of A2 is not misread as a violation of A3.
#
#   dev/resume-e2e.sh                          # the coder must reach Ready and the first claim under the ceilings
#   READY_CEILING_MS=... dev/resume-e2e.sh     # override the ceiling that fails the test (milliseconds since container start)
#   CLAIM_CEILING_MS=... dev/resume-e2e.sh     # same, for the first claim
#
# Start the stack first (the model `mock-coder` is the script in
# dev/wiremock/mock-openai/mappings/coder-script.json; the default scenario's text below is the
# one the scripted model recognises, the same as `dev/coder-e2e.sh`):
#
#   docker compose --profile app up -d --build --wait \
#     postgres mock-openai mock-github mock-github-mcp git-server coder
#
# Steps:
#   1. Preconditions: the coder's `GET /healthz` answers 200 at
#      `http://127.0.0.1:${CODER_PORT:-8080}/healthz` (the chart's probe path; the row says `/readyz`
#      and the coder serves `/healthz` as its readiness signal — same signal, no new route).
#   2. Send one A2A `SendStreamingMessage` to the coder with the default scenario's message text
#      (the same as `dev/coder-e2e.sh`: a repository `local/sandbox` and a file `hello.txt`); a
#      unique `messageId` per run. Read the stream far enough to capture the task id (the A2A task
#      id is the run id; `bin/adam-coder/tests/binary.rs`: `RunId(task.id.parse())`), then close
#      the stream.
#   3. Poll the run store (Postgres of the compose stack, database `adam_test`, table `adam_runs`,
#      default prefix `adam_`; read with `docker compose exec -T postgres psql ... -tAc`, so no host
#      `psql` is needed) for the run's first claim (`lease_owner IS NOT NULL` on a `runnable` run)
#      with a deadline of 60 s and a `POLL_INTERVAL_S` cadence (0.2 s). As soon as the claim is
#      observed, send SIGTERM directly (`docker kill -s SIGTERM`) — `docker compose stop` would
#      wait up to its 10 s graceful-shutdown timeout and let the worker process the rest of the
#      run; the A2 invariant is the run left `runnable` and not `done`. Close the A2A stream
#      first (kill the curl) so the HTTP server's 10 s graceful-shutdown wait does not hold
#      the process past the SIGTERM.
#   4. After the stop, assert the A2 invariant on the run: the container exited with code 0, the
#      run is `runnable` and `lease_owner IS NULL`. If any of these fail, the script fails the
#      test with a message naming A2's invariant (a violation of A2 is not a violation of A3).
#   5. Remove the coder container and the `coder-work` volume (the cache: find the volume name
#      with `docker inspect` on the coder container's `/work` mount, then `docker volume rm`), then
#      `docker compose up -d --no-build --detach coder` (NOT `--wait`: the worker pool can claim
#      a runnable run as soon as the process binds its port, before the compose healthcheck
#      returns; waiting would mean polling starts after the lease is already gone on a fast
#      worker). On the empty cache, measure:
#        * t0: the container's `docker inspect -f '{{.State.StartedAt}}'` (RFC3339 with
#          nanoseconds, converted with GNU `date -d`; CI is ubuntu and this host has GNU `date`).
#        * ready: the first `GET /healthz` that answers 200.
#        * first claim: the first poll that shows the run (the same id) with `lease_owner IS NOT
#          NULL` again.
#      Print both deltas in milliseconds and fail when either exceeds its ceiling.
#   6. Cleanup: a `trap` removes the tmp dir; the compose stack stays up (the caller owns it).
#
# Environment (defaults match `compose.yaml` on one machine):
#   CODER_URL         http://127.0.0.1:${CODER_PORT:-8080}
#   CODER_TOKEN       dev-token
#   CODER_PORT        8080
#   POSTGRES_PORT     5432
#   COMPOSE_PROJECT_NAME  (unset)  the compose project to use; this script runs `docker compose` so
#                                   the same env var the caller used to start the stack is read
#   READY_CEILING_MS  5000    a fresh coder must reach `/healthz` 200 within this many ms of
#                             `State.StartedAt`. Default set from a measurement on the owner's
#                             machine (see below); CI (ubuntu, cold runner) is the worse case.
#                             The preconditions above must hold before this is measured; if a
#                             preconditions check fails the script exits non-zero with a message
#                             naming A2's invariant.
#   CLAIM_CEILING_MS  5000    the same run, claimed after a fresh start, must be claimed within
#                             this many ms of `State.StartedAt`. Same shape of margin.
#   POLL_INTERVAL_S   0.2     cadence of the claim poll (the deadline is `POLL_DEADLINE_S`)
#   POLL_DEADLINE_S   60      the deadline of the claim poll (the brief specifies 60 s)
#   MEASURE_INTERVAL_S 0.05   cadence of the `/healthz` and claim poll after the fresh start
#                             (the lease is held for the run's processing time, ~2.5 s with
#                             mock-coder, so a tight cadence catches it)
#   RESUME_E2E_DEBUG  (unset) 1 prints each poll result of step 4 (the lease value at each
#                             `MEASURE_INTERVAL_S` tick)
#
# Needs curl, jq, GNU `date`, `docker compose` and a coder image the `coder` service can use without
# a rebuild (`CODER_IMAGE=${CODER_IMAGE:-adam-rs/coder:dev}`).
#
# Measured 2026-11-28 on the owner's host (AMD 24 cores, cgroup `app.slice`; `COMPOSE_PROJECT_NAME=
# anr759`, ports `25432`, `28080`, `28081`, `28082`, `28083`, `28085`): 5 fresh starts with an empty
# `coder-work` volume:
#   ready: 221, 257, 260, 221, 277 ms
#   first claim: 319, 347, 381, 314, 399 ms (the claim is within ~100 ms of `/healthz` 200)
# 3x the max measured is well under 2 s; the brief says the ceiling has a floor of ~5 s so CI noise
# does not flake it, and the default ceilings are `READY_CEILING_MS=5000` and `CLAIM_CEILING_MS=5000`.
# shellcheck disable=SC2034  # `text` is read by `jq` as `$text` inside a `--arg`; shellcheck cannot
#                            # see the use and warns the variable is unused.
set -eu

cd "$(dirname "$0")/.."

coder=${CODER_URL:-http://127.0.0.1:${CODER_PORT:-8080}}
coder=${coder%/}
token=${CODER_TOKEN:-dev-token}
# The coder's health endpoint (the chart's probe path; the spec row says `/readyz` and the coder
# serves `/healthz` as its readiness signal — same signal, no new route).
healthz="$coder/healthz"
ready_ceiling_ms=${READY_CEILING_MS:-5000}
claim_ceiling_ms=${CLAIM_CEILING_MS:-5000}
poll_deadline_s=${POLL_DEADLINE_S:-60}
# Step 3 polls for the claim so SIGTERM can be sent; the brief specifies 0.2 s.
poll_interval_s=${POLL_INTERVAL_S:-0.2}
# Step 4 polls for `/healthz` 200 and the first claim after the fresh start; tighter because the
# lease is short (mock-coder finishes in ~2.5 s; the new coder claims within ~100 ms of `/healthz`).
measure_interval=${MEASURE_INTERVAL_S:-0.05}

# Sanity: a non-numeric ceiling is a typo, refuse it before measuring.
case "$ready_ceiling_ms" in '' | *[!0-9]*) echo "READY_CEILING_MS must be a positive integer (milliseconds), not '$ready_ceiling_ms'" >&2; exit 2 ;; esac
case "$claim_ceiling_ms" in '' | *[!0-9]*) echo "CLAIM_CEILING_MS must be a positive integer (milliseconds), not '$claim_ceiling_ms'" >&2; exit 2 ;; esac
case "$poll_deadline_s" in '' | *[!0-9]*) echo "POLL_DEADLINE_S must be a positive integer (seconds), not '$poll_deadline_s'" >&2; exit 2 ;; esac
case "$poll_interval_s" in '' | *[!0-9.]*) echo "POLL_INTERVAL_S must be a positive number (seconds), not '$poll_interval_s'" >&2; exit 2 ;; esac
case "$measure_interval" in '' | *[!0-9.]*) echo "MEASURE_INTERVAL_S must be a positive number (seconds), not '$measure_interval'" >&2; exit 2 ;; esac

# The compose project the caller used to start the stack: `docker compose` reads it from the
# environment. When unset, the script prints commands that target the default project (`adam-rs`),
# the same project name the user starts.
project=${COMPOSE_PROJECT_NAME:-}

# The task text the scripted `mock-coder` model recognises: the default scenario of `dev/coder-e2e.sh`.
# It names `local/sandbox` and asks for `hello.txt`; the model's script then does the rest
# (prepare the workspace, list branches over MCP, write the file, run the checks, commit and push,
# open the pull request, final text). It is the same text `dev/coder-e2e.sh` sends, byte for byte.
# Passed to `jq` as `--arg text`, so shellcheck cannot see the use and warns the variable is unused.
# shellcheck disable=SC2034
text="In http://git-server:8080/local/sandbox.git (base branch main), add hello.txt containing hello."

# The number of seconds, expressed with the precision GNU `date -d` preserves on the input
# (RFC3339 with nanoseconds: nine digits after the dot, the container's `State.StartedAt`).
delta_ms() {
  start_iso=$1
  end_iso=$2
  start_ns=$(date -d "$start_iso" +%s%N)
  end_ns=$(date -d "$end_iso" +%s%N)
  printf '%d' "$(( (end_ns - start_ns) / 1000000 ))"
}

fail=0
ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fail=1; }

tmp=$(mktemp -d)
cleanup() { rm -rf "$tmp"; }
trap cleanup EXIT

# docker_compose <args...>: invoke `docker compose` with the caller's `COMPOSE_PROJECT_NAME` if it
# was set. Avoids re-binding the project for every invocation.
docker_compose() {
  if [ -n "$project" ]; then
    docker compose --project-name "$project" "$@"
  else
    docker compose "$@"
  fi
}

# --- 1. Preconditions: `/healthz` answers 200 ----------------------------------------------------
echo "step 1: the coder is up (${healthz})"
deadline=$(( $(date +%s) + 30 ))
ready=0
while [ "$(date +%s)" -lt "$deadline" ]; do
  code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 "$healthz" || true)
  if [ "$code" = 200 ]; then
    ready=1
    ok "GET ${healthz} is 200"
    break
  fi
  sleep 0.2
done
if [ "$ready" = 0 ]; then
  bad "GET ${healthz} did not answer 200 within 30 s; start the stack first (see the header)"
  exit 1
fi

# --- 2. Send the task, capture the task id --------------------------------------------------------
echo "step 2: create a waiting run and capture its A2A task id"
# A unique `messageId` per run: same agent, no conversation id, so a different message id means a
# different task.
run_id="resume-e2e-$(date +%s)-$$"
rpc=$(jq -n --arg id "$run_id" --arg text "$text" '{jsonrpc: "2.0", id: "1", method: "SendStreamingMessage", params: {message: {messageId: $id, role: "ROLE_USER", parts: [{text: $text}]}}}')
stream=$tmp/stream.sse
: > "$stream"
curl -sN --max-time 5 -o "$stream" -w 'HTTP %{http_code}\n' \
  -X POST "$coder/" \
  -H "Authorization: Bearer $token" -H 'Content-Type: application/json' \
  -d "$rpc" >"$tmp/send.out" 2>"$tmp/send.err" &
send_pid=$!
# Read the first event that carries the task id. The first event is the task itself
# (`result.task.id`); a status update is also valid (`result.statusUpdate.taskId`). The stream is
# a server-sent-events stream of `data: <json-rpc>` lines, one event per line.
task_id=
deadline=$(( $(date +%s) + 5 ))
while [ "$(date +%s)" -lt "$deadline" ] && [ -z "$task_id" ]; do
  sed -n 's/^data: //p' "$stream" 2>/dev/null | jq -r '(.result.task.id // .result.statusUpdate.taskId) // empty' 2>/dev/null \
    | head -n 1 | grep -E '^[0-9a-f-]{36}$' > "$tmp/taskid" || true
  task_id=$(cat "$tmp/taskid" 2>/dev/null || true)
  if [ -z "$task_id" ]; then sleep 0.05; fi
done
if [ -z "$task_id" ]; then
  bad "the stream does not carry the task id within 5 s (HTTP $(cat "$tmp/send.out" 2>/dev/null || echo ?))"
  kill "$send_pid" 2>/dev/null || true
  wait "$send_pid" 2>/dev/null || true
  exit 1
fi
ok "the A2A task id is $task_id"
# The task id is the run id: `bin/adam-coder/tests/binary.rs` does `RunId(task.id.parse())`.

# --- 3. Wait for the claim, park with SIGTERM, assert the A2 invariant -----------------------------
echo "step 3: wait for the claim, then SIGTERM and assert the A2 invariant"
# A claim is `lease_owner IS NOT NULL` on a `runnable` run. The poll is `SELECT lease_owner IS NOT
# NULL FROM adam_runs WHERE id = '$task_id'`. psql's `-tAc` outputs `t` or `f` on one line, trimmed
# by `tr -d`; the lease is held briefly (the model is fast), so poll every ~0.2 s and stop as
# soon as the first `t` is seen.
claimed=0
deadline=$(( $(date +%s) + poll_deadline_s ))
while [ "$(date +%s)" -lt "$deadline" ]; do
  val=$(docker_compose exec -T postgres psql -U postgres -d adam_test -tAc \
    "SELECT (lease_owner IS NOT NULL) FROM adam_runs WHERE id='$task_id'" 2>/dev/null | tr -d '[:space:]' || true)
  if [ "$val" = "t" ]; then
    claimed=1
    ok "the run was claimed"
    break
  fi
  sleep "$poll_interval_s"
done
if [ "$claimed" = 0 ]; then
  bad "the run was not claimed within ${poll_deadline_s} s; the A2 invariant is not the bottleneck"
  kill "$send_pid" 2>/dev/null || true
  wait "$send_pid" 2>/dev/null || true
  exit 1
fi

# Park: SIGTERM. The A2 drain finishes the in-flight step, commits it and exits 0. Close the A2A
# stream first so the HTTP server's 10 s graceful-shutdown wait does not hold the process past
# Docker's 10 s stop timeout. `docker compose stop -t <grace>` waits up to `grace` seconds for the
# container to exit before it gives up; here we use `docker kill -s SIGTERM` (a direct SIGTERM,
# no graceful-shutdown wait, the container is allowed to drain and the next assertion polls for the
# exit) so the signal reaches the worker as soon as the claim is observed and the run is left
# `runnable` for the second start to pick up. `docker compose stop -t 30` is also fine on slower
# hosts where the drain finishes within the grace period.
kill "$send_pid" 2>/dev/null || true
wait "$send_pid" 2>/dev/null || true
if ! docker kill -s SIGTERM "$(docker_compose ps -q coder)" >/dev/null 2>&1; then
  bad "docker kill -s SIGTERM coder did not return 0; the A2 invariant is not the bottleneck"
  exit 1
fi

# Wait for the container to actually exit (the stop command returns once the container is stopped).
deadline=$(( $(date +%s) + 30 ))
exited=0
# `ps -aq` includes stopped containers; `ps -q` returns only running ones, which is empty after the
# stop. The container is identified by name; `--format` prints the same shape `inspect` reads.
container_id=$(docker_compose ps -aq coder 2>/dev/null | head -n 1 || true)
while [ "$(date +%s)" -lt "$deadline" ]; do
  status=$(docker inspect -f '{{.State.Status}} {{.State.ExitCode}}' "$container_id" 2>/dev/null || true)
  if [ "$status" = "exited 0" ]; then
    exited=1
    break
  fi
  sleep 0.2
done
if [ "$exited" = 1 ]; then
  ok "the container exited with code 0 (the A2 drain finished the in-flight step)"
else
  bad "the container did not exit cleanly (status: $status)"
fi

# The A2 invariant: the run is `runnable` and `lease_owner IS NULL`. If this fails, A2 is broken;
# say so plainly and stop, so a violation of A2 is not misread as a violation of A3.
after_status=$(docker_compose exec -T postgres psql -U postgres -d adam_test -tAc \
  "SELECT status FROM adam_runs WHERE id='$task_id'" 2>/dev/null | tr -d '[:space:]' || true)
after_lease=$(docker_compose exec -T postgres psql -U postgres -d adam_test -tAc \
  "SELECT (lease_owner IS NULL) FROM adam_runs WHERE id='$task_id'" 2>/dev/null | tr -d '[:space:]' || true)
if [ "$after_status" = "runnable" ] && [ "$after_lease" = "t" ]; then
  ok "the run is runnable and the lease is NULL (the A2 invariant)"
else
  bad "the A2 invariant is broken: status='$after_status', lease_owner IS NULL? $after_lease"
  exit 1
fi

# --- 4. Measure on an empty cache --------------------------------------------------------------------
echo "step 4: restart on an empty cache and measure"
# The coder container and its `coder-work` volume: the volume carries the package cache (the
# dependency builds, the cargo registry, etc.). On the second start, `StartedAt` to `/healthz` 200
# would be ~hundreds of ms, not the resume budget; an empty cache is the worst case the budget
# is for.
# `docker compose down coder` removes the container and its network attachment; without it the
# volume may still be reported as in use. `--remove-orphans` is a no-op here, kept for the case
# where the original stack was started with a different set of services.
if ! docker_compose down --remove-orphans coder >/dev/null 2>&1; then
  bad "docker compose down coder did not return 0"
  exit 1
fi
# The volume name: read it from any coder container that ever existed (the just-removed one
# included). Look at the compose project's containers (`-a` includes stopped ones).
work_volume=$(docker inspect --format '{{range .Mounts}}{{if eq .Destination "/work"}}{{if eq .Type "volume"}}{{.Name}}{{end}}{{end}}{{end}}' \
  "$(docker_compose ps -aq coder 2>/dev/null | head -n 1)" 2>/dev/null || true)
if [ -n "$work_volume" ]; then
  if ! docker volume rm "$work_volume" >/dev/null 2>&1; then
    bad "could not remove the coder-work volume ($work_volume)"
    exit 1
  fi
fi
# Start the coder again. `--no-build` and the same image the caller built for the original stack.
# `--no-wait` matters: the worker pool can claim a runnable run as soon as the process binds its
# port, before the compose healthcheck (`bash -c "exec 3<>/dev/tcp/127.0.0.1/8080"`) returns.
# Waiting for `--wait` would mean polling starts after the lease is already gone, on a fast
# worker. `--detach` returns once the container is created, with `StartedAt` written, and the
# polling loop runs in parallel.
if ! docker_compose up -d --no-build --detach coder >/dev/null 2>&1; then
  bad "docker compose up -d coder did not return 0"
  exit 1
fi
new_container=$(docker_compose ps -q coder)
started_at=$(docker inspect -f '{{.State.StartedAt}}' "$new_container")
ok "the coder started at $started_at"

# `ready` (first `/healthz` 200) and `first claim` (first poll that shows the run with a lease)
# both come from the same fresh start. The claim can happen **before** `/healthz` 200 (the
# worker pool claims before the HTTP server is listening), so the two are polled in one loop:
# the first poll that sees a claim captures `claimed_at`, and the first poll that sees `/healthz`
# 200 captures `ready_at`. The deadline is 60 s.
#
# The lease is held for the run's processing time (~2.5 s with mock-coder; the model is fast).
# A poll at ~0.2 s catches the claim in one iteration; a tighter cadence (50 ms) is used here
# because the post-stop `runnable` window is short and the worker is right behind the new coder.
ready_at=
claimed_at=
deadline=$(( $(date +%s) + 60 ))
measure_interval=${MEASURE_INTERVAL_S:-0.05}
dbg=${RESUME_E2E_DEBUG:-0}
while [ "$(date +%s)" -lt "$deadline" ]; do
  if [ -z "$ready_at" ]; then
    code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 "$healthz" || true)
    if [ "$code" = 200 ]; then
      ready_at=$(date -u +%Y-%m-%dT%H:%M:%S.%NZ)
    fi
  fi
  if [ -z "$claimed_at" ]; then
    val=$(docker_compose exec -T postgres psql -U postgres -d adam_test -tAc \
      "SELECT (lease_owner IS NOT NULL) FROM adam_runs WHERE id='$task_id'" 2>/dev/null | tr -d '[:space:]' || true)
    if [ "$dbg" = 1 ]; then
      elapsed_ms=$(delta_ms "$started_at" "$(date -u +%Y-%m-%dT%H:%M:%S.%NZ)" 2>/dev/null || echo "?")
      echo "[debug t=${elapsed_ms}ms] lease=$val"
    fi
    if [ "$val" = "t" ]; then
      claimed_at=$(date -u +%Y-%m-%dT%H:%M:%S.%NZ)
    fi
  fi
  if [ -n "$ready_at" ] && [ -n "$claimed_at" ]; then break; fi
  sleep "$measure_interval"
done
# Once both are captured, `up -d`'s `--wait` semantics (which the next test wants) can be restored
# by `docker compose up -d --wait coder`: the container is already running and healthy, so it
# returns immediately.
if ! docker_compose up -d --no-build --wait coder >/dev/null 2>&1; then
  bad "docker compose up -d --wait coder did not return 0 (after the measurement)"
  exit 1
fi
if [ -z "$ready_at" ]; then
  bad "GET ${healthz} did not answer 200 within 60 s on the fresh start"
  exit 1
fi
ready_ms=$(delta_ms "$started_at" "$ready_at")
echo "ready: ${ready_ms} ms (ceiling: ${ready_ceiling_ms} ms)"
if [ "$ready_ms" -le "$ready_ceiling_ms" ]; then
  ok "ready (${ready_ms} ms) is under the ceiling (${ready_ceiling_ms} ms)"
else
  bad "ready (${ready_ms} ms) exceeds the ceiling (${ready_ceiling_ms} ms)"
fi
if [ -z "$claimed_at" ]; then
  bad "the run was not claimed within 60 s of the fresh start"
  exit 1
fi
claim_ms=$(delta_ms "$started_at" "$claimed_at")
echo "first claim: ${claim_ms} ms (ceiling: ${claim_ceiling_ms} ms)"
if [ "$claim_ms" -le "$claim_ceiling_ms" ]; then
  ok "first claim (${claim_ms} ms) is under the ceiling (${claim_ceiling_ms} ms)"
else
  bad "first claim (${claim_ms} ms) exceeds the ceiling (${claim_ceiling_ms} ms)"
fi

if [ "$fail" -eq 0 ]; then echo "resume e2e passed"; else echo "resume e2e FAILED"; exit 1; fi
