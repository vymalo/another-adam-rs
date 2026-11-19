#!/bin/sh
# The dry run of the benchmark harness: both agents, three self-test tasks, a scripted model, no money.
#
#   BENCH_TEST_DOCKER=1 sh bench/dry-run.sh                  all phases
#   BENCH_TEST_DOCKER=1 sh bench/dry-run.sh --phase grades   one phase: grades, budget or resume
#   BENCH_TEST_DOCKER=1 BENCH_TEST_REQUIRE_DOCKER=1 sh bench/dry-run.sh   a missing container service is a failure
#
# It proves, from the ledger it leaves: the grades are 1, 1, 0; the cost is the scripted usage times the price; a
# run over its budget ends budget_hit; a harness killed in the middle of a run starts again and re-runs only the
# interrupted run (bench/README.md, "The dry run"). It brings the stack of bench/compose.yaml up and, when it ends,
# takes it down with its volumes and stops the harness's whole process group, so a stopped run leaves no child (a
# `resume-child` that outlived it rebuilt the self images under the next run, which then failed with `No such image`).
# It needs Docker with the compose plugin, Node 22 or newer, and what `node bench/bin/bench.mjs doctor` checks. It has
# run on the host's Docker (2026-11-19, Docker 29.9.3 rootless; bench/README.md, "The dry run").
#
# Environment: BENCH_HOME (default: a new temporary directory, removed at the end), BENCH_METER_BIND (default:
# 0.0.0.0, which the stack's relay reaches through the host gateway; under rootless Docker the bridge gateway is not
# the host's address, so the meter listens on every interface and its paths still require a run token),
# BENCH_METER_PORT (18090), BENCH_GIT_PORT (18083), BENCH_CODER_1_PORT (18081), BENCH_CODER_2_PORT (18082).
set -eu

cd "$(dirname "$0")/.."

if [ "${BENCH_TEST_DOCKER:-}" != 1 ]; then
  if [ "${BENCH_TEST_REQUIRE_DOCKER:-}" = 1 ]; then
    echo "bench dry run: BENCH_TEST_REQUIRE_DOCKER=1 needs BENCH_TEST_DOCKER=1" >&2
    exit 1
  fi
  echo "bench dry run: skipped (BENCH_TEST_DOCKER is not 1)"
  exit 0
fi

fail() {
  echo "bench dry run: $1" >&2
  exit 1
}

command -v node >/dev/null 2>&1 || fail "node is not installed"
command -v docker >/dev/null 2>&1 || fail "docker is not installed"
docker compose version >/dev/null 2>&1 || fail "the docker compose plugin is not installed"
docker info >/dev/null 2>&1 || fail "the Docker daemon does not answer"

made_home=
harness=
harness_group=
harness_pid_file=
if [ -z "${BENCH_HOME:-}" ]; then
  BENCH_HOME=$(mktemp -d "${TMPDIR:-/tmp}/adam-bench-dryrun.XXXXXX")
  made_home=$BENCH_HOME
fi
chmod 755 "$BENCH_HOME"
export BENCH_HOME
if [ -z "${BENCH_METER_BIND:-}" ]; then
  # A relay reaches the host through the host's own address; the meter listens on every interface, and only a
  # request with a run token reaches the model.
  BENCH_METER_BIND=0.0.0.0
fi
export BENCH_METER_BIND

cleanup() {
  status=$?
  # What follows must not change the status of the run: no -e here.
  set +e
  # The harness and everything it started (a resume child, a build) go first, before the stack they use. A killed dry
  # run that leaves them behind rebuilds the self images under the next run, which then fails with `No such image`.
  if [ -n "${harness:-}" ] && [ -n "$harness_pid_file" ] && [ -f "$harness_pid_file" ]; then
    leader=$(cat "$harness_pid_file" 2>/dev/null)
    case "$leader" in
      '' | *[!0-9]*) ;;
      *)
        if [ -n "$harness_group" ]; then
          kill -TERM -"$leader" 2>/dev/null
          sleep 1
          kill -KILL -"$leader" 2>/dev/null
        else
          kill -TERM "$leader" 2>/dev/null
        fi
        ;;
    esac
  fi
  rm -f "$harness_pid_file" 2>/dev/null
  node bench/bin/bench.mjs dry-run --down >/dev/null 2>&1 || echo "bench dry run: the stack may still be up: docker compose -p adam-bench down --volumes" >&2
  if [ -n "$made_home" ]; then
    # a container may have made what is in it: the service removes it, then the host does
    rm -rf "$made_home" 2>/dev/null || echo "bench dry run: $made_home was not removed; remove it by hand" >&2
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

node bench/bin/bench.mjs doctor

# The harness runs in a session of its own, so its children are in its process group and the cleanup can take the whole
# group down; `wait` (not a foreground command) is what lets the traps above run while it is going. `setsid --wait` is
# util-linux's; without it the harness shares this shell's group and only it is stopped, never the group (which holds
# the caller's shell too).
harness_pid_file=$BENCH_HOME/dry-run-harness.pid
harness=
harness_group=
if command -v setsid >/dev/null 2>&1 && setsid --help 2>&1 | grep -q -- '--wait'; then
  harness_group=1
  # The inner shell expands $$, $0 and $@; this shell must not.
  # shellcheck disable=SC2016
  setsid --fork --wait sh -c 'echo $$ > "$0"; exec "$@"' "$harness_pid_file" node bench/bin/bench.mjs dry-run "$@" &
else
  node bench/bin/bench.mjs dry-run "$@" &
  echo "$!" > "$harness_pid_file"
fi
harness=$!
wait "$harness"
