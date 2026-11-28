#!/bin/sh
# ANR-1427: every action a workflow uses stays at or above the major recorded in
# tools/workflow-pins.txt. Those floors are the majors that run on node24, and a
# pin below one is how a node20 action comes back: #495 added a job with
# `actions/checkout@v4` and `actions/setup-node@v4` after ANR-62 had moved every
# pin, and nothing went red until a person grepped (ANR-1410). A copied workflow
# is the usual way it happens, so this is a scan and not a convention. Run from
# the repository root (CI's `fmt` job):
#
#   sh tools/check-workflow-pins.sh
#
# A pin is read as `owner/repo@<ref>`: a tag (`v7`, `v7.0.1`) gives its major
# directly; a SHA must name it in a trailing `# vX.Y.Z` comment, which is only as
# good as its author (ANR-1428 corrects three stale ones, so a lie passes here);
# a branch ref (`stable`, `master`, `1.98`) is allowed only for an action the
# table records as `-`. A `uses:` of a local action (`./...`) is not a pin.
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

table=tools/workflow-pins.txt
failed=0
fail() { echo "$1" >&2; failed=1; }

# recorded_major <action>: prints the table's major (a number, or `-` for a
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
# the floor, is unversioned on an action that has a major, or names an action the
# table does not record.
check_ref() {
  where=$1
  line=$2
  ref=$(printf '%s\n' "$line" | sed -nE 's/^[[:space:]]*(-[[:space:]]+)?uses:[[:space:]]+([^[:space:]]+).*/\2/p')
  [ -n "$ref" ] || return 0
  case $ref in ./*) return 0 ;; esac
  action=${ref%@*}
  pin=${ref#*@}
  major=''
  case $pin in
    v[0-9]*)
      major=${pin#v}
      major=${major%%.*}
      case $major in *[!0-9]*) major='' ;; esac
      ;;
    *[!0-9a-f]* | '') ;; # a branch ref: no major to read
    *)
      # A 40-hex SHA: its major is the one its trailing comment names.
      major=$(printf '%s\n' "$line" | sed -nE 's/.*# *v([0-9]+)\..*/\1/p')
      if [ -z "$major" ]; then
        echo "$where: $ref is pinned by SHA with no '# vX.Y.Z' comment, so its major cannot be read" >&2
        return 1
      fi
      ;;
  esac
  if ! floor=$(recorded_major "$action"); then
    echo "$where: $action is not in $table: record it, so a fallback cannot pass" >&2
    return 1
  fi
  [ "$floor" = '-' ] && return 0
  if [ -z "$major" ]; then
    echo "$where: $ref is pinned by a branch, but $table records major $floor" >&2
    return 1
  fi
  if [ "$major" -lt "$floor" ]; then
    echo "$where: $ref is major $major, below the $floor recorded in $table" >&2
    return 1
  fi
  return 0
}

# A guard that has been narrowed (a lost comparison, a table lookup that always
# succeeds, a pattern that no longer matches) passes everything and says nothing,
# so every form is proven on a probe line before the tree is read. The refusals
# are what the check exists for; the allowances are what keeps it from being
# "fixed" by refusing the whole file.
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

probe_refused "a tag below the floor" '      - uses: actions/checkout@v4'
probe_refused "a SHA whose comment is below the floor" \
  '      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v4.2.2'
probe_refused "a SHA pin with no version comment" \
  '      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1'
probe_refused "a branch ref on an action that has a major" '      - uses: actions/checkout@main'
probe_refused "an action the table does not record" '      - uses: some/unknown-action@v1'
probe_allowed "a tag at the floor" '      - uses: actions/checkout@v7'
probe_allowed "a tag above the floor" '      - uses: actions/checkout@v9.1.2'
probe_allowed "a SHA whose comment is at the floor" \
  '      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1'
probe_allowed "a branch ref the table records as -" '      - uses: dtolnay/rust-toolchain@stable'
probe_allowed "a local action" '      - uses: ./.github/actions/thing'

for file in .github/workflows/*.yml .github/workflows/*.yaml; do
  [ -f "$file" ] || continue
  lineno=0
  while IFS= read -r line || [ -n "$line" ]; do
    lineno=$((lineno + 1))
    check_ref "$file:$lineno" "$line" || failed=1
  done < "$file"
done

if [ "$failed" -ne 0 ]; then exit 1; fi
echo "every workflow action pin is at or above its recorded major (tools/workflow-pins.txt)"
