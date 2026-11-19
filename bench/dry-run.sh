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
# takes it down with its volumes. It needs Docker with the compose plugin, Node 22 or newer, and what
# `node bench/bin/bench.mjs doctor` checks. Nothing here has run where it was written: no container runtime was there.
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
node bench/bin/bench.mjs dry-run "$@"
