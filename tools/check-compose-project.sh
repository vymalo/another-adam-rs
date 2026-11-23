#!/bin/sh
# Two checkouts must get two compose project names: compose.yaml pins no `name:`, so Compose
# derives the project from the checkout's directory.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
if grep -q '^name:' "$root/compose.yaml"; then
  echo "compose.yaml pins a project name; remove the top-level 'name:'" >&2
  exit 1
fi
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
for w in a b; do
  mkdir -p "$tmp/wt-$w"
  cp -R "$root/compose.yaml" "$tmp/wt-$w/"
done
name_of() { (cd "$1" && docker compose config --format json 2>/dev/null | sed -n 's/^  "name": "\(.*\)",\{0,1\}$/\1/p'); }
a=$(name_of "$tmp/wt-a"); b=$(name_of "$tmp/wt-b")
if [ -z "$a" ] || [ "$a" = "$b" ]; then
  echo "two checkouts gave project names '$a' and '$b'" >&2
  exit 1
fi
echo "ok: two checkouts give '$a' and '$b'"
