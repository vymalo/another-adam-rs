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

## The model's window (ADR 0088, slice W1)

Built 2026-11-27. The invariant: **one rule answers a model alias's window**, and every reader asks the client.

* The rule: `adam_model::resolve_context_window` (`crates/adam-model/src/profiles.rs:221`): the profile's
  `contextWindow`, else the window configured for that alias outside the profiles (`DEFAULT_MODEL_CONTEXT_WINDOW`), else
  `None`; a hand-built `0` falls through. The loader refuses `0` and anything above `MAX_CONTEXT_WINDOW` (2^53 − 1,
  `profiles.rs:211`) in `window` (`profiles.rs:624`), like a cap of `0`. A preset has no window: it is a fact of the
  model, and a preset carries no limit of the model either.
* The clients answer through it: the trait default (`crates/adam-model/src/client.rs:44`, the profile only),
  `MockModel` (`crates/adam-model/src/mock.rs:203`) and `OpenAiCompatible` (`crates/adam-model-openai/src/lib.rs:985`);
  `GateModel` and `Arc<T>` forward. `ModelConfig::client` gives the client both `DEFAULT_MODEL_CONTEXT_WINDOW` for `MODEL`
  and the profiles (`crates/adam-service/src/config.rs:1347`, `:1358`).
* The readers ask the client per alias: the compaction budget of the live alias
  (`crates/adam-llm-agent/src/agent.rs:3741`, narrowed by a session's `context_window` in
  `compaction::effective_window`), each `usage/v1` report (`agent.rs:4224`), and the output cap's clamp
  (`crates/adam-assembly/src/assembly.rs:758`). A subagent is built on the same client with its own alias, so its
  window is its own profile's.
* The engine (amended 2026-11-28): `EngineBuilder::start` wraps the host's client in `Profiled`
  (`crates/adam-engine/src/profiled.rs`) when `EngineBuilder::models` gave profiles, before the assembly is built, so the
  three readers above see the profiles' windows without the host giving its client the same profiles. For an alias the
  client has a profile of, the client answers alone (its profile wins whole, not merged: its requests are sent by it);
  otherwise `resolve_context_window(engine's profile, client.context_window(alias))`, the client's answer without a
  profile being its configured window. `profile()` answers likewise; everything else forwards. Check: no second rule,
  and the wrap stays before `bound.model(..)`. The reasoning wire and extra body are still the client's own profiles.

Tests: `profiles.rs` `a_context_window_is_one_to_two_to_the_53_minus_one_tokens`,
`the_profiles_window_wins_over_the_configured_one`; `client.rs` `the_default_window_is_the_profiles`; `mock.rs`
`a_profiles_window_wins_over_the_told_one`; `adam-model-openai` `a_profiles_window_wins_over_the_configured_one_alias_by_alias`;
`adam-service` `a_profiles_context_window_wins_over_default_model_context_window`; `adam-llm-agent` `tests/usage.rs`
`each_report_carries_the_window_its_aliases_profile_names` and `tests/compaction.rs`
`the_budget_is_taken_from_the_window_of_the_models_profile`; `adam-assembly` `tests/cap.rs`
`a_subagent_on_another_alias_has_its_own_profiles_window`; `adam-engine` `tests/engine.rs`
`the_profiles_window_reaches_a_client_without_profiles` and the unit tests of `src/profiled.rs`.

## The history budget's default (ADR 0088, slice W3)

Built 2026-11-28. The invariants: **one constant is the default**, and **a history the window cannot hold never
squeezes the output cap to nothing**.

* The default: `adam_llm_agent::DEFAULT_MAX_HISTORY_TOKENS` = 200,000 (`crates/adam-llm-agent/src/agent.rs:918`), the
  value of `Limits::default()` (`agent.rs:955`); a folder that omits `limits.max_history_tokens` takes it through
  `def::limits` (`crates/adam-assembly/src/def.rs:1539`). No other crate writes the number: grep for `200_000` beside
  `max_history_tokens` before adding one.
* The compaction budget is unchanged: the smaller of the window's budget and the limit, `0` the window's alone
  (`crates/adam-llm-agent/src/compaction.rs:241`).
* The output cap's clamp (`crates/adam-assembly/src/assembly.rs:761`) reserves `min(max_history_tokens, window / 2)`,
  and `window / 2` for `0`, for every alias. Before W3 only a model the agent does not run on itself had the half
  bound; with the 200,000 default the agent's own model on a 131,072-token window (the dev stack) would have asked for
  one output token. A folder's explicit history over half the window now gives way too, which amends ADR 0064
  decision 4 (dated note there).
* The coder's folder states the value (`bin/adam-coder/agent/instructions.md:11`) rather than leaving it to the
  default, so its budget is read where its other limits are and a change of the library's default does not move it.

Tests: `compaction.rs` `the_default_history_budget_is_the_budget_when_no_window_is_known`,
`a_known_smaller_window_wins_over_the_default_history_budget`; `def.rs` `an_omitted_history_budget_is_the_loops_default`;
`adam-assembly` `tests/cap.rs` `a_history_the_window_cannot_hold_leaves_the_output_half_the_window` and the cases
before it; `adam-engine` `the_deployments_profiles_reach_an_engines_agent` (80,000 on a 200,000 window).

## The default window's variable (ADR 0088, slice W2)

Built 2026-11-28. The invariant: **one variable answers the window outside the profiles, and two answers fail closed.**

* The reader: `parse_default_context_window` in `crates/adam-service/src/config.rs`, called once by
  `ModelConfig::parse` (so once at startup per binary): `DEFAULT_MODEL_CONTEXT_WINDOW`, else the deprecated
  `MODEL_CONTEXT_WINDOW`. A set alias logs one `WARN` naming the new variable, whether or not the new one is set.
  Both set: equal (as numbers, so `" 131072 "` equals `131072`) is the value; different is one problem naming both
  variables and neither value, which the binaries turn into a `ConfigError` (exit 78). A value one of them refuses is
  that variable's problem alone, not a conflict. Blank is unset (`not_blank`).
* Both are held to `MAX_CONTEXT_WINDOW` by `parse_context_window(name, ..)`, whose problem starts with the name.
* The result is still `ModelConfig::context_window`, the `configured` argument of `resolve_context_window`: W1's
  rule is unchanged, the profile's `contextWindow` wins.
* The chart: `coder.modelContextWindow` (`deploy/coder/templates/_helpers.tpl`) renders `DEFAULT_MODEL_CONTEXT_WINDOW`
  in `templates/statefulset.yaml`; `templates/_validate.tpl` refuses the value beside either name in
  `config.extraEnv`. No golden sets the value, so no golden changed; `tests/render-check.sh` holds the name.
* Every in-repository writer moved: `compose.yaml`, `dev/compose.real-model.yaml`. `bench/` did not: it starts the
  coder image pinned in `bench/pins.json` (`sha-9a9e6c5`, 2026-11-21), which reads only the old name; it moves with the pin.
* The alias is removed in a later minor release, with a dated *Amended* note on ADR 0088.

Tests: `adam-service` `model_context_window_is_a_deprecated_alias_of_default_model_context_window` (new alone, old
alone with one warning, equal, different, one invalid, blank alias; the `WARN` lines captured with a
`tracing_subscriber` writer), `the_context_window_is_a_positive_count_for_the_models_alias` (both names' bounds);
`adam-agent` `src/config.rs` `the_context_window_is_read_by_the_workers_and_a_bad_one_stops_the_start` (exit 78 for
a conflict). For people: [Environment](../../docs/reference/environment.md), the chart's
[README](../../deploy/coder/README.md#the-models-context-window).

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
  SHA-256 pass the head already needed: O(rows), and **one** `key_set().await` per decode (`check_read` reads it once and
  hands it to `verify_head_with` and `verify_turns_with`; `seal::turn_tests::a_decode_reads_the_key_set_once`). The
  "small against a model call" argument holds for a claim only. `Runtime::state` (the engine's reopen) and `prior_state`
  have no model call behind them. A2A `GetTask`/`ListTasks` use `Runtime::view`, which decodes the head record only and
  verifies nothing, so polling pays none of this (`runtime.rs` `view`). No cache is built; a follow-up if a long run read by
  `state` in a loop shows (a verified-prefix cache keyed by run, version and key-set generation). Decision 4 said one check
  for the newest turn would do; this checks all of them (a rotation could revoke a key that signed an early turn).
* **Revocation.** Revoking a key makes every past run it signed permanently `Corrupt` on `state`, claim and continuation.
  Rotate (keep the old key in the set, unrevoked) to retire it without losing history.
* **Commit guard.** `Sealer::require_signed_turns` (presence of a turn signature on every assistant/tool row) runs in
  `worker.rs` `plan` (a step whose rows hold an unsigned turn is turned into a `Failed` transition with the state untouched,
  error `RuntimeError::UnsignedTurns`, class `Invalid`) and again in `seal_update` before any head is signed (a commit error
  of class `Invalid`; an adopted unsigned v4 run that ends is written unsigned). Tests:
  `tests/head_signature.rs` `a_keyed_runtime_with_an_agent_that_does_not_sign_fails_the_run_and_stores_no_signed_head`,
  `seal.rs` `the_commit_guard_names_the_first_unsigned_turn`.
* **Key outage at a continuation.** `TurnKeys` at `prior_state` is `Transient`, makes no run, and A2A maps it to
  `Unavailable` (`backend.rs` `map_err` by class); test `turn_reentry.rs`
  `a_key_outage_at_the_prior_state_makes_no_run_and_the_request_succeeds_later`.
* **Adoption plus carry (release N).** A store attacker who strips the head signature, `seal` and every turn signature gets
  the history adopted; a carry then signs it as a validly signed new run. Closed by T5 (no adoption).
* **Tests.** `crates/adam-runtime/src/seal.rs` `turn_tests` (the head verifies, the turn does not: forged assistant, forged tool,
  changed user, spliced, unsigned, reordered, revoked key, a non-turn row left alone, key set down); `crates/adam-llm-agent/tests/turn_reentry.rs`
  (resume, new task, session round, each with four forgeries and no model call; a carry signed again under the new run; adoption once;
  revoked key; an appended user row and a changed head field at a resume; a key outage at `prior_state`).

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

## Child runs: every child is a session, started with subagent_start (ADR 0089, slices C1 and C4)

For people: [Child runs](../../docs/architecture.md#child-runs). *Verified 2026-11-28* by reading the code and running the
tests named here.

* **No tool per subagent (C4).** `session_roster` (`crates/adam-assembly/src/def.rs`, the walk's `Walk::agent`) checks the
  names (`SubagentToolClash`, now also against `subagent_*` for a subagent so named), binds each remote's
  `RemoteEndpoint` and its `RemoteRoundAgent`, and builds the roster (`session_targets`) for **every** agent with
  subagents; `BoundDef::model` (`crates/adam-assembly/src/assembly.rs`) makes `SessionTools` per node (`session_tools`,
  whose bounds read the node's own manifest through `manifest_of`). `SubagentTool` (`subagent.rs`) and
  `RemoteSubagentTool` are deleted; `DEFAULT_SUBAGENT_MINUTES` moved to `def.rs`.
* **The one-shot call** is `subagent_start` with `mode: one_shot`, `wait: true`: `Start::call`
  (`crates/adam-llm-agent/src/sessions.rs`) starts round 1 under `ToolCtx::child_run_id()` (the id the named tool used, so
  a replay finds the same child) and returns `ToolError::AwaitSession`; the loop's `PendingWait::Session` settles with
  `poll_report`, the session's page. Its model park is carried up by `LlmAgent::carry_round_park`
  (`crates/adam-llm-agent/src/agent.rs`, only when the waiting tool is `subagent_start`, whose round 1 is the run's own
  child) into `PendingSession::model_wait`, as `PendingWait::Run` keeps a child's, and the coder's `commands_end_with`
  (`bin/adam-coder/src/agent.rs`, `child_model_wait`) stops its commands for either.
* **Modes.** `resolve_modes` (`def.rs`) counts the five as the agent's tools when it has subagents: they are made at
  `model`, after the modes resolve, and before C4 a mode that listed them dropped them with a warning.
* **The loader.** `names_a_subagent` (`crates/adam-agent-fs/src/source.rs`) refuses an agent's or a mode's `tools:` that
  names one of the agent's subagents, at the `tools` line, naming `subagent_start` (decision 6).
* **C1, kept for hosts.** `ToolCtx::start_one_shot_session` is still `sessions::start_one_shot`: `first_header` with
  `closed: true`, `ChildStarter::start_round`, and the `InvalidSession` fallback that finds a plain child of before C1 or
  a conversation id past `MAX_SESSION_CONVERSATION_BYTES`. No tool of the assembly calls it now.
* **Upgrade.** A run parked on `PendingWait::Run` from a named tool settles from the store without the tool
  (`answer_run`); a `PendingWait::Remote` from a remote's named tool reads "the tool `<name>` that started this task is
  gone" (`LlmAgent::poll_remote`).
* **Tests.** `crates/adam-assembly/tests/subagents.rs`: `a_parent_calls_its_subagent_which_calls_its_own_and_the_parent_goes_on`
  (sessions at depth two, the conversation ids), `a_one_shot_start_that_waits_is_a_session_closed_after_its_round`,
  `two_calls_of_one_turn_are_two_sessions_with_their_own_conversations`,
  `every_agent_with_subagents_gets_the_session_tools_and_no_other`, `a_mode_that_names_the_session_tools_keeps_them`,
  `a_hosts_one_shot_tool_finds_a_child_started_before_it_was_a_session`,
  `a_parent_parked_on_a_subagents_own_tool_of_before_is_answered_after_the_upgrade`; `tests/remote.rs` (the remote one-shot,
  `the_remote_subagent_of_a_subagent_has_a_session_and_a_round_agent`); `crates/adam-agent-fs/tests/modes.rs`
  `a_tools_list_that_names_a_subagent_is_refused_naming_subagent_start`; `crates/adam-llm-agent/tests/model_park.rs`
  `a_start_that_waits_carries_its_rounds_park_and_a_message_wakes_the_round_through_it`;
  `crates/adam-a2a-runtime/tests/child_sessions.rs` `a_one_shot_session_is_listed_and_takes_no_message_from_the_person`;
  `crates/adam-export/tests/export.rs` `a_subagents_run_is_a_record_of_its_own_linked_to_its_parent`.

## L2 memory beside the run store

* **The table.** `<prefix>memory_lesson` is created by every store migration (schema version 10) and, alone, by
  `PgMemory::migrate`; the DDL is `memory_lesson_ddl` in `crates/adam-store-postgres/src/lib.rs`, whatever the `memory`
  feature says, so a binary without the feature that stamps version 10 cannot leave a feature-enabled one on a schema
  with no table. `root_cause` is `COLLATE "C"`, so the recall's tie-break is byte order as
  `sort_newest_first` (`crates/adam-memory/src/lesson.rs`) is. Fixture:
  `crates/adam-store-postgres/tests/fixtures/schema-v10.sql`.
* **The adapter.** `crates/adam-store-postgres/src/memory.rs`: `record` is one transaction that takes
  `pg_advisory_xact_lock(hashtextextended(scope || chr(31) || audience, 0))`, counts served lessons and upserts
  keeping `created`; `retire` takes the same lock in a CTE; the synchronous `Memory` port over the async pool is bridged
  by `PgMemory::block_on` (outside a runtime, `block_in_place` in a multi-thread one, a scoped thread in a
  current-thread one). The constructors and `migrate` answer `adam_core::StoreError` with the store's classes.
* **The suite.** `crates/adam-store-postgres/tests/memory.rs` runs `memory_conformance!` against a real server; under
  `ADAM_TEST_REQUIRE_DB=1` a missing URL fails. The scope-blind mutation that makes its two-scope case fail is recorded
  in ADR 0081's agent page.
