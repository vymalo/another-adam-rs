#!/bin/sh
# ANR-1456, widened by ANR-1458: a log record's body is the message its macro wrote, and the
# collector's allowlist (`adam_otel::logs::ALLOWED`) cannot drop it — only fields are filtered. A
# message that interpolates a value therefore puts that value in every exported record: a startup
# warning's path, a provider's reason, a panic payload, an agent program's stderr. The value belongs
# in a field, where the allowlist decides, and the message stays static. Run from the repository
# root (CI's `fmt` job):
#
#   sh tools/check-log-messages.sh
#
# The scan is `tools/scan-log-messages.py`: it reads the macro's **top-level string arguments**, so it
# sees a bare `warn!` from `use tracing::warn`, a message after fields, and a message continued over
# several lines — and it does not mistake a field's own `format!` for the message. Test modules are
# skipped: a fixture renders a message on purpose.
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

failed=0
fail() { echo "$1" >&2; failed=1; }

# A guard that has been narrowed passes everything and says nothing, so each shape is proven on a
# fixture before the tree is read: an inline interpolation, a message after fields, a bare import, a
# message followed by its values, a field's own format string (which is not the message), a literal
# brace, and a test module (which is not scanned).
probe_dir=$(mktemp -d)
trap 'rm -rf "$probe_dir"' EXIT
must_flag() { # must_flag <what> <line>: the scanner exits 1 on a hit
  printf '%s\n' "$2" > "$probe_dir/probe.rs"
  if python3 tools/scan-log-messages.py "$probe_dir/probe.rs" >/dev/null 2>&1; then
    fail "the log-message scan no longer sees $1: it was narrowed"
  fi
}
must_pass() { # must_pass <what> <line>: no hit, so the scanner exits 0
  printf '%s\n' "$2" > "$probe_dir/probe.rs"
  if ! python3 tools/scan-log-messages.py "$probe_dir/probe.rs" >/dev/null 2>&1; then
    fail "the log-message scan now refuses $1, which it must allow: it was narrowed"
  fi
}

for level in trace debug info warn error; do
  must_flag "an interpolated $level" "tracing::$level!(\"a value: {value}\");"
  must_flag "an interpolated $level after fields" "tracing::$level!(run = %r, \"a value: {value}\");"
done
must_flag "a message followed by its values" 'tracing::warn!("{}", value);'
must_flag "a bare import" 'warn!(program = %p, "{}", text);'
must_flag "a message continued over two lines" 'tracing::warn!("a value: \
    {value}");'
must_pass "a static message" 'tracing::warn!("a static message");'
must_pass "a static message with a field" 'tracing::warn!(warning = %warning, "a static message");'
must_pass "a field's own format string" 'tracing::error!(error = %format!("{e:#}"), code, "a static message");'
must_pass "a literal brace" 'tracing::warn!("a {{literal}} brace");'
must_pass "a test module" 'fn x() { tracing::warn!("a static message"); }
#[cfg(test)]
mod tests { fn y() { tracing::warn!("{v}"); } }'

hits=$(python3 tools/scan-log-messages.py crates bin || true)
if [ -n "$hits" ]; then
  printf '%s\n' "$hits" >&2
  fail "a tracing message interpolates a value: log a static message and put the value in a field (ANR-1456)"
fi

if [ "$failed" -ne 0 ]; then exit 1; fi
echo "every tracing message is static; a value goes in a field, where the logs allowlist decides"
