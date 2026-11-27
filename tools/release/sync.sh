#!/bin/sh
# What a release moves that cannot carry an x-release-please-version mark (ADR 0084). release-please.yml
# runs it on the release branch, after release-please has rewritten the marked lines:
#
#   * every tracked Cargo.lock outside .agents/: `cargo metadata` records the workspace crates at their new
#     version (it resolves and compiles nothing; a missed mark fails here, not at someone's next build);
#   * the chart's goldens (deploy/coder/tests/golden/*.yaml), rendered again by
#     deploy/coder/tests/render-goldens.sh: a version moves two labels of every object and, through them,
#     the checksum annotations of the ConfigMaps a pod reads, so rewriting the labels alone leaves the
#     checksums stale. render-check.sh, run on the release pull request, proves them right.
#
#   sh tools/release/sync.sh        (from the repository root; CARGO_NET_OFFLINE=true works offline)
#
# Needs git, cargo, sed and helm 4.
set -eu

chart=deploy/coder/Chart.yaml
# `[^ #]*` and not `.*`: both lines end in a comment, and a pattern anchored at the end would match nothing.
version=$(sed -n 's/^version: *\([0-9][^ #]*\).*$/\1/p' "$chart" | head -n 1)
app=$(sed -n 's/^appVersion: *"\([^"]*\)".*$/\1/p' "$chart" | head -n 1)
if [ -z "$version" ] || [ -z "$app" ]; then
  echo "sync: cannot read version and appVersion from $chart" >&2
  exit 1
fi

for lock in $(git ls-files -- 'Cargo.lock' '*/Cargo.lock'); do
  case "$lock" in .agents/*) continue ;; esac
  echo "sync: cargo metadata in $(dirname "$lock")"
  (cd "$(dirname "$lock")" && cargo metadata --format-version 1 > /dev/null)
done

sh deploy/coder/tests/render-goldens.sh
echo "sync: the lockfiles, and the goldens at coder-$version and \"$app\""
