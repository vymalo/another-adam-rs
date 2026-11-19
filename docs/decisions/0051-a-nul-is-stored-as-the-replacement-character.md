# 0051. A NUL is stored as the replacement character

Status: **Accepted** (2026-11-18). The fix for a run that failed after its pull request was open: a command's output
held a NUL byte, PostgreSQL's `JSONB` refused the run's state, and the task ended `failed`
(*verified 2026-11-18*: the smoke test's run, SQLSTATE `22P05` from
`crates/adam-store-postgres/src/lib.rs`).

## Context

A tool's result is text the tool did not write: a command's output, an MCP server's answer, a file's bytes decoded as
UTF-8. A NUL (`\u0000`) is valid UTF-8 and can appear in any of them. The loop journals the result, applies it to the
conversation and stores both; PostgreSQL's `JSONB` cannot hold a NUL in a string or a key, and BSON holds one in a
string value but not in a field name (the MongoDB codec escapes a key as `%00`).
Two problems follow:

* a run fails on the store it happens to run on, after its work is done, with a failure no change of the code can
  cure;
* a value written through one backend would not be the value read through another, so a run's bytes would depend on
  where it ran.

`StoreError::InvalidInput` was the contract for this: a NUL was data "the store cannot hold". The runtime treats that
class as permanent, so the run failed.

## Decision

1. **The loop sanitizes a tool's output before it is journaled.** `ToolOutput::sanitized` replaces a NUL with U+FFFD in
   every text a tool can write — the content, the announced answer, the repeat key, a kept output, the plan's title and
   items, an artifact's name, media type and JSON data — and one call in `agent.rs`, inside the closure `Ctx::step`
   journals, applies it. So the journaled step and the model's message are the same bytes whatever store is behind the
   run, and a replay shows exactly what the live run showed.
2. **Every adapter replaces a NUL with U+FFFD before it writes**, in a string and in an object key, at any depth, for
   every JSON value it stores: a run's state, a journal payload, a push configuration and its cursor. The one function
   is `adam_core::storable` (and `storable_text` for one string); it borrows the value when there is nothing to
   replace, so the common path copies nothing. U+FFFD is the replacement character, and it is what the coder's kept
   outputs already write for a NUL. This is the backend contract, and it covers the text the loop did not write: the
   model's own answer, a compaction summary, a person's message.
3. **A store returns what it stored.** `create_run`, `commit_run` and `journal_put` return the value as it was written
   (the adapter produces it from what it wrote), so the loop applies the same bytes it journaled and a replay decodes the same value.
   The live transition and the replay therefore agree, and the model's next request is built from the stored value.
   *Amended 2026-11-18:* For these three methods, MongoDB builds the returned record and entry from the values it
   wrote instead of converting the stored document back to JSON; the requirement is unchanged.
4. **The testkit requires it.** `nul_characters_are_stored_as_the_replacement_character` replaces the old
   `nul_characters_roundtrip_or_reject_cleanly`, which accepted either behaviour: every adapter must now round-trip a
   NUL as U+FFFD, in the state and in a journal payload, for values and for keys.
5. **A rejection is a bug, not a contract.** `InvalidInput` keeps its other cases (a number beyond the column, a bad
   table prefix). PostgreSQL's `22P05` mapping stays as a defensive net, and its message says a NUL reached the store
   unsanitized, which is a bug in the caller of `storable`.

## Consequences

* A tool that prints a NUL no longer fails the run, and the bytes of a run are the same in every backend.
* The stored text is not byte-for-byte the tool's text: one character became U+FFFD. That is deliberate, and it is
  the only character a backend cannot hold.
* A NUL in a key can make two keys equal after the replacement (the later one wins). A key with a NUL is
  pathological; the alternative was a failed run.
* **This covers JSON values only.** A NUL in a text column (a push configuration's `id`, a delivery
  `last_error`, a journal step name) is the caller's, not a tool's output: it is still refused, now as
  `InvalidInput` for SQLSTATE `22021` as for `22P05`, instead of an opaque backend error.
* `adam_core::storable` and `storable_text` are new public API, re-exported from `adam-core`; `MemoryStore` applies
  the rule too, so the reference adapter passes the same testkit as the databases. `ToolOutput::sanitized` is
  `pub(crate)`: the loop applies it, a tool author does not.
* No trait method changed. The store READMEs, the store-adapters reference and the `InvalidInput` doc say the rule.
