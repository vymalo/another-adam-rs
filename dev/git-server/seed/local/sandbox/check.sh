#!/bin/sh
# The sandbox's check: what an agent must keep green before opening a pull
# request. Exit 0 = pass.
set -eu
cd "$(dirname "$0")"

test -f README.md || { echo "check: README.md is missing" >&2; exit 1; }

if [ -f hello.txt ] && ! grep -q '^hello' hello.txt; then
  echo "check: hello.txt must start with 'hello'" >&2
  exit 1
fi

echo "check: ok"
