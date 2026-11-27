# Architecture: the evidence

For people: [Architecture](../../docs/architecture.md). This page holds what the page for people rests on: what was
read or run to check it, at which commit, and what is still unverified. Re-read a claim here before you build on it,
and move it from *Unverified* to *Verified* (with the date and the source) when you check it.

## Verified and unverified

**Verified 2026-09-29, reading this repository at `172a117`:** the ports and their implementations, the
run transitions, the request path and the coder's rules. **Re-read 2026-10-06:** the crate graph (from
`cargo metadata`) and the schema (`crates/adam-store-postgres/src/lib.rs`, `crates/adam-store-mongodb/src/lib.rs`).
The Mermaid syntax of every diagram is checked in CI by `tools/docs-check`.

**Verified 2026-09-29, executed:** the cross-process fan-out and the listener lifecycle
(`crates/adam-notify-postgres/tests/two_runtimes.rs`, PostgreSQL 16) and the `NOTIFY` payload limit of
8000 bytes (16.13 server); the continuation of a finished task (ADR 0003).

**Verified 2026-10-07, executed:** Swagger UI at `/docs` of `adam-agent` (PostgreSQL 16, a stub model) in headless
Chromium: rendered under its CSP with no error and no request to another origin, Authorize then Try it out over both
bindings ([ADR 0031](../../docs/decisions/0031-swagger-ui-and-the-a2a-rest-binding.md)).

**Unverified:**

* The `sysexits.h` numbers 78, 69, 71 and 70 are from memory.
* The A2A method names, `TASK_STATE_*` names and the codes `-32001` and `-32002` are as documented in
  `crates/adam-a2a/src/lib.rs` for the pinned SDK (`a2a-lf` 0.3, `a2a-server-lf` 0.4), not checked against
  the A2A specification here. Neither is the SDK's SSE keepalive (every 15 s).
* `adam-acp`'s notes on OpenCode's behaviour (ACP over stdio) are as recorded in its README; the coder
  no longer starts OpenCode (ADR 0038), and a live run against a real gateway is the nightly `live-opencode` job.
* The run pods (ADR 0019) were tested against a fake API server and a `kind` cluster, not the owner's.
* The chart was rendered, not applied to a cluster; how it reaches the cluster after the tag bump is
  outside this repository.

## Signed turns on re-entry (ADR 0068, slice T3a-3)

For people: [Signed turns](../../docs/architecture.md#signed-turns-each-assistant-and-tool-turn-is-signed-when-it-is-committed).

* **Where.** `Sealer::check_read` (`crates/adam-runtime/src/seal.rs`) is the one function every row-reading decode calls
  (`join_head`, `check_full`, `verify_child`'s unsigned path, `worker.rs` `verify_reloaded`). After the head check it calls
  `Sealer::check_turns`: one `key_set()` read, then one `verify_turn` per row whose `role` is `assistant` or `tool`, at its
  index, `prv` folded over the rows as stored (`storable`). A row that is not a turn is chained, not checked. So the re-entries
  are covered by what already reads rows: a claim (resume, restart, another worker), `prior_state` (a new task that continues
  a conversation; `start_child_continuing`, a session round), `Runtime::state`, and `Engine` over any store, since the engine
  only runs a `Runtime`. `check_sealed` (a head by its own `seal`, no rows read) does not check turns, by design.
* **Strict when signed.** A head with a signature makes every turn row mandatory: an unsigned one is `Corrupt`
  (`TurnSignatureError::Unsigned` as the source). A runtime with keys therefore needs agents that sign: give the
  `LlmAgent` the same `turn_keys` (the engine and service wiring is T2). A test agent with role-shaped rows must sign them.
* **Adoption (decision 11).** Unchanged from T3a-2: an unsigned head with no signed row is adopted, nothing is checked; the
  next commit that writes rows signs the head, and the `LlmAgent` signs every unsigned turn. An unsigned head over a signed row
  is `Corrupt`. Test: `tests/turn_reentry.rs` `a_legacy_run_is_adopted_once_and_checked_in_full_after`.
* **Errors.** `RuntimeError::Corrupt` with the position and role in `reason`, the `TurnSignatureError` as source (no row
  content); a key set that cannot be read is `RuntimeError::TurnKeys`, so a claimed run keeps its lease (T3a-2 point 1).
* **Re-signing.** `continue_conversation` (`crates/adam-llm-agent/src/agent.rs`) calls `signing::unsign_from(messages, 0)`
  on the carried rows, so `answer_owed_calls`' synthetic results, the cut tail and every old signature go together and
  `sign_transition` signs them under the new run at its first commit. The carried rows were verified under their own run by
  `prior_state` before they got there. A compaction and a shortening already `unsign_from` the changed rows (T3a-1).
  A starter has no keys, so the strip is unconditional: a keyless process carries rows unsigned, as a legacy run.
* **Cost.** One Ed25519 verify and one canonical-JSON form per turn on every decode that reads the rows, beside the one
  SHA-256 pass the head already needed. Decision 4 said one check for the newest turn would do; this checks all of them
  (a rotation could revoke a key that signed an early turn).
* **Tests.** `crates/adam-runtime/src/seal.rs` `turn_tests` (the head verifies, the turn does not: forged assistant, forged tool,
  changed user, spliced, unsigned, reordered, revoked key, a non-turn row left alone, key set down); `crates/adam-llm-agent/tests/turn_reentry.rs`
  (resume, new task, session round, each with four forgeries and no model call; a carry signed again under the new run; adoption once;
  revoked key).

## Signed heads (ADR 0068, slice T3a-2)

For people: [Signed turns](../../docs/architecture.md#signed-turns-each-assistant-and-tool-turn-is-signed-when-it-is-committed).
Written 2026-11-26 on top of #453.

* **Payload.** `adam_core::turn::head_payload` (`crates/adam-core/src/turn/chain.rs`): JCS of `{typ: "adam-head/v1", run,
  cnv, len, chain, head}`. Test `tests/turn_signature.rs` `the_head_signature_binds_each_member_of_its_payload`.
* **`head`** is the stored state document minus `signature`, `seal` and `body.generation`, through `storable`
  (`crates/adam-runtime/src/seal.rs` `signed_head`). The generation is excluded because the store patches it after the
  runtime signs (`set_head_body`); `len` is in the payload.
* **Write sites.** Step commit: `worker.rs` `seal_update`, after `build_update`, in the CAS retry loop (a v5 head with a
  split: `Seal::over(next.split rows)`; a head that carries its rows: the base `seal`; a v4 state: the chain over
  `Inner::transcript_of(agent, next.agent)`). Delivery and cancel: `Inner::reseal` (`runtime.rs`), after the last mutation
  and before `carried`. A signing failure at commit is a `StoreError::Backend` with the keys' class: the run keeps its
  lease. Not re-signed because the state is unchanged: `park_again`, the failed-run write of `transition`.
* **Read sites.** `Inner::join_head` (after the row prefix is complete, before `join`), `join_stored` and `claimed` for a
  v4 state (`check_full`). They verify against `rec.state`, the record as read. Everything that decodes goes through one of
  them: `Runtime::state`, `view` does not (it reads the head only).
* **Adoption.** `Sealer::check_read`: no `signature` and no row with `extra_content.adam.turn_signature` is accepted; any
  turn signature without a head signature is `Corrupt`. A delivery or cancel never signs an unsigned document.
* **`seal`** `{len, chain}` is a head-only carry; a reader recomputes. A v4 state has none (its chain is recomputed from
  the agent's `split`; `transcript` moved to `ErasedStarter` so a start-only runtime can do it).
* **Errors.** `RuntimeError::Corrupt` (source `TurnSignatureError`) for a refused head, new `RuntimeError::TurnKeys` for a
  key set that cannot be read or keys that cannot sign. A claimed run that does not verify is committed `Failed`, state as
  found, by the existing unreadable-state path.
* **Review of #454 (2026-11-26).** `Inner::transcript_of(run, agent, state)` is `Result<Option<rows>>`: `None` for an agent
  not registered here (the v4 chain check is skipped, `check_full`; `reseal` writes unsigned), `Corrupt` for a state that does
  not decode. `worker.rs` `transition`: a `RuntimeError::TurnKeys` from `claimed` returns `false` (lease kept), only `Corrupt`
  commits `Failed`. `Inner::reseal` swallows only `Corrupt` when not strict. `Inner::verify_child` (`runtime.rs`) is called by
  `Ctx::child_status` (terminal children) and `Ctx::child_look`; `Sealer::check_sealed` checks a head by its own `seal`.
  `worker.rs` `verify_reloaded` checks the record the CAS retry reloads; `seal_update` refuses a signed base with no usable
  seal (`StoreError::Corrupt`). Tests: `tests/head_signature.rs` (outage, front end, cancel, child), `worker.rs` `claim_tests`.
  Broken on purpose: `verify_reloaded` and `verify_child` made to return `Ok` fail their tests.
* **Checked 2026-11-26:** `cargo test -p adam-core -p adam-runtime --locked`; breaking `seal.rs` `check_read` (chain over no
  rows) fails 6 of 12 cases of `head_signature.rs`.

## Signed turns at commit (ADR 0068, slice T3a-1)

For people: [Signed turns](../../docs/architecture.md#signed-turns-each-assistant-and-tool-turn-is-signed-when-it-is-committed).
Read 2026-11-26 at the head of PR #453.

* **Seam.** `LlmAgent::step` calls `step_unsigned`, then `sign_transition` (`crates/adam-llm-agent/src/agent.rs`), which
  signs the state of every `Transition` variant through `sign_messages` (`crates/adam-llm-agent/src/signing.rs`). No keys:
  the transition is returned untouched.
* **What is signed.** Each assistant or tool row with no `extra_content.adam.turn_signature`, at its index, with `prv` the
  chain over the rows before it as stored; the chain is folded once, in order. Rows after the last unsigned one are not read.
* **As stored.** `make_storable` runs `adam_core::storable` (the store's own rule, ADR 0051) on every row it reads, and the
  message is replaced by its storable form, so the signed bytes equal the stored bytes. Tests: `signing.rs`
  `a_nul_is_replaced_before_it_is_signed`; `tests/turn_signatures.rs` `rows_with_a_nul_verify_as_the_store_keeps_them`
  (text, an argument string and an argument key, read back from `MemoryStore`).
* **Stale signatures.** `signing::unsign_from(messages, from)` removes adam's member (and an emptied `extra_content`) from
  `from` on. Callers: `compaction.rs` after the transcript is rebuilt (`from = 1`, the tail moved) and `agent.rs` after
  `history::record_shortenings_from` (`from` = the first shortened row). Not covered, by design of later slices: a
  continued run's carried rows, and the in-place rewrite of the last user message (`rewrite_last_user`, the image
  extension), which happen before a later signed row exists. Tests: `tests/compaction.rs`
  `a_compacted_transcript_is_signed_again_and_verifies_at_its_new_positions`, `history.rs`
  `the_first_changed_message_is_reported`, `signing.rs` `unsigning_removes_adams_member_from_a_position_on`.
* **Failure.** `SigningError` (`Keys(TurnKeyError)`, `Row`, `Position`) is `Classify`: the class is the keys' own
  (`Refused` is `Unauthenticated`, `Unavailable` is `Transient`), a row that cannot be written or an index past `u32::MAX`
  is `Internal`; it never signs `null` and never clamps. Tests: `signing.rs`
  `a_failing_key_source_is_a_classified_error_and_signs_nothing`; `tests/turn_signatures.rs`
  `failing_keys_fail_the_run_and_commit_no_unsigned_turn` (the stored transcript is the user row alone).
* **Vector.** `crates/adam-llm-agent/tests/vectors/turn_chain_vector.py` regenerates the golden constants of `signing.rs`
  (checked identical 2026-11-26).

