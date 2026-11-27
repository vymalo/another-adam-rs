#!/bin/sh
# The digest a ghcr.io reference resolves to, read from the registry without pulling anything
# (release.yml, ADR 0084: an image tag or a chart version that is already published is never
# pushed over).
#
#   sh tools/release/registry-digest.sh ghcr.io/<owner>/<name> <tag>
#
# Exit 0 with the digest on stdout; 3 when the registry answers 404 (no such tag, or no such package);
# 1 for anything else (a refusal, a 5xx, no answer), so a caller never pushes past a question the
# registry did not answer. Credentials: GHCR_USER and GHCR_TOKEN when both are set (read from the
# environment and handed to curl on its stdin, never on a command line), anonymous otherwise.
#
# Needs curl and jq.
set -eu

if [ "$#" -ne 2 ]; then
  echo "usage: $0 ghcr.io/<owner>/<name> <tag>" >&2
  exit 2
fi
case "$1" in
  ghcr.io/*) repo=${1#ghcr.io/} ;;
  *) echo "registry-digest: '$1' is not on ghcr.io" >&2; exit 2 ;;
esac
ref=$2

headers=$(mktemp)
trap 'rm -f "$headers"' EXIT

if [ -n "${GHCR_USER:-}" ] && [ -n "${GHCR_TOKEN:-}" ]; then
  answer=$(printf 'user = "%s:%s"\n' "$GHCR_USER" "$GHCR_TOKEN" |
    curl -sS --config - "https://ghcr.io/token?scope=repository:${repo}:pull") || exit 1
else
  answer=$(curl -sS "https://ghcr.io/token?scope=repository:${repo}:pull") || exit 1
fi
# A package that does not exist gets no token (`null`, *verified 2026-11-27* anonymously): nothing to read.
token=$(printf '%s' "$answer" | jq -r '.token // empty' 2>/dev/null || true)
if [ -z "$token" ]; then
  echo "registry-digest: no token for ${repo}: no such package (or none this caller may read)" >&2
  exit 3
fi

accept='application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json'
accept="$accept, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json"
code=$(printf 'header = "Authorization: Bearer %s"\n' "$token" |
  curl -sS --config - -o /dev/null -D "$headers" -w '%{http_code}' -I -H "Accept: $accept" \
    "https://ghcr.io/v2/${repo}/manifests/${ref}") || exit 1

case "$code" in
  200)
    digest=$(tr -d '\r' < "$headers" | sed -n 's/^[Dd]ocker-[Cc]ontent-[Dd]igest: *\(sha256:[0-9a-f]\{64\}\)$/\1/p' | head -n 1)
    if [ -z "$digest" ]; then
      echo "registry-digest: ${repo}:${ref} answered 200 with no digest" >&2
      exit 1
    fi
    printf '%s\n' "$digest"
    ;;
  404) exit 3 ;;
  *)
    echo "registry-digest: ${repo}:${ref} answered HTTP $code" >&2
    exit 1
    ;;
esac
