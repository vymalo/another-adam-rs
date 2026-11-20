#!/bin/sh
# Compare each catalog fixture of adam-ui with the published file at the commit that
# `crates/adam-ui/tests/fixtures/UPSTREAM` names, and the conformance blocks of
# `choices-v7-vectors.json` and `choices-v8-vectors.json` with the ones on the contract page at
# their commit, and the A2UI v1.0 schemas of `crates/adam-ui/tests/a2ui-v1_0/` with the upstream files at the
# commit its `UPSTREAM` names. A catalog line with a fourth field (the path of a page) is the body of the page's
# `json catalog-<n>` block at that commit, not a file. Needs curl, node and
# the network (CI, job `docs`). `UI_CATALOG_CONTRACT=<file>` reads the contract page from a file
# instead of fetching it; `UI_CATALOG_CONTRACT_V2=<file>` does the same for the `ui-catalog-v2` page that
# the catalogs from version 9 are read from.
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
fixtures="$root/crates/adam-ui/tests/fixtures"
base=https://raw.githubusercontent.com/vymalo/another-agentic-system
path=web/src/features/chat/lib/a2ui/catalog/catalog.json
tmp=$(mktemp)
trap 'rm -f "$tmp" "$tmp.page"' EXIT

failed=0
while read -r file commit digest page; do
  case "$file" in
    catalog-v*.json)
      if [ -n "$page" ]; then
        # The catalog is the body of the page's `json catalog-<n>` block.
        n=${file#catalog-v}
        n=${n%.json}
        if [ -n "${UI_CATALOG_CONTRACT_V2:-}" ]; then
          cp "$UI_CATALOG_CONTRACT_V2" "$tmp.page"
        else
          curl -sSfL -o "$tmp.page" "$base/$commit/$page"
        fi
        node --input-type=module - "$tmp.page" "$n" "$tmp" <<'JS'
import { readFileSync, writeFileSync } from "node:fs";

const [page, n, out] = process.argv.slice(2);
const found = readFileSync(page, "utf8").match(new RegExp(`^\`\`\`json catalog-${n}\n([\\s\\S]*?)^\`\`\`$`, "m"));
if (!found) {
  console.error(`MISSING  the page has no \`json catalog-${n}\` block`);
  process.exit(1);
}
writeFileSync(out, found[1]);
JS
        src="the \`json catalog-$n\` block of $page"
        rm -f "$tmp.page"
      else
        curl -sSfL -o "$tmp" "$base/$commit/$path"
        src="$base/$commit/$path"
      fi
      if cmp -s "$tmp" "$fixtures/$file"; then
        echo "ok       $file at $commit ($digest)"
      else
        echo "DIFFERS  $file from $src" >&2
        failed=1
      fi
      ;;
    choices-v*-vectors.json)
      # `digest` is the path of the page here.
      case "$file" in
        choices-v7-vectors.json) blocks='{"vectors":"vectors","whole-answers":"wholeAnswers","invalid-questions":"invalidQuestions","surfaces":"surfaces","questions":"questions","answers":"answers"}' ;;
        choices-v8-vectors.json) blocks='{"v8-surfaces":"v8Surfaces"}' ;;
        *) echo "UNKNOWN  $file has no list of blocks in $0" >&2; failed=1; continue ;;
      esac
      if [ -n "${UI_CATALOG_CONTRACT:-}" ]; then
        cp "$UI_CATALOG_CONTRACT" "$tmp"
      else
        curl -sSfL -o "$tmp" "$base/$commit/$digest"
      fi
      if node --input-type=module - "$tmp" "$fixtures/$file" "$blocks" <<'JS'
import { readFileSync } from "node:fs";
import { isDeepStrictEqual } from "node:util";

const [page, fixture, blocks] = process.argv.slice(2);
const wanted = JSON.parse(blocks);
const found = {};
for (const [, tag, body] of readFileSync(page, "utf8").matchAll(/^```json (\S+)\n([\s\S]*?)^```$/gm)) {
  if (tag in wanted) found[wanted[tag]] = JSON.parse(body);
}
const vendored = JSON.parse(readFileSync(fixture, "utf8"));
let ok = true;
for (const key of Object.values(wanted)) {
  if (!(key in found)) {
    console.error(`MISSING  the page has no \`json\` block for ${key}`);
    ok = false;
  } else if (!isDeepStrictEqual(found[key], vendored[key])) {
    console.error(`DIFFERS  ${key} of the vendored file is not the page's block`);
    ok = false;
  }
}
const extra = Object.keys(vendored).filter((key) => !Object.values(wanted).includes(key));
if (extra.length > 0) {
  console.error(`EXTRA    the vendored file has ${extra.join(", ")}, which the page does not have`);
  ok = false;
}
process.exit(ok ? 0 : 1);
JS
      then
        echo "ok       $file at $commit (the blocks of $digest)"
      else
        echo "DIFFERS  $file from the blocks of $digest at $commit" >&2
        failed=1
      fi
      ;;
    *) continue ;;
  esac
done <"$fixtures/UPSTREAM"

# The A2UI v1.0 schemas: each file is the upstream file at the commit, and has the SHA-256 the record gives.
schemas="$root/crates/adam-ui/tests/a2ui-v1_0"
repo=$(sed -n 's/^repo=//p' "$schemas/UPSTREAM")
commit=$(sed -n 's/^commit=//p' "$schemas/UPSTREAM")
while read -r file sha upath; do
  case "$file" in
    ''|'#'*|repo=*|commit=*) continue ;;
  esac
  curl -sSfL -o "$tmp" "https://raw.githubusercontent.com/$repo/$commit/$upath"
  if ! cmp -s "$tmp" "$schemas/$file"; then
    echo "DIFFERS  $file from $repo at $commit ($upath)" >&2
    failed=1
  elif [ "$(sha256sum "$schemas/$file" | cut -d' ' -f1)" != "$sha" ]; then
    echo "SHA      $file is the upstream file but UPSTREAM records another SHA-256" >&2
    failed=1
  else
    echo "ok       $file at $repo $commit ($sha)"
  fi
done <"$schemas/UPSTREAM"
exit "$failed"
