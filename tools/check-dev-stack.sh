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

# The scans, as functions so that the probes below run exactly what the tree scans run.
scan_engine() { grep -rni -- podman "$@"; }
scan_scenarios() {
  grep -rnE 'default-env|broken-env|no-runtime|SCENARIO=devcontainer|\[mock:devcontainer\]' "$@"
}
scan_override() { grep -rn -- 'compose\.devcontainer' "$@"; }
scan_e2e_script() { grep -n devcontainer "$@"; }

# The roots each scan must read. A call narrowed by hand (a dropped `.github`, say) would pass
# everything under the dropped root, so `scan_over` refuses a root list that lacks one.
scan_over() { # scan_over <scan> <root>...
  scan=$1
  shift
  case $scan in
    scan_engine | scan_override) required='dev .github' ;;
    scan_scenarios) required='dev .github docs/reference docs/guides' ;;
    scan_e2e_script) required='dev/coder-e2e.sh' ;;
    *)
      echo "scan_over: unknown scan $scan" >&2
      return 2
      ;;
  esac
  for scan_root in $required; do
    case " $* " in
      *" $scan_root "*) ;;
      *)
        echo "the $scan scan was narrowed: it no longer reads $scan_root" >&2
        return 2
        ;;
    esac
  done
  "$scan" "$@"
}

scan_tree() { # scan_tree <message> <scan> <root>...
  message=$1
  scan=$2
  shift 2
  rc=0
  hits=$(scan_over "$scan" "$@") || rc=$?
  case $rc in
    0)
      printf '%s\n' "$hits" >&2
      fail "$message"
      ;;
    1) ;;
    *) fail "the $scan scan did not read every root (exit $rc)" ;;
  esac
}

# A guard that has been narrowed (a dropped `-i`, a name dropped from the pattern, a dropped root)
# is worse than none: it passes everything and says nothing. Each scan is run on a probe first, so
# weakening it fails here. The scenario probe is one file per form, so dropping any one alternative
# fails too.
probe=$(mktemp -d)
trap 'rm -rf "$probe"' EXIT
printf '%s\n' 'a line that names Podman' > "$probe/engine"
printf '%s\n' 'SCENARIO=devcontainer' > "$probe/scenario-devcontainer"
printf '%s\n' 'default-env' > "$probe/scenario-default-env"
printf '%s\n' 'broken-env' > "$probe/scenario-broken-env"
printf '%s\n' 'no-runtime' > "$probe/scenario-no-runtime"
printf '%s\n' '[mock:devcontainer]' > "$probe/scenario-mock"
printf '%s\n' 'docker compose -f compose.yaml -f dev/compose.devcontainer.yaml up' > "$probe/override"
printf '%s\n' 'the devcontainer scenario' > "$probe/e2e-script"
printf '%s\n' 'a path that is back' > "$probe/present"
scan_engine "$probe/engine" >/dev/null || fail "the engine scan no longer matches its probe: it was narrowed"
for name in scenario-devcontainer scenario-default-env scenario-broken-env scenario-no-runtime scenario-mock; do
  scan_scenarios "$probe/$name" >/dev/null || fail "the scenario scan no longer matches $name: it was narrowed"
done
scan_override "$probe/override" >/dev/null || fail "the override scan no longer matches its probe: it was narrowed"
scan_e2e_script "$probe/e2e-script" >/dev/null || fail "the e2e script scan no longer matches its probe: it was narrowed"
present() { # present <path>: names the path and returns 1 when it is back
  if [ -e "$1" ]; then
    echo "$1 is back: ADR 0052 removes the container engine of the dev stack" >&2
    return 1
  fi
}
if present "$probe/present" 2>/dev/null; then
  fail "the path check no longer notices a path that is back: it was narrowed"
fi

for path in dev/podman dev/compose.devcontainer.yaml; do
  present "$path" || failed=1
done

scan_tree "the dev stack and CI name a container engine: ADR 0052 removes it (the lines above)" \
  scan_engine dev .github
scan_tree "a removed e2e scenario is named again: ADR 0052 removes devcontainer, default-env, broken-env and no-runtime" \
  scan_scenarios dev .github docs/reference docs/guides
# The override has no "podman" in its name, so the engine scan does not see a workflow that starts it.
scan_tree "a script or workflow names the removed devcontainer override: ADR 0052 removes dev/compose.devcontainer.yaml" \
  scan_override dev .github
# The e2e script ran the four; it reads no devcontainer configuration itself, so the word has no
# place there at all (a case alternative is enough to accept a scenario again).
scan_tree "dev/coder-e2e.sh names a devcontainer scenario again" \
  scan_e2e_script dev/coder-e2e.sh

if [ "$failed" -ne 0 ]; then exit 1; fi
echo "the dev stack and CI carry no container engine, and none of the four removed scenarios"
