#!/usr/bin/env sh
# Regenerate the parity goldens of `adam-operator-domain` (crates/adam-operator-domain/tests/golden/*.json)
# from what the chart `deploy/coder` of THIS checkout renders, or check that the checked-in ones are what
# it renders now.
#
# Needs `helm` (3.x; `HELM=/path/to/helm` names one off PATH) and `python3` with PyYAML. No cluster, no network.
#
#   sh tools/adam-operator-parity/regen.sh          # rewrite the goldens
#   sh tools/adam-operator-parity/regen.sh --check  # diff, write nothing (CI: the operator workflow, job `chart`)
#
# A change to the coder chart that changes what the pods run makes `--check` fail until the goldens are
# regenerated and `adam-operator-domain` agrees with them: the diff of the goldens is the change of the env
# contract.
set -eu

NAMESPACE="another-agentic-system"

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
golden="$root/crates/adam-operator-domain/tests/golden"
chart="$root/deploy/coder"
helm=${HELM:-helm}
mode=rewrite
[ "${1:-}" = "--check" ] && mode=check

command -v "$helm" >/dev/null 2>&1 || { echo "helm is not on PATH (set HELM)" >&2; exit 2; }
python3 -c 'import yaml' 2>/dev/null || { echo "python3 with PyYAML is needed" >&2; exit 2; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
helm_version=$("$helm" version --short)

status=0
for values in "$here"/cases/*.yaml; do
  case=$(basename "$values" .yaml)
  "$helm" template coder "$chart" --namespace "$NAMESPACE" -f "$values" \
    | python3 "$here/extract.py" > "$work/$case.projection.json"
  # The provenance goes in the file: which chart, which values, which helm. The revision is not in it: the
  # chart is in this repository, so the revision is the one that holds the file.
  python3 - "$work/$case.projection.json" "$case" "$helm_version" "$NAMESPACE" > "$work/$case.json" <<'PY'
import json, sys
path, case, helm, ns = sys.argv[1:]
doc = json.load(open(path))
doc["source"] = {
    "chart": "deploy/coder",
    "values": f"tools/adam-operator-parity/cases/{case}.yaml",
    "release": "coder",
    "namespace": ns,
    "helm": helm,
    "generatedBy": "tools/adam-operator-parity/regen.sh",
}
json.dump(doc, sys.stdout, indent=2, sort_keys=True)
sys.stdout.write("\n")
PY
  if [ "$mode" = check ]; then
    if ! diff -u "$golden/$case.json" "$work/$case.json"; then
      echo "stale: crates/adam-operator-domain/tests/golden/$case.json (run tools/adam-operator-parity/regen.sh)" >&2
      status=1
    fi
  else
    cp "$work/$case.json" "$golden/$case.json"
    echo "wrote crates/adam-operator-domain/tests/golden/$case.json"
  fi
done
exit "$status"
