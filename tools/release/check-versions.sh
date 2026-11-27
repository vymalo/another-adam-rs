#!/bin/sh
# The release's version lines (ADR 0084). release-please rewrites, in each file that
# release-please-config.json lists under `extra-files`, the first version on every line marked
# `x-release-please-version`, and nothing else. This script checks that the marks are where a
# release needs them.
#
#   sh tools/release/check-versions.sh           (CI, every pull request; from the repository root)
#   sh tools/release/check-versions.sh v1.2.3    (release.yml, on a tag: also every marked line is 1.2.3)
#
# It fails when:
#   * a listed file is missing, or has no marked line;
#   * a tracked Cargo.toml or Chart.yaml has a marked line but is not listed (release-please would skip it);
#   * a path dependency of the workspace carries a `version` and no mark: a release would move the crate and
#     not the requirement, and a 0.x caret range does not cross a minor, so the workspace would stop resolving;
#   * the marked lines of the Cargo manifests disagree;
#   * with a tag: a marked line, Chart.yaml's `version` and `appVersion` included, is not the tag's version.
#
# Needs git, jq, grep and sed.
set -eu

config=release-please-config.json
mark=x-release-please-version
fail=0
bad() { echo "check-versions: $1" >&2; fail=1; }

want=
if [ "$#" -eq 1 ]; then
  want=${1#v}
  if [ "$(printf '%s' "$want" | wc -l)" -ne 0 ] || ! printf '%s\n' "$want" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
    echo "check-versions: '$1' is not a release tag vX.Y.Z" >&2
    exit 2
  fi
elif [ "$#" -ne 0 ]; then
  echo "usage: $0 [vX.Y.Z]" >&2
  exit 2
fi

listed=$(jq -r '.packages["."]["extra-files"][] | if type == "string" then . else .path end' "$config")

# The first X.Y.Z of each marked line, which is the one release-please replaces.
first_versions_of() {
  grep -e "$mark" -- "$1" | while IFS= read -r line; do
    printf '%s\n' "$line" | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -n 1
  done
}

for f in $listed; do
  if [ ! -f "$f" ]; then
    bad "$f is listed in $config but does not exist"
    continue
  fi
  if ! grep -q -e "$mark" -- "$f"; then
    bad "$f is listed in $config but has no line marked $mark"
  fi
  if grep -e "$mark" -- "$f" | grep -vqE '[0-9]+\.[0-9]+\.[0-9]+'; then
    bad "$f has a line marked $mark with no X.Y.Z on it"
  fi
done

for f in $(git ls-files -- 'Cargo.toml' '*/Cargo.toml' '*/Chart.yaml'); do
  case "$f" in .agents/*) continue ;; esac
  grep -q -e "$mark" -- "$f" || continue
  printf '%s\n' "$listed" | grep -qxF -- "$f" || bad "$f has lines marked $mark but is not in $config's extra-files"
done

for f in Cargo.toml crates/*/Cargo.toml bin/*/Cargo.toml; do
  [ -f "$f" ] || continue
  unmarked=$(grep -nE 'path = "[^"]*".*version = "|version = "[^"]*".*path = "' -- "$f" | grep -v -e "$mark" | cut -d: -f1 | tr '\n' ' ' || true)
  [ -z "$unmarked" ] || bad "$f, lines ${unmarked% }: a path dependency with a version and no $mark mark"
done

cargo_versions=$(for f in $listed; do
  case "$f" in *Cargo.toml) [ -f "$f" ] && first_versions_of "$f" ;; esac
done | sort -u)
if [ "$(printf '%s\n' "$cargo_versions" | grep -c .)" -ne 1 ]; then
  bad "the marked lines of the Cargo manifests disagree: $(printf '%s' "$cargo_versions" | tr '\n' ' ')"
fi

if [ -n "$want" ]; then
  for f in $listed; do
    [ -f "$f" ] || continue
    for v in $(first_versions_of "$f"); do
      [ "$v" = "$want" ] || bad "$f has a marked line at $v, the tag is v$want"
    done
  done
fi

if [ "$fail" -ne 0 ]; then
  exit 1
fi
echo "check-versions: $(printf '%s\n' "$listed" | grep -c .) files, the Cargo lines at $cargo_versions${want:+, every line at $want}"
