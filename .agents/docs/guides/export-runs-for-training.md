# Export runs for training: invariants, gates and evidence

For people: [Export runs for training](../../../docs/guides/export-runs-for-training.md). That page decides; this one gives
the invariants to check on a change to the exporter, the commands to run and where each claim stands in the code. The
decision is [ADR 0085](../../../docs/decisions/0085-runs-are-exported-as-training-records.md); the crate is
[`adam-export`](../../../crates/adam-export/README.md). Written on 2026-11-27 against the working tree on `f69e6ea` plus
the slice E1 and E2 change, and updated the same day for slice E5 (per-turn samples) on `3bc7997`; line numbers move, so
the function names are the anchors.

## Invariants to check on a change

1. **No secret and no person's data beyond the messages.** `record_with` (`crates/adam-export/src/convert.rs:174`,
   through `head_of` at `:228`) reads only these fields of a `Conversation`: `messages` (`:182-192`, and `facets_of` at
   `:253`), `turns`, `tool_calls`, `usage` (`:248-250`), `artifacts` by `name` and `mime_type` (`:257-264`), and
   through `is_compacted` (`:275`) `compactions`, `shortenings`, `omitted_turns`, `summary`, and through `models_of`
   (`:283`) each assistant message's `provider.model` and `model_switch` (`:295`). `samples_of`
   (`crates/adam-export/src/sample.rs:204`) also reads `compactions`, `continued_from`, `omitted_turns`, `request`, `limit` and
   `shortenings` to place and mark the samples, and `context` **only** through `interface_language` (`:228`), to mark a
   sample not exact: no value of `context` is written. It never reads `context` otherwise
   (`crates/adam-llm-agent/src/conversation.rs:821`; the caller is the `vymalo.caller` key in it, `CALLER_CONTEXT_KEY`
   at `:167`), `prompt_statement` (`:851`) or `session` (`:867`). The driver reads `messages` only to skip an empty
   state (`crates/adam-export/src/export.rs:232`), and `request` and `requests` to load their blobs
   (`recorded_requests`, `export.rs:247`). A change that reads another field must say why in the pull request
   and update both pages.
2. **Every string is cleaned.** Each `String` a record carries from the run passes `ExportOptions::clean`
   (`convert.rs:104`): the deployment's `Scrub`, then `scrub_known_shapes` (`crates/adam-export/src/scrub.rs:62`;
   the shapes at `:41`, the URL password at `:57`). Today that is the system prompt (a per-turn sample's too), every message's text, a tool
   call's `arguments`, the reasoning, a tool result, a tool's `description`, the error and an artifact's name. Not
   cleaned: a tool's `parameters` schema (copied from the agent's `ToolSpec`), a tool call's id and name, the agent's
   name, `mime_type`. A new string field goes through `clean`.
3. **Ids are pseudonyms.** The run, its parent and its conversation go through `Pseudonyms::of`
   (`convert.rs:236-243`; `crates/adam-export/src/pseudonym.rs:50`), HMAC-SHA256 checked against RFC 4231. No
   `RunId` or conversation id is written anywhere else in a record. `Pseudonyms`' `Debug` hides the key
   (`pseudonym.rs:73`), and so does `ExportOptions`' (`convert.rs:113`).
4. **Images stay out.** An image part becomes `IMAGE_PLACEHOLDER` (`convert.rs:20`, `:342`, `:358`); no image bytes or
   image references are written.
5. **Reasoning only when asked.** `reasoning_content` is set only under `include_reasoning` (`convert.rs:330`).
6. **Facets never read what a person wrote**: `facets_of` skips `Message::User`
   (`crates/adam-export/src/facets.rs:147`), and a facet is a name, never a value from the text.
7. **An export is a command.** `export_runs` is called only by the `export` subcommand of `adam-agent` and `adam-coder`;
   no worker component, route or schedule calls it. Check with
   `grep -rn "export_runs" crates bin --include=*.rs`.
8. **Which runs are reached.** `export_agent` lists with `ConversationScope::Prefix(String::new())`
   (`export.rs:138`), and `ConversationScope::contains` is `false` for a run with no conversation id
   (`crates/adam-core/src/store/query.rs:23-29`). A child from `Runtime::start_child` has none
   (`crates/adam-runtime/src/runtime.rs:1536`, "it has no conversation"), so plain subagent runs are not exported;
   both pages say so. Listing them needs a query by parent, which `RunQuery` does not have: a `Store` change
   (Rule 2), not an exporter change.
9. **The schema is stable.** `SCHEMA` is `adam.export/v1` (`crates/adam-export/src/record.rs:11`). A new optional field
   is fine; a changed meaning is a new schema and an *Amended* note on ADR 0085. Slice E5 added `train_on` (default
   `assistant`, so an older record reads the same) and `sample` (absent from a run record).
10. **A per-turn sample is never invented** (slice E5). `samples_of` (`sample.rs:204`) makes samples only for the
    answers of the calls since the run's own last compaction (`last_compaction`, `:59`, skips a continuation's
    `carried` record); the turns before are counted (`turns_lost_to_compaction`, `:54`), never rebuilt. Answers are
    placed from the newest back (`targets`, `:153`): one per call, the loop's reports message is not an answer
    (`is_answer`, `:108`), a thinking-cut turn whose message was popped counts by its instruction (`is_thinking_cut`,
    `:129`), and a run from nothing must have no answer left over (`:177-180`). The prefix is `history[..at]` and the
    target the answer, with `CLOSING_TURN` put back before the closing answer only (`closing_answers`, `:294`; the
    loop sends it and does not keep it, `crates/adam-llm-agent/src/agent.rs:3056`). Nothing else is added to or taken
    from a prefix.
11. **`exact` is conservative** (`sample.rs:251`): it is `false` unless the placement adds up (`:216`), the tools are
    the recorded entry at or before the turn (`request_for`, `:189`), no shortening is at a later turn, the run's
    context names no interface language (the request had a turn for it, `agent.rs:3043`), the loop added no read
    (`added_wait`, `:136`), no answer is a possible closing one, and the last call recorded a request or the sample is
    not after the last entry. A new case where the history differs from the request must clear `exact`, never guess.
12. **The recorded list is derived, bounded and replayed.** `Conversation::note_request`
    (`crates/adam-llm-agent/src/conversation.rs:1031`) sets `request` and appends to `requests` (`:993`) when the
    `(system, tools)` ids differ from the last entry's, keeping the newest `MAX_RECORDED_REQUESTS` (64, `:998`); the
    turn calls it with the journaled answer's request (`agent.rs:3431`), so a replay writes the same list, and the
    blobs were referenced by the commits that added them. A run-mode record keeps E3's rule: the last call's request,
    none when that call recorded none (`export.rs:168`).
13. **The budget never cuts.** `max_tokens` drops a whole record whose `estimated_tokens` (`sample.rs:72`, characters
    of messages and tools / 4, rounded up, the rule of `adam-llm-agent`'s `compaction::estimate`) is over it, and
    counts it (`export.rs:186`). In turn mode the filter keeps compacted runs (`export.rs:130-137`).

## The gates

```sh
cargo fmt --all --check
cargo clippy -p adam-export --all-targets --locked -- -D warnings
cargo test -p adam-export --locked
cargo test -p adam-llm-agent --test recorded_request --locked        # the per-change request list (slice E5)
cargo test -p adam-service --features export --lib export --locked   # --samples, --max-tokens
cargo clippy --workspace --all-targets --locked -- -D warnings      # the binaries' subcommand
cargo nextest run -p adam-agent -p adam-coder --locked              # the subcommand's tests
sh tools/release/check-versions.sh                                  # a new path dependency with a version is marked
node tools/docs-check/check-docs.mjs
```

No test of the crate needs a database. The crate README is not its rustdoc (`lib.rs` has its own `//!` text), so its
blocks are not doc-tested.

## Evidence

| Claim | Test |
|---|---|
| a conversation becomes chat messages: pseudonymous ids, tool calls with JSON-text arguments, images as `[image]`, artifacts | `convert.rs` `a_conversation_becomes_chat_messages` |
| reasoning only when asked | `convert.rs` `reasoning_is_kept_only_when_asked` |
| `models` are the turns' aliases and both sides of the fallback | `convert.rs` `models_are_the_turns_and_the_fallback` |
| a summary marks the record `compacted` | `convert.rs` `a_summary_marks_the_record_compacted` |
| `ExportOptions::current` adds a system message and tools, `tools_source: current` (the CLI has no flag for it) | `convert.rs` `the_current_shape_adds_a_system_message_and_tools` |
| a run's recorded request becomes the system message and `tools`, `tools_source: recorded` | `tests/export.rs` `a_stack_filter_keeps_the_run_that_read_the_manifest`; the recording itself: `adam-llm-agent` `tests/recorded_request.rs` |
| a failed run's error is scrubbed | `convert.rs` `a_failed_run_keeps_its_error_scrubbed` |
| N answers are N samples, each prefix the history before its answer, pairs intact, tools per turn from `requests` | `sample.rs` `each_answer_is_a_sample_with_its_prefix_and_its_turns_tools` |
| without a recorded request the samples fall back to current or none and are not exact | `sample.rs` `without_a_recorded_request_no_sample_is_exact` |
| a compacted run's samples start at the marker, the kept answer is context, lost turns counted | `sample.rs` `a_compacted_run_starts_at_the_marker_and_counts_what_it_lost` |
| a later shortening clears `exact` on the earlier samples | `sample.rs` `a_later_shortening_makes_the_earlier_samples_inexact` |
| the reports message is not a target; a popped thinking-cut turn still counts; too many answers clear `exact` | `sample.rs` `the_reports_message_is_context_and_a_cut_turn_still_counts` |
| the closing answer is asked with `CLOSING_TURN` | `sample.rs` `the_closing_answer_is_asked_with_the_closing_instruction` |
| a `wait_<turn>` read clears `exact`; one that names another turn clears it for the run | `sample.rs` `a_read_the_loop_added_marks_its_answer_or_the_run_inexact` |
| the estimate is characters / 4 | `sample.rs` `the_estimate_is_a_quarter_of_the_characters` |
| turn mode end to end: two calls are two samples, recorded tools, `train_on: "last"`; run mode says `assistant`; a budget keeps the first | `tests/export.rs` `a_turn_export_writes_one_sample_per_answer` |
| `requests` gets one entry per change, the same list after a replayed commit, one entry when nothing changes; an older state keeps its bytes | `adam-llm-agent` `tests/recorded_request.rs` `tools_that_change_mid_run_record_one_request_per_change`, `the_last_call_names_its_prompt_and_tools_stored_once`, `a_replayed_call_names_the_same_request_without_the_model`, `a_state_from_before_decodes_and_writes_the_same_bytes`; the rule and the cap: `conversation.rs` `a_request_is_listed_once_per_change_and_the_list_is_bounded` |
| `--samples run\|turn` and `--max-tokens N` parse strictly (64 otherwise) | `adam-service` `src/export.rs` `samples_and_a_token_budget_read`, `no_flag_exports_every_agent_s_finished_runs_to_standard_output` |
| `lang` from paths and commands, not from a URL | `facets.rs` `a_path_and_a_command_say_the_language` |
| `dep` from a `Cargo.toml`, with a line-number prefix, target tables and `[dependencies.<name>]` | `facets.rs` `a_cargo_manifest_names_its_dependencies` |
| `dep` from `use` and `extern crate`, `_` read as `-` | `facets.rs` `a_use_line_names_its_crate` |
| `dep` from a `package.json` | `facets.rs` `a_package_json_names_its_dependencies` |
| a person's text is not read for facets | `facets.rs` `what_a_person_wrote_is_not_read` |
| `--require` is `kind=value`, the value lower-cased | `filter.rs` `a_requirement_is_kind_equals_value` |
| the default filter keeps `done` only; `since` is inclusive | `filter.rs` `the_default_keeps_finished_runs_only` |
| the pseudonym is HMAC-SHA256 | `pseudonym.rs` `the_mac_is_hmac_sha256`, `a_long_key_is_hashed_first` (RFC 4231 cases 2 and 6) |
| a pseudonym depends on the key and the kind; `Debug` hides the key | `pseudonym.rs` `a_pseudonym_depends_on_the_key_and_the_kind` |
| the well-known token shapes and a URL's password are removed | `scrub.rs` `every_known_shape_is_removed`, `a_url_keeps_everything_but_its_password` |
| ordinary text is not copied; a closure is a `Scrub` | `scrub.rs` `ordinary_text_is_borrowed`, `a_closure_is_a_scrub` |

The crate's integration tests (`crates/adam-export/tests/export.rs`, among them
`a_stack_filter_keeps_the_run_that_read_the_manifest`) drive `export_runs` over a runtime end to end; the binaries'
tests cover the subcommand. Add each new one to this table by name.

## Known limits of per-turn samples

Not detectable from the state, so not marked: an answer the loop dropped as stale (a lost commit, then a message;
`agent.rs`, `answered_stale`) shifts the placement of the earlier answers in a continued or compacted run (in a run
from nothing the count catches it); the reports a stale answer with calls was told are not in the history; a call
whose request recording failed while the tools changed reads the entry before. Each is rare and fail-open by design
(E3).

## Unverified

* That the OpenAI chat shape of a record is what the chat templates of Qwen and the common SFT tools accept: from
  memory (ADR 0085 decision 2), not checked against each trainer's current documentation.
* The benchmark harness drops and recreates its slot's database before each run (`bench/postgres-init.sql`'s comment,
  `resetDatabase` in `bench/lib/stack.mjs`), so its runs cannot be exported after the fact: *verified* by reading
  them on 2026-11-27, not by running the harness.
