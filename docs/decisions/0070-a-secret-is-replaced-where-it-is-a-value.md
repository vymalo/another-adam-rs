# 0070. A secret is replaced where it is a value, and the mark says what happened

Status: **Accepted** (2026-11-23). Settled from the bench's re-analysis (epic #359, the lean-request series): the coder
image in a bench run carried `StaticDatabaseURI: "[redacted]://localhost:5432"` in what the model wrote, because the
database password was `postgres`, an ordinary word, and the redactor replaced it everywhere — inside the DSN's scheme
and inside identifiers — with a mark (`[redacted]`) that read like a value the model could paste back.

## Context

Every redactor in the tree replaced a registered value at **every** occurrence. That is right for a stream, and wrong
for code:

- **A secret that is also an ordinary word is everywhere in code.** `postgres`, `password`, `admin`, `secret`: a
  Dockerfile's `FROM postgres:16`, a `postgres-config.yaml`, `fn postgres_handler`, `my_postgres_pool`. Replacing them
  rewrites the code a model must read and edit, and the bench measured what follows: the model pasted the mark back
  as a value.
- **The mark was a placeholder.** `[redacted]` looks like something to fill in, and it says nothing about why a value
  is missing.
- **The streaming scrubber cannot take a lookbehind.** Its replacement may not depend on where a chunk starts, or a
  text split at every byte would leak a value's tail (`kept.rs`, `StreamScrubber`). So one rule cannot serve both.

## Decision

**1. One mark, from one constant.** `adam_llm_agent::REDACTED_MARKER` =
`[redacted: secret removed, do not reuse]`, used by the three redactors (`adam-code-tools`, `adam-agent`,
`adam-mcp`). It says what happened, so a model that reads a result does not copy the mark back as a value.

**2. Two scrubs, because the sites differ.**

- `scrub`, `scrub_string` and `failure_text` replace **every** occurrence. They carry **messages** — a failure text, a
  tool error, a question — which leave the process as free text: a value glued into a longer token there is a leak, not
  a word.
- `scrub_values`, `scrub_value`'s string values and `step_io` apply the **whole-value rule**
  (`adam_llm_agent::is_value_position`) to the text a model reads back **as code**: a tool's result, a kept output, a
  file an artifact carries, a JSON string value, a step's input and output.

**3. The rule, and its scope.** A value stands at the start or end of the text, or between bytes that cannot be part
of an identifier (anything but ASCII alphanumerics and `_`), and is not the scheme of a URL (`postgres://…`). It applies
only to a needle that is an **ordinary word** — lowercase ASCII letters only. Any other needle (a digit, a capital,
`_`, `-`, a Base64 form) can never be an identifier in code, so it is replaced wherever it stands: a Base64 form glued
to other text (`x cG9zdGdyZXM=abc`) is not kept because the byte after it is a letter.

**4. A mark already in the text is not scanned again.** The coder settles an output with one scrub and the redacting
layer scrubs the result with another; a registered value that is a word of the mark (`secret`) would nest inside it.

**5. Object keys are not scrubbed** (`scrub_value`): a key names a field, and a held value that is also a word would
otherwise rename it. The ruling is S-4G7Y's (2026-11-22).

**6. `adam-mcp` keeps the in-word rule**, and changes only its mark: its output is untrusted and a value may be
embedded in a larger token, so the rule would under-redact there.

## Alternatives

- **Keep replacing everywhere, with a longer mark.** Fixes the paste-back half and none of the code half.
- **A dictionary of ordinary words.** Needs a dictionary per language, in the binary, for a heuristic that a lexical
  test (`is_ordinary_word`) approximates: a value with a digit or a capital is not an identifier in code.
- **An opaque mark (`[REDACTED]`).** No collision with any word, and it reads as a value: the bench's second harm
  stays. The mark's wording is instead kept out of the `secret`-shaped words as far as it can be, and the one collision
  that remains (a value equal to a word of the mark) is indistinguishable in the output, so it tells nothing.
- **A lookbehind in the streaming scrubber.** Breaks the split-at-every-byte invariant.

## Consequences

- A secret that *is* an identifier in code is kept at a whole-value site. The message and streaming scrubs still
  replace it, and the bench's DSN is fixed.
- The other code positions are still replaced (`FROM postgres:16`, `postgres-config.yaml`, a bare
  `let engine = "postgres"`): an ordinary word cannot be told from an identifier without a dictionary, and the trade
  is to keep the one the bench measured.
- A value that is also a URL scheme (`postgres`) stays as the scheme, on purpose: the alternative is `[mark]://`, the
  bench's harm.
- A JSON key leaves the process unscrubbed. A high-entropy key in a JSON object is a known cost of decision 5.
