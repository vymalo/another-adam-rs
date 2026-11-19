#!/bin/sh
# ADR 0052 decision 1: the dev stack and CI carry no container engine, and the four end-to-end
# scenarios that ran on one (`devcontainer`, `default-env`, `broken-env`, `no-runtime`) are gone
# with it. A later change can bring them back by habit (a copied workflow, a reverted file), so the
# rule is a scan and not a convention. Run from the repository root (CI's `fmt` job):
#
#   sh tools/check-dev-stack.sh
#
# It fails when dev/podman or dev/compose.devcontainer.yaml returns, when the engine is named
# anywhere under dev/ or .github/, or when a script or a workflow names one of the four scenarios or
# its mock switch. It reads the working tree, tracked and untracked files alike, so a local run sees
# a file that was just added (a `git grep` would not).
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

failed=0
fail() { echo "$1" >&2; failed=1; }

for path in dev/podman dev/compose.devcontainer.yaml; do
  if [ -e "$path" ]; then
    fail "$path is back: ADR 0052 removes the container engine of the dev stack"
  fi
done

hits=$(grep -rni -- podman dev .github || true)
if [ -n "$hits" ]; then
  echo "$hits" >&2
  fail "the dev stack and CI name a container engine: ADR 0052 removes it (the lines above)"
fi

hits=$(grep -rnE 'SCENARIO=(devcontainer|default-env|broken-env|no-runtime)|\[mock:(devcontainer|default-env|broken-env|no-runtime)\]' \
  dev .github || true)
if [ -n "$hits" ]; then
  echo "$hits" >&2
  fail "a removed e2e scenario is named again: ADR 0052 removes devcontainer, default-env, broken-env and no-runtime"
fi

if [ "$failed" -ne 0 ]; then exit 1; fi
echo "the dev stack and CI carry no container engine, and none of the four removed scenarios"
