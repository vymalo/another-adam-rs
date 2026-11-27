#!/bin/sh
# Set `image.tag` in a values file to a new tag and show the change.
#
#   bump-tag.sh <values.yaml> <tag>
#
# <tag> is `sha-<7 hex>`, the image of a commit of main (coder.yml's bump), or `X.Y.Z`, a release
# (release.yml sets it in the chart it packages, never in the repository: ADR 0084).
#
# Prints `unchanged` (and leaves the file alone) when the tag is already
# current, so a re-run is a no-op and the workflow never commits twice. Only the
# `tag:` key inside the top-level `image:` block is touched; every other line is
# preserved byte for byte. Used by the real bump job and by the dry run on pull
# requests, so the dry run proves the logic that will run on main.
set -eu

if [ "$#" -ne 2 ]; then
  echo "usage: $0 <values.yaml> <tag>" >&2
  exit 2
fi
file=$1
tag=$2

case "$tag" in
  sha-[0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]) ;;
  *)
    # One line only: grep matches line by line, so a tag with a newline in it could pass.
    if [ "$(printf '%s' "$tag" | wc -l)" -ne 0 ] || ! printf '%s\n' "$tag" | grep -Eq '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'; then
      echo "refusing tag '$tag': expected sha-<7 hex digits> or a release X.Y.Z" >&2
      exit 2
    fi
    ;;
esac

current=$(awk '
  /^image:/ { in_image = 1; next }
  in_image && /^[^ #]/ { in_image = 0 }
  in_image && /^  tag:/ { sub(/^  tag:[ ]*/, ""); sub(/[ ]*(#.*)?$/, ""); gsub(/"/, ""); print; exit }
' "$file")
if [ -z "$current" ]; then
  echo "no image.tag found in $file" >&2
  exit 1
fi
if [ "$current" = "$tag" ]; then
  echo "unchanged: image.tag is already $tag"
  exit 0
fi

tmp=$(mktemp)
awk -v tag="$tag" '
  /^image:/ { in_image = 1; print; next }
  in_image && /^[^ #]/ { in_image = 0 }
  in_image && !done && /^  tag:/ { print "  tag: " tag; done = 1; next }
  { print }
' "$file" > "$tmp"
cat "$tmp" > "$file"
rm -f "$tmp"
echo "changed: image.tag $current -> $tag"
