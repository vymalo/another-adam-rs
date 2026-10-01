#!/bin/sh
# The GitHub MCP server of the coder image, over stdio, as bin/adam-coder/agent/mcp.json starts it
# (same arguments, no credential at all): it answers `tools/list`, the list holds every tool of the
# coder's allow-list, and no tool that writes (`--read-only`). A missing name would stop the coder
# at startup, so the image build runs this and the container smoke test runs it again.
#
#   github-mcp-tools.sh            # the server is `github-mcp-server` on PATH
#   github-mcp-tools.sh <binary>   # or this binary
#
# Verified 2026-10-01 against github-mcp-server v1.12.2 (image digest in docker/coder/Dockerfile):
# `tools/list` needs no token (the server lists tools without calling GitHub; a missing credential
# starts its OAuth login, which only a `tools/call` triggers). The server stops at the end of its
# input before it has answered, so the requests are followed by a pause.
set -eu

server=${1:-github-mcp-server}
out=$(mktemp)
trap 'rm -f "$out"' EXIT

(
  printf '%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}' \
    '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
    '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}'
  sleep 3
) | env -i PATH="$PATH" HOME="${TMPDIR:-/tmp}" "$server" stdio --read-only --toolsets context,repos,issues,pull_requests >"$out" 2>/dev/null || true

status=0
for tool in get_me search_repositories get_file_contents list_branches list_commits get_commit \
  search_code list_issues issue_read search_issues list_pull_requests pull_request_read; do
  if ! grep -q "\"name\":\"$tool\"" "$out"; then
    echo "github-mcp-server does not list the tool $tool" >&2
    status=1
  fi
done
# With --read-only none of the server's write tools is offered.
for verb in create_ update_ delete_ push_ merge_ add_ fork_; do
  if grep -q "\"name\":\"$verb" "$out"; then
    echo "github-mcp-server lists a tool that writes ($verb...) although it was started with --read-only" >&2
    status=1
  fi
done
if [ "$status" -eq 0 ]; then
  echo "github-mcp-server lists the 12 tools of the coder's allow-list, and none that writes"
fi
exit "$status"
