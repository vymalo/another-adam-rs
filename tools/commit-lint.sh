#!/usr/bin/env sh
# The Conventional Commits check, the one rule for a commit's subject in this repository. The local
# commit-msg hook (.githooks/commit-msg) and CI (.github/workflows/commit-lint.yml, the pull request's
# title and each of its commits) both call it, so the rule cannot drift. release-please reads these
# subjects to choose the next version and write the changelog (ADR 0084); a pull request is squashed, so
# its title is the commit that lands on main.
#
#   tools/commit-lint.sh --file <path>        the first line of a message file (the hook)
#   tools/commit-lint.sh --message "<line>"   one subject (CI)
#
# Exit 0: valid, or not ours to shape (merges, git's revert, fixup and squash subjects). Exit 1:
# refused, with the reason. Exit 2: usage. POSIX sh.
set -eu

# feat and fix move the version (feat a minor, fix a patch, `!` a breaking change, which before 1.0
# moves the minor); the others are listed in the changelog (style is hidden). Keep in sync with the
# changelog-sections of release-please-config.json and with CLAUDE.md, "Commits and pull requests".
TYPES='feat|fix|docs|style|refactor|perf|test|build|ci|chore|revert|deps'

# type(optional scope)(optional !): description
#   scope: lowercase, `-`, `.`, `_`, `/`, and `,` between two scopes: feat(engine,deploy): ...
PATTERN="^(${TYPES})(\([a-z0-9._/,-]+\))?!?: .+"

usage() { echo "usage: $0 --file <path> | --message <subject>" >&2; exit 2; }

[ $# -eq 2 ] || usage
case "$1" in
  --file) subject=$(sed -n '/^[^#]/{p;q;}' "$2") ;; # the first line that is not a comment
  --message) subject=$2 ;;
  *) usage ;;
esac

case "$subject" in
  'Merge '*|'Revert "'*|'fixup! '*|'squash! '*|'amend! '*) exit 0 ;;
esac

if printf '%s\n' "$subject" | head -n 1 | grep -Eq "$PATTERN"; then
  len=$(printf '%s' "$subject" | wc -c | tr -d ' ')
  if [ "$len" -gt 100 ]; then
    echo "commit-lint: note: the subject is $len characters; a changelog reads better under 100." >&2
  fi
  exit 0
fi

cat >&2 <<MSG
commit-lint: not a Conventional Commit:

    ${subject}

  the shape:   <type>[(scope)][!]: <description>
  types:       feat fix docs style refactor perf test build ci chore revert deps
  scope:       optional, lowercase, two joined by a comma: feat(engine,deploy): ...
  !:           a breaking change: feat(service)!: ...

  examples:    feat(a2a): serve ListTasks
               fix(coder): keep the branch when a check fails
               docs(adr): 0084, releases by release-please
               chore(deploy): bump coder to sha-1234567

  The type picks the next version and the changelog section (ADR 0084).
MSG
exit 1
