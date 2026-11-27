#!/bin/sh
# Write the four goldens of tests/golden/ from the chart, with the values render-check.sh compares each
# against. A release moves the chart's version and appVersion, which are in two labels of every object
# and, through them, in the checksums of the ConfigMaps a pod reads, so release-please.yml runs this on
# the release branch (tools/release/sync.sh, ADR 0084). Run it by hand only for a deliberate change to a
# render, and read the diff: render-check.sh is what says the goldens are right.
#
#   sh deploy/coder/tests/render-goldens.sh       (from the repository root; needs helm 4)
set -eu

chart="$(dirname "$0")/.."
golden="$chart/tests/golden"
render() { helm template coder "$chart" --namespace coder-ns --set image.tag=sha-abc1234 "$@"; }

render > "$golden/combined.yaml"
render --set runPods.enabled=true > "$golden/run-pods.yaml"
render --set topology=split --set 'a2a.push.allowedUrls={hooks.example.com}' \
  --set a2a.cardSigning.secretName=coder-card-key > "$golden/a2a-split.yaml"
render --set cache.create=true --set cache.storageClass=rwx --set runPods.enabled=true > "$golden/cache.yaml"
echo "render-goldens: wrote combined, run-pods, a2a-split and cache"
