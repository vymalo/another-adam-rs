#!/bin/sh
# Tests of deploy/operator/bump-tag.sh: it edits only image.tag, is idempotent, and
# refuses input it does not understand. Needs only sh, awk, cmp and diff.
#
#   sh deploy/operator/tests/bump-tag-test.sh        (from the repository root)
set -eu

here=$(cd "$(dirname "$0")" && pwd)
bump="$here/../bump-tag.sh"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
fail=0

ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fail=1; }

# run <file> <tag>: sets $out and $rc without aborting the script.
run() {
  rc=0
  out=$(sh "$bump" "$@" 2>&1) || rc=$?
}

cat > "$work/values.yaml" <<'YAML'
# A comment mentioning image: and tag: must not confuse the edit.
replicaCount: 1
image:
  repository: ghcr.io/vymalo/another-adam-rs/operator
  tag: sha-0000000 # bumped by CI
  pullPolicy: IfNotPresent
sidecar:
  image:
    tag: keep-me
  tag: also-keep-me
tag: top-level-keep-me
YAML

# 1. A new tag replaces image.tag and nothing else.
cp "$work/values.yaml" "$work/before.yaml"
run "$work/values.yaml" sha-abc1234
if [ "$rc" -eq 0 ] && [ "$out" = "changed: image.tag sha-0000000 -> sha-abc1234" ]; then
  ok "a new tag is applied and reported"
else
  bad "a new tag: rc=$rc out='$out'"
fi
if grep -q '^  tag: sha-abc1234$' "$work/values.yaml"; then ok "image.tag holds the new tag"; else bad "image.tag was not set"; fi
changed=$(diff "$work/before.yaml" "$work/values.yaml" | grep -c '^[<>]' || true)
if [ "$changed" -eq 2 ]; then ok "exactly one line changed"; else bad "$changed diff lines, want 2 (one line out, one in)"; fi
for keep in 'tag: keep-me' 'tag: also-keep-me' '^tag: top-level-keep-me'; do
  if grep -q -- "$keep" "$work/values.yaml"; then ok "untouched: $keep"; else bad "lost: $keep"; fi
done

# 2. The same tag again is a no-op and leaves the file byte for byte alone.
cp "$work/values.yaml" "$work/after-first.yaml"
run "$work/values.yaml" sha-abc1234
case "$out" in unchanged*) unchanged=1 ;; *) unchanged=0 ;; esac
if [ "$rc" -eq 0 ] && [ "$unchanged" -eq 1 ]; then ok "the second run prints unchanged"; else bad "second run: rc=$rc out='$out'"; fi
if cmp -s "$work/after-first.yaml" "$work/values.yaml"; then ok "the second run leaves the file identical"; else bad "the second run modified the file"; fi

# 3. A quoted current tag is read correctly.
sed 's/^  tag: .*/  tag: "sha-1111111"/' "$work/before.yaml" > "$work/quoted.yaml"
run "$work/quoted.yaml" sha-1111111
case "$out" in unchanged*) ok "a quoted current tag counts as current" ;; *) bad "quoted tag: rc=$rc out='$out'" ;; esac

# 4. Malformed tags are refused before the file is touched.
cp "$work/before.yaml" "$work/refuse.yaml"
for tag in latest sha-abc123 sha-abc12345 sha-ABCDEF1 sha-abcdefg v1.2.3 ""; do
  run "$work/refuse.yaml" "$tag"
  if [ "$rc" -eq 2 ]; then ok "refuses tag '$tag' (exit 2)"; else bad "tag '$tag': rc=$rc, want 2"; fi
done
if cmp -s "$work/before.yaml" "$work/refuse.yaml"; then ok "refused tags leave the file alone"; else bad "a refused tag changed the file"; fi

# 5. Wrong argument count is a usage error.
run "$work/refuse.yaml"
if [ "$rc" -eq 2 ]; then ok "one argument is a usage error"; else bad "one argument: rc=$rc"; fi
run
if [ "$rc" -eq 2 ]; then ok "no arguments is a usage error"; else bad "no arguments: rc=$rc"; fi

# 6. A values file without image.tag fails without writing anything.
printf 'replicaCount: 1\nimage:\n  repository: x\n' > "$work/notag.yaml"
cp "$work/notag.yaml" "$work/notag.before"
run "$work/notag.yaml" sha-abc1234
if [ "$rc" -eq 1 ]; then ok "no image.tag is an error (exit 1)"; else bad "no image.tag: rc=$rc, want 1"; fi
if cmp -s "$work/notag.before" "$work/notag.yaml"; then ok "the file without image.tag is untouched"; else bad "it modified a file without image.tag"; fi
run "$work/none.yaml" sha-abc1234
if [ "$rc" -ne 0 ]; then ok "a missing file fails"; else bad "a missing file succeeded"; fi

# 7. The chart's real values.yaml (on a copy): bump, then bump again.
cp "$here/../values.yaml" "$work/real.yaml"
run "$work/real.yaml" sha-1234567
if [ "$rc" -eq 0 ] && grep -q '^  tag: sha-1234567' "$work/real.yaml"; then ok "the chart's values.yaml can be bumped"; else bad "real values.yaml: rc=$rc out='$out'"; fi
run "$work/real.yaml" sha-1234567
case "$out" in unchanged*) ok "and bumping it again is a no-op" ;; *) bad "real values.yaml, second run: '$out'" ;; esac
lines_before=$(wc -l < "$here/../values.yaml")
lines_after=$(wc -l < "$work/real.yaml")
if [ "$lines_before" -eq "$lines_after" ]; then ok "the real file keeps its line count"; else bad "line count $lines_before -> $lines_after"; fi

if [ "$fail" -eq 0 ]; then echo "bump-tag tests passed"; else echo "bump-tag tests FAILED"; exit 1; fi
