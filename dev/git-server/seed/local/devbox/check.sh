#!/bin/sh
# The devbox's check: what an agent must keep green before opening a pull request. Exit 0 = pass.
#
# `devbox-tool` exists only in this repository's devcontainer (.devcontainer/Dockerfile), so the
# check passes only where the coder runs it in that environment, not in the coder's own container.
set -eu
cd "$(dirname "$0")"

test -f README.md || { echo "check: README.md is missing" >&2; exit 1; }

command -v devbox-tool >/dev/null || { echo "check: devbox-tool is not here: this is not the devcontainer" >&2; exit 1; }
devbox-tool --version | grep -q 'from the devcontainer' || { echo "check: devbox-tool is not the devcontainer's" >&2; exit 1; }

if [ -f tool.txt ] && ! grep -q 'from the devcontainer' tool.txt; then
  echo "check: tool.txt must say where devbox-tool ran: 'from the devcontainer'" >&2
  exit 1
fi

echo "check: ok"
