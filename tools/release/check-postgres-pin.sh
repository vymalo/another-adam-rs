#!/bin/sh
# Every PostgreSQL a workflow or the compose file runs is the same image, pinned by tag and digest.
#
#   sh tools/release/check-postgres-pin.sh        # from the repository root; ci.yml's `fmt` job runs it
#
# The store refuses a server older than PostgreSQL 18 (ADR 0082, decision 11). release.yml kept `postgres:17` for its
# container smoke test after coder.yml and ci.yml moved, and the first release (v0.6.0) failed on it, in a workflow
# that runs from the tag's own copy and so could not be fixed after the tag. The reference is compose.yaml's image;
# every `image: postgres...` line of .github/workflows/*.yml and compose.yaml must be that exact line.
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
cd "$root"

pins() { grep -nE '^[[:space:]]*image:[[:space:]]*"?postgres[:@"[:space:]]|^[[:space:]]*image:[[:space:]]*"?postgres$' "$@" || true; }

ref=$(pins compose.yaml | sed -E 's/^[0-9]+:[[:space:]]*image:[[:space:]]*//' | sort -u)
case "$ref" in
  postgres:*@sha256:????????????????????????????????????????????????????????????????) ;;
  *) echo "compose.yaml must name one postgres image, by tag and digest; found: ${ref:-none}" >&2; exit 1 ;;
esac

failed=0
count=0
for f in .github/workflows/*.yml compose.yaml; do
  pins "$f" | while IFS= read -r line; do
    image=$(printf '%s' "$line" | sed -E 's/^[0-9]+:[[:space:]]*image:[[:space:]]*//')
    if [ "$image" != "$ref" ]; then
      echo "$f:${line%%:*}: postgres image '$image' is not '$ref'" >&2
      exit 1
    fi
  done || failed=1
  n=$(pins "$f" | grep -c . || true)
  count=$((count + n))
done
if [ "$count" -lt 5 ]; then
  echo "found only $count postgres images (want ci.yml twice, coder.yml, release.yml, compose.yaml): the scan has lost its pattern" >&2
  exit 1
fi
[ "$failed" -eq 0 ] && echo "$count postgres images, all $ref"
exit "$failed"
