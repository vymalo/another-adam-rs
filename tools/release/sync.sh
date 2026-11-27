#!/bin/sh
# What a release moves that cannot carry an x-release-please-version mark (ADR 0084). release-please.yml
# runs it on the release branch, after release-please has rewritten the marked lines:
#
#   * every tracked Cargo.lock outside .agents/: `cargo metadata` records the workspace crates at their new
#     version (it resolves and compiles nothing; a missed mark fails here, not at someone's next build);
#   * the chart's goldens (deploy/coder/tests/golden/*.yaml): every object carries
#     `helm.sh/chart: coder-<version>` and `app.kubernetes.io/version: "<appVersion>"`, and those two
#     labels are the only lines a version moves. render-check.sh, run on the release pull request,
#     proves the edit equals what helm renders.
#
#   sh tools/release/sync.sh        (from the repository root; CARGO_NET_OFFLINE=true works offline)
#
# Needs git, cargo, sed and awk.
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

for golden in deploy/coder/tests/golden/*.yaml; do
  tmp=$(mktemp)
  awk -v version="$version" -v app="$app" '
    /^ *helm\.sh\/chart: coder-/ { sub(/coder-.*$/, "coder-" version); chart++ }
    /^ *app\.kubernetes\.io\/version: "/ { sub(/"[^"]*"$/, "\"" app "\""); appv++ }
    { print }
    END { if (!chart || !appv) exit 1 }
  ' "$golden" > "$tmp" || {
    rm -f "$tmp"
    echo "sync: $golden has no helm.sh/chart or app.kubernetes.io/version label; the label's shape changed" >&2
    exit 1
  }
  cat "$tmp" > "$golden"
  rm -f "$tmp"
done
echo "sync: the lockfiles, and the goldens at coder-$version and \"$app\""
