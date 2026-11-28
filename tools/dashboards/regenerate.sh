#!/bin/sh
# Regenerate every dashboard of `deploy/dashboards/` from its script in `tools/dashboards/`
# (ANR-1349). The JSON is committed and is the source of truth for a reviewer; this script is what
# CI runs, and it fails the build through `git diff --exit-code` when the committed JSON and the
# scripts disagree, so a hand-edited dashboard cannot merge.
#
# Standard library only: the scripts need `python3` and nothing else. A file whose name starts with
# an underscore is a helper module, not a dashboard, and is not run.
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)

for script in "$root"/tools/dashboards/*.py; do
  case "$script" in
    */_*) continue ;;
  esac
  python3 "$script"
done
