#!/bin/sh
# Compare each catalog fixture of adam-ui with the published file at the commit that
# `crates/adam-ui/tests/fixtures/UPSTREAM` names. Needs curl and the network (CI, job `docs`).
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
fixtures="$root/crates/adam-ui/tests/fixtures"
base=https://raw.githubusercontent.com/vymalo/another-agentic-system
path=web/src/features/chat/lib/a2ui/catalog/catalog.json
tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT

failed=0
while read -r file commit digest; do
  case "$file" in
    catalog-v*.json) ;;
    *) continue ;;
  esac
  curl -sSfL -o "$tmp" "$base/$commit/$path"
  if cmp -s "$tmp" "$fixtures/$file"; then
    echo "ok       $file at $commit ($digest)"
  else
    echo "DIFFERS  $file from $base/$commit/$path" >&2
    failed=1
  fi
done <"$fixtures/UPSTREAM"
exit "$failed"
