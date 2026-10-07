#!/bin/sh
# Set `image.tag` in a values file to a new tag and show the change.
#
#   bump-tag.sh <values.yaml> <tag>
#
# Prints `unchanged` (and leaves the file alone) when the tag is already
# current, so a re-run is a no-op and the workflow never commits twice. Only the
# `tag:` key inside the top-level `image:` block is touched; every other line is
# preserved byte for byte. Used by the real bump job and by the dry run on pull
# requests, so the dry run proves the logic that will run on main. The script of vymalo/another-adam-rs
# deploy/coder/bump-tag.sh, unchanged but for this comment.
set -eu

if [ "$#" -ne 2 ]; then
  echo "usage: $0 <values.yaml> <tag>" >&2
  exit 2
fi
file=$1
tag=$2

case "$tag" in
  sha-[0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]) ;;
  *) echo "refusing tag '$tag': expected sha-<7 hex digits>" >&2; exit 2 ;;
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
