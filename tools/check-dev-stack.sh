#!/bin/sh
# ADR 0052 decision 1: the dev stack and CI carry no container engine, and the four end-to-end
# scenarios that ran on one (`devcontainer`, `default-env`, `broken-env`, `no-runtime`) are gone
# with it. A later change can bring them back by habit (a copied workflow, a reverted file), so the
# rule is a scan and not a convention. Run from the repository root (CI's `fmt` job):
#
#   sh tools/check-dev-stack.sh
#
# It fails when dev/podman or dev/compose.devcontainer.yaml returns, when the engine is named
# anywhere under dev/ or .github/, when a script or workflow names the override, or when a script,
# a workflow or a guide names one of the four scenarios. It reads the working tree, tracked and
# untracked files alike, so a local run sees a file that was just added (a `git grep` would not).
# The decision records are not scanned: they are the history, and ADR 0052 names what it removes.
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

failed=0
fail() { echo "$1" >&2; failed=1; }

# The scans, as functions so that the probe below runs exactly what the tree scan runs.
scan_engine() { grep -rni -- podman "$@"; }
scan_scenarios() {
  grep -rnE 'default-env|broken-env|no-runtime|SCENARIO=devcontainer|\[mock:devcontainer\]' "$@"
}
scan_override() { grep -rn -- 'compose\.devcontainer' "$@"; }

# A guard that has been narrowed (a dropped `-i`, a name dropped from the pattern) is worse than
# none: it passes everything and says nothing. Each scan is run on a probe first, so weakening it
# fails here.
probe=$(mktemp -d)
trap 'rm -rf "$probe"' EXIT
printf '%s\n' 'a line that names Podman' > "$probe/engine"
printf '%s\n' 'SCENARIO=broken-env' 'default-env' > "$probe/scenario"
printf '%s\n' 'docker compose -f compose.yaml -f dev/compose.devcontainer.yaml up' > "$probe/override"
scan_engine "$probe" >/dev/null || fail "the engine scan no longer matches its probe: it was narrowed"
scan_scenarios "$probe" >/dev/null || fail "the scenario scan no longer matches its probe: it was narrowed"
scan_override "$probe" >/dev/null || fail "the override scan no longer matches its probe: it was narrowed"

for path in dev/podman dev/compose.devcontainer.yaml; do
  if [ -e "$path" ]; then
    fail "$path is back: ADR 0052 removes the container engine of the dev stack"
  fi
done

hits=$(scan_engine dev .github || true)
if [ -n "$hits" ]; then
  echo "$hits" >&2
  fail "the dev stack and CI name a container engine: ADR 0052 removes it (the lines above)"
fi

hits=$(scan_scenarios dev .github docs/reference docs/guides || true)
if [ -n "$hits" ]; then
  echo "$hits" >&2
  fail "a removed e2e scenario is named again: ADR 0052 removes devcontainer, default-env, broken-env and no-runtime"
fi

# The override has no "podman" in its name, so the engine scan does not see a workflow that starts it.
hits=$(scan_override dev .github || true)
if [ -n "$hits" ]; then
  echo "$hits" >&2
  fail "a script or workflow names the removed devcontainer override: ADR 0052 removes dev/compose.devcontainer.yaml"
fi

# The e2e script ran the four; it reads no devcontainer configuration itself, so the word has no
# place there at all (a case alternative is enough to accept a scenario again).
hits=$(grep -n devcontainer dev/coder-e2e.sh || true)
if [ -n "$hits" ]; then
  echo "$hits" >&2
  fail "dev/coder-e2e.sh names a devcontainer scenario again"
fi

if [ "$failed" -ne 0 ]; then exit 1; fi
echo "the dev stack and CI carry no container engine, and none of the four removed scenarios"
