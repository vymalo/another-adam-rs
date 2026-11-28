#!/bin/sh
# ANR-1427: every action a workflow uses stays at or above the major recorded in
# tools/workflow-pins.txt, so a pin cannot slip back to a major the repository
# has already left. It has happened: #495 added a job with
# `actions/checkout@v4` and `actions/setup-node@v4` after ANR-62 had moved every
# pin to a node24 major, and nothing went red until a person grepped (ANR-1410).
# A copied workflow is the usual way it happens, so this is a scan.
#
#   sh tools/check-workflow-pins.sh
#
# **What the floors are, and are not.** Each line of the table is the major this
# repository has moved to, which makes it an anti-regression floor: for
# `checkout`, `setup-node`, `upload-artifact` and `download-artifact` it is
# deliberately *above* the oldest node24 major, because those are the pins the
# fallback happened on. It is not a claim that every action at its floor runs on
# node24 — `Swatinem/rust-cache@v2.0.0` is node16, `taiki-e/install-action` and
# `sigstore/cosign-installer` are composite, `hadolint` and `cargo-deny` are
# Docker actions — and a minor version is never checked.
#
# A pin is read as `owner/repo@<ref>`: the whole string before the `@` is the
# table key, so a reusable workflow or a `docker://` image needs a row of its
# own. A tag (`v7`, `v7.0.1`) gives its major directly; a 40-character SHA must
# name it in the `# vX.Y.Z` comment that follows it, which is only as good as
# its author (ANR-1428 corrects three stale ones, so a lie passes here); a
# branch ref (`stable`, `master`, `1.98`) is allowed only where the table says
# `-`. A `uses:` of a local action (`./...`) is not a pin.
#
# Known limits, stated rather than hidden: a `uses:` inside a `run:` block is
# read as a pin; a `uses:` written in flow style, with a quoted key, with a
# space before the colon, or with its value on the next line is refused instead
# of passed (the message says which); and the major of a SHA pin is whatever its
# comment claims.
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

table=tools/workflow-pins.txt
failed=0
refs_seen=0
fail() { echo "$1" >&2; failed=1; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# validate_table <file>: every row that is not blank or a comment is
# `action major`, and the major is a number or `-`. A malformed floor would make
# the comparison below error, and under `set -e` inside an `if` that allows
# anything, so the table is checked before a pin is read.
validate_table() {
  vfile=$1
  bad=0
  row_no=0
  while IFS= read -r row || [ -n "$row" ]; do
    row_no=$((row_no + 1))
    case $row in
      '' | '#'*) continue ;;
    esac
    # shellcheck disable=SC2086 # split on purpose: a row is exactly two fields.
    set -- $row
    if [ "$#" -ne 2 ]; then
      echo "$vfile:$row_no: expected 'action major', read '$row'" >&2
      bad=1
      continue
    fi
    case $2 in
      -) ;;
      *[!0-9]*)
        echo "$vfile:$row_no: the floor '$2' is neither a number nor -" >&2
        bad=1
        ;;
      *) ;;
    esac
  done < "$vfile"
  return "$bad"
}

# recorded_major <action>: prints the table's floor (a number, or `-` for a
# branch-pinned action) and returns 1 when the action is absent.
recorded_major() {
  awk -v want="$1" '
    $1 ~ /^#/ { next }
    NF < 2 { next }
    $1 == want { print $2; found = 1; exit }
    END { exit(found ? 0 : 1) }
  ' "$table"
}

# check_ref <where> <line>: refuses the line, printing why, when its pin is below
# the floor, names no major on an action that has one, is written in a shape this
# line scan cannot read, or names an action the table does not record.
check_ref() {
  where=$1
  line=$2
  stripped=${line#"${line%%[! ]*}"}
  case $stripped in
    # Fail closed on the shapes a line scan would silently miss, rather than
    # report a pin it did not really read.
    '{'* | ','* | '- {'*)
      case $stripped in
        *uses*) echo "$where: flow-style uses: cannot be read; write it as a list item" >&2; return 1 ;;
      esac
      ;;
  esac
  if printf '%s\n' "$stripped" | grep -qE "^[\"']uses[\"'][[:space:]]*:|^uses[[:space:]]+:|^uses:[[:space:]]*$"; then
    echo "$where: this uses: cannot be read (quoted key, space before the colon, or the value on the next line); write 'uses: <ref>' on one line" >&2
    return 1
  fi
  ref=$(printf '%s\n' "$line" | sed -nE 's/^[[:space:]]*(-[[:space:]]+)?uses:[[:space:]]+([^[:space:]]+).*/\2/p')
  [ -n "$ref" ] || return 0
  refs_seen=$((refs_seen + 1))
  raw_ref=$ref
  case $ref in
    \"*\") ref=${ref#\"}; ref=${ref%\"} ;;
    \'*\') ref=${ref#\'}; ref=${ref%\'} ;;
  esac
  case $ref in ./*) return 0 ;; esac
  action=${ref%@*}
  pin=${ref#*@}
  # The floor first: an action the table records as `-` is branch-pinned, so a
  # SHA of it is a hardening edit with no version to write.
  if ! floor=$(recorded_major "$action"); then
    echo "$where: $action is not in $table: record it, so a fallback cannot pass" >&2
    return 1
  fi
  [ "$floor" = '-' ] && return 0
  major=''
  case $pin in
    v[0-9]*)
      major=${pin#v}
      major=${major%%.*}
      case $major in *[!0-9]*) major='' ;; esac
      ;;
    [0-9a-f]*)
      if [ "${#pin}" -eq 40 ]; then
        case $pin in *[!0-9a-f]*) ;; *) major=sha ;; esac
      fi
      ;;
  esac
  if [ "$major" = sha ]; then
    # Only the comment that follows this very ref: a later `# vX.` on the same
    # line must not answer for it. `raw_ref` keeps the quotes, so a quoted SHA
    # still finds its own comment.
    rest=${line#*"$raw_ref"}
    major=$(printf '%s\n' "$rest" | sed -nE 's/^[[:space:]]*# *v([0-9]+)\..*/\1/p')
    if [ -z "$major" ]; then
      echo "$where: $ref is pinned by SHA with no '# vX.Y.Z' comment right after it" >&2
      return 1
    fi
  fi
  if [ -z "$major" ]; then
    echo "$where: $ref is pinned by '$pin', which names no major, but $table records $floor" >&2
    return 1
  fi
  if [ "$major" -lt "$floor" ]; then
    echo "$where: $ref is major $major, below the $floor recorded in $table" >&2
    return 1
  fi
  return 0
}

# scan_file <path>: every line of a workflow or of a composite action; returns 1
# when any line was refused.
scan_file() {
  file_failed=0
  lineno=0
  while IFS= read -r line || [ -n "$line" ]; do
    lineno=$((lineno + 1))
    check_ref "$1:$lineno" "$line" || file_failed=1
  done < "$1"
  return "$file_failed"
}

# A guard that has been narrowed (a lost comparison, a table lookup that always
# succeeds, a pattern that no longer matches, a scan that reads nothing) passes
# everything and says nothing, so every form is proven before the tree is read:
# the refusals are what the check exists for, the allowances are what keeps it
# from being "fixed" by refusing the whole file.
probe_refused() { # probe_refused <what> <line>
  if ( check_ref probe "$2" ) >/dev/null 2>&1; then
    fail "the pin check no longer refuses $1: it was narrowed"
  fi
}
probe_allowed() { # probe_allowed <what> <line>
  if ! ( check_ref probe "$2" ) >/dev/null 2>&1; then
    fail "the pin check now refuses $1, which it must allow: it was narrowed"
  fi
}
probe_scan() { # probe_scan <what> <fail|pass> <workflow body>
  printf '%s\n' "$3" > "$tmp/probe.yml"
  if ( scan_file "$tmp/probe.yml" ) >/dev/null 2>&1; then
    [ "$2" = pass ] || fail "the scan no longer refuses $1: it was narrowed"
  else
    [ "$2" = fail ] || fail "the scan now refuses $1, which it must allow: it was narrowed"
  fi
}

probe_refused "a tag below the floor" '      - uses: actions/checkout@v4'
probe_refused "a dotted tag below the floor" '      - uses: actions/checkout@v4.2.2'
probe_refused "one major below the floor" '      - uses: actions/checkout@v6'
probe_refused "a uses: under a named step, with no dash" '        uses: actions/checkout@v4'
probe_refused "a quoted tag below the floor" '      - uses: "actions/checkout@v4"'
probe_refused "a SHA whose comment is below the floor" \
  '      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v4.2.2'
probe_refused "a SHA whose first comment is below the floor and a later one above" \
  '      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v4.2.2, was # v7.0.0'
probe_refused "a SHA pin with no version comment" \
  '      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1'
probe_refused "a short hex ref read as a branch" '      - uses: actions/checkout@cafe # v7.0.0'
probe_refused "a branch ref on an action that has a major" '      - uses: actions/checkout@main'
probe_refused "an action the table does not record" '      - uses: some/unknown-action@v1'
probe_refused "an unrecorded action at a high major" '      - uses: some/unknown-action@v999'
probe_refused "flow-style uses:" '      - {uses: actions/checkout@v4}'
probe_refused "a quoted uses: key" '      "uses": actions/checkout@v4'
probe_refused "a space before the colon" '      uses : actions/checkout@v4'
probe_refused "a value on the next line" '      uses:'
probe_allowed "a tag at the floor" '      - uses: actions/checkout@v7'
probe_allowed "a tag above the floor" '      - uses: actions/checkout@v9.1.2'
probe_allowed "a quoted tag at the floor" '      - uses: "actions/checkout@v7"'
probe_allowed "a SHA whose comment is at the floor" \
  '      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1'
probe_allowed "a SHA of a branch-pinned action" \
  '      - uses: dtolnay/rust-toolchain@d1f0d4a9a0c4e3a0b9f2e3b1a5c6d7e8f9012345'
probe_allowed "a branch ref the table records as -" '      - uses: dtolnay/rust-toolchain@stable'
probe_allowed "a local action" '      - uses: ./.github/actions/thing'
probe_allowed "a local action, quoted" '      - uses: "./.github/actions/thing"'
probe_scan "a workflow with a pin below the floor" fail \
  'jobs:
  x:
    steps:
      - uses: actions/checkout@v4'
probe_scan "a workflow whose pins are at the floor" pass \
  'jobs:
  x:
    steps:
      - uses: actions/checkout@v7'

# A floor that is not a number must be refused, or the comparison errors and
# (inside an `if`) allows anything.
printf '%s\n' 'actions/checkout v7' > "$tmp/pins-bad.txt"
if ( validate_table "$tmp/pins-bad.txt" ) >/dev/null 2>&1; then
  fail "a floor that is not a number is no longer refused: the table check was narrowed"
fi
printf '%s\n' 'actions/checkout 7' > "$tmp/pins-good.txt"
if ! ( validate_table "$tmp/pins-good.txt" ) >/dev/null 2>&1; then
  fail "a well-formed table row is now refused: the table check was narrowed"
fi

if [ ! -f "$table" ]; then
  echo "$table is missing: the pin check cannot read a floor without it" >&2
  exit 1
fi
validate_table "$table" || failed=1
for file in .github/workflows/*.yml .github/workflows/*.yaml; do
  [ -f "$file" ] || continue
  scan_file "$file" || failed=1
done
# A composite action of this repository carries pins of its own.
if [ -d .github/actions ]; then
  find .github/actions -type f \( -name 'action.yml' -o -name 'action.yaml' \) | sort > "$tmp/actions.txt"
  while IFS= read -r file; do
    scan_file "$file" || failed=1
  done < "$tmp/actions.txt"
fi

if [ "$refs_seen" -eq 0 ]; then
  fail "no uses: was read at all: the scan no longer matches the workflows"
fi
if [ "$failed" -ne 0 ]; then exit 1; fi
echo "every workflow action pin is at or above its recorded major (tools/workflow-pins.txt): $refs_seen uses: read"
