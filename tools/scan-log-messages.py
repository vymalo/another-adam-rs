#!/usr/bin/env python3
"""Find a `tracing` macro call whose **message** interpolates a value (ANR-1456).

A log record's body is the message, and the collector's allowlist (`adam_otel::logs::ALLOWED`) filters
fields only — so `warn!("{warning}")` or `warn!("{}", value)` exports the value verbatim. The message
is a **top-level string argument** of the macro: a string inside a nested call is a field's own
format (`error = %format!("{e:#}")`), not the body.

    python3 tools/scan-log-messages.py crates bin     # prints file:line: message, exits 1 on a hit

Written in Python because the shapes cannot be told apart line by line: a macro may be called bare
(`use tracing::warn`), the message may follow fields, and a field may carry its own format string.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

LEVELS = ("trace", "debug", "info", "warn", "error")
CALL = re.compile(r"(?:tracing::)?(?:" + "|".join(LEVELS) + r")!\(")


def messages(path: Path) -> list[tuple[int, str]]:
    """Every message of a macro call in `path` that contains a `{` (an interpolation)."""
    text = path.read_text(encoding="utf-8", errors="replace")
    found: list[tuple[int, str]] = []
    for match in CALL.finditer(text):
        i = match.end()
        depth = 0
        strings: list[str] = []
        while i < len(text):
            c = text[i]
            if c == '"':
                j = i + 1
                buf: list[str] = []
                while j < len(text):
                    if text[j] == "\\":
                        buf.append(text[j : j + 2])
                        j += 2
                        continue
                    if text[j] == '"':
                        break
                    buf.append(text[j])
                    j += 1
                if depth == 0:
                    strings.append("".join(buf))
                i = j + 1
                continue
            if c == "(":
                depth += 1
            elif c == ")":
                if depth == 0:
                    break
                depth -= 1
            i += 1
        line = text.count("\n", 0, match.start()) + 1
        for message in strings:
            # `{{` is a literal brace, not an interpolation.
            if "{" in message.replace("{{", ""):
                found.append((line, message))
                break
    return found


def main(argv: list[str]) -> int:
    roots = argv[1:] or ["crates", "bin"]
    failed = False
    for root in roots:
        base = Path(root)
        if not base.exists():
            continue
        paths = [base] if base.is_file() else sorted(base.rglob("*.rs"))
        for path in paths:
            if "tests" in path.parts:
                continue
            for line, message in messages(path):
                print(f"{path}:{line}: the message interpolates a value: {message!r}")
                failed = True
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
