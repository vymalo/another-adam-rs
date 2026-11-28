#!/bin/sh
# ANR-1456: a log record's body is the format string its macro wrote, and the collector's allowlist
# (`adam_otel::logs::ALLOWED`) cannot drop it — only fields are filtered. A message that interpolates
# a value therefore puts that value in every exported record: a startup warning's path, a provider's
# reason, a panic payload. The value belongs in a field, where the allowlist decides, and the message
# stays static. Run from the repository root (CI's `fmt` job):
#
#   sh tools/check-log-messages.sh
#
# It reads every `tracing::trace|debug|info|warn|error!` whose message is on the macro's own line and
# interpolates. A message written on the next line is not seen (the pattern is line-anchored); that
# shape is rare and the review of ANR-1456 found none.
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

# The pattern, as a function so that the probe below runs exactly what the scan runs.
scan() { grep -rnE 'tracing::(trace|debug|info|warn|error)!\(\s*"[^"]*\{' --include='*.rs' "$@"; }
# Test fixtures render messages on purpose; the rule is about what the process logs.
scan_tree() { scan "$@" | grep -v '/tests/' || true; }

failed=0
fail() { echo "$1" >&2; failed=1; }

# A guard that has been narrowed (a lost `\{`, a level dropped from the alternation) passes
# everything and says nothing, so the pattern is proven on a line it must match first.
probe_dir=$(mktemp -d)
probe=$probe_dir/probe.rs
trap 'rm -rf "$probe_dir"' EXIT
for level in trace debug info warn error; do
  printf 'tracing::%s!("a value: {value}");\n' "$level" > "$probe"
  if ! scan "$probe" >/dev/null; then
    fail "the log-message scan no longer matches an interpolated $level: it was narrowed"
  fi
done
printf 'tracing::warn!(warning = %%warning, "a static message");\n' > "$probe"
if scan "$probe" >/dev/null; then
  fail "the log-message scan now refuses a static message with a field: it was narrowed"
fi

hits=$(scan_tree crates bin)
if [ -n "$hits" ]; then
  printf '%s\n' "$hits" >&2
  fail "a tracing message interpolates a value: log a static message and put the value in a field (ANR-1456)"
fi

if [ "$failed" -ne 0 ]; then exit 1; fi
echo "every tracing message is static; a value goes in a field, where the logs allowlist decides"
