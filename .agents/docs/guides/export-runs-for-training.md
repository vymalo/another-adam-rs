# Export runs for training: invariants, gates and evidence

For people: [Export runs for training](../../../docs/guides/export-runs-for-training.md). That page decides; this one gives
the invariants to check on a change to the exporter, the commands to run and where each claim stands in the code. The
decision is [ADR 0085](../../../docs/decisions/0085-runs-are-exported-as-training-records.md); the crate is
[`adam-export`](../../../crates/adam-export/README.md). Written on 2026-11-27 against the working tree on `f69e6ea` plus
the slice E1 and E2 change, and updated the same day for slice E5 (per-turn samples) on `3bc7997` and for the ports
and adapters on `6a859c0`; line numbers move, so the function names are the anchors.

## Invariants to check on a change

1. **No secret and no person's data beyond the messages.** `record_with` (`crates/adam-export/src/convert.rs:174`,
   through `head_of` at `:228`) reads only these fields of a `Conversation`: `messages` (`:182-192`, and `facets_of` at
   `:255`), `turns`, `tool_calls`, `usage` (`:250-252`), `artifacts` by `name` and `mime_type` (`:259-266`), and
   through `is_compacted` (`:277`) `compactions`, `shortenings`, `omitted_turns`, `summary`, and through `models_of`
   (`:285`) each assistant message's `provider.model` and `model_switch` (`:297`). `samples_of`
   (`crates/adam-export/src/sample.rs:204`) also reads `compactions`, `continued_from`, `omitted_turns`, `request`, `limit` and
   `shortenings` to place and mark the samples, and `context` **only** through `interface_language` (`:228`), to mark a
   sample not exact: no value of `context` is written. It never reads `context` otherwise
   (`crates/adam-llm-agent/src/conversation.rs:821`; the caller is the `vymalo.caller` key in it, `CALLER_CONTEXT_KEY`
   at `:167`), `prompt_statement` (`:851`) or `session` (`:867`). `RuntimeSource` reads `messages` only to skip an empty
   state (`crates/adam-export/src/runtime_source.rs:146`), and `request` and `requests` to load their blobs
   (`recorded_requests`, `runtime_source.rs:165`). A change that reads another field must say why in the pull request
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
4. **Images stay out.** An image part becomes `IMAGE_PLACEHOLDER` (`convert.rs:20`, `:344`, `:360`); no image bytes or
   image references are written.
5. **Reasoning only when asked.** `reasoning_content` is set only under `include_reasoning` (`convert.rs:332`).
6. **Facets never read what a person wrote**: `facets_of` skips `Message::User`
   (`crates/adam-export/src/facets.rs:147`), and a facet is a name, never a value from the text.
7. **An export is a command.** An `Exporter` is run (directly, or through `export_runs`) only by the `export`
   subcommand of `adam-agent` and `adam-coder` (`crates/adam-service/src/export.rs:419-424`); no worker component,
   route or schedule runs one. Check with `grep -rn "Exporter::new\|export_runs" crates bin --include=*.rs`.
8. **Which runs are reached.** `RuntimeSource::page` lists with `ConversationScope::Prefix(String::new())`
   (`crates/adam-export/src/runtime_source.rs:85`), and `ConversationScope::contains` is `false` for a run with no conversation id
   (`crates/adam-core/src/store/query.rs:23-29`). Since ADR 0087 slice C1 a subagent's own tool starts its child as a
   one-shot session (`SubagentTool::call` → `ToolCtx::start_one_shot_session`), so every subagent run has a session's
   conversation id and is exported, linked by `parent`
   (`crates/adam-export/tests/export.rs` `a_subagents_run_is_a_record_of_its_own_linked_to_its_parent`). Still not
   reached: a child a host starts with `Runtime::start_child` (no conversation) and a subagent run from before C1;
   listing those needs a query by parent, which `RunQuery` does not have: a `Store` change (Rule 2), not an exporter
   change. A source lists per agent name (`RunSource::agents`), and the `export` subcommand defaults to every registered agent
   (`agents_to_export`, `crates/adam-service/src/export.rs`), the subagents included.
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
    none when that call recorded none (`export.rs:189`).
13. **The budget never cuts.** `max_tokens` drops a whole record whose `estimated_tokens` (`sample.rs:72`, characters
    of messages and tools / 4, rounded up, the rule of `adam-llm-agent`'s `compaction::estimate`) is over it, and
    counts it (`over_budget`, `export.rs:209`, `:223`). In turn mode the filter keeps compacted runs (`record_filter`,
    `export.rs:98-104`).
14. **Ports and adapters** (ADR 0085, *Amended* 2026-11-27). The pipeline (`Exporter`, `crates/adam-export/src/export.rs:88`)
    names no store and no file: it reads `&dyn RunSource` (`crates/adam-export/src/source.rs:34`) and writes
    `&mut dyn RecordSink` (`crates/adam-export/src/sink.rs:27`), both `#[async_trait]` like `Store`. Neither error names a
    backend type (Rule 2): `SourceError::Backend` and `SinkError::Backend` box it with a class; a runtime's error keeps its
    class (`source.rs:245`), a write is `Transient` and a put after `finish` `Internal` (`sink.rs:89`), a foreign cursor
    `Invalid` (`source.rs:251`). A source counts what it lists and cannot yield in `Skipped` (`source.rs:195`; outside the
    query, not a conversation, unreadable), and the pipeline adds them to the `Summary` (`count_listed`, `export.rs:45`),
    so the counts are those of the driver before the ports. `RuntimeSource::page` (`runtime_source.rs:78`) is the old
    listing, unchanged: one `list_runs` of `SourceQuery::limit` (100, `export.rs:17`), statuses and `since` pushed, `until`
    checked (`runtime_source.rs:105`), the cursor the last run's `(updated_at, id)` (`:122`). A new port method breaks
    every adapter: say so in the pull request and in both READMEs.
15. **One unnamed source writes the lines of before.** `Record::source` (`crates/adam-export/src/record.rs:30`) is
    skipped when `None`; `head_of` leaves it `None` (`convert.rs:245`) and the pipeline sets it from the source's label
    only (`export.rs:215`). The `export` subcommand runs one unnamed `RuntimeSource` (`crates/adam-service/src/export.rs:419`),
    so its output is byte-identical (`convert.rs` `a_record_without_a_source_is_the_line_it_was`, a line captured before
    the change; `tests/export.rs` `export_runs_is_the_exporter_over_one_unnamed_runtime_source`). Several sources must
    each have a label of their own, or nothing is read (`check_labels`, `export.rs:231`). The pseudonym key is the
    export's, so ids link across sources. A label is the operator's word and is not scrubbed: never put a host or a URL
    in one.

## The gates

```sh
cargo fmt --all --check
cargo clippy -p adam-export --all-targets --locked -- -D warnings
cargo test -p adam-export --locked
cargo test -p adam-export-testkit --locked                           # the port suites against RuntimeSource and JsonLines
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
| several stores into one sink: labels in `source`, the summary of both, the same id alone and together, an unnamed source among several refused | `tests/export.rs` `two_stores_export_into_one_sink_with_their_labels_and_linked_ids` |
| `export_runs` is the `Exporter` over one unnamed `RuntimeSource` into `JsonLines`, byte for byte | `tests/export.rs` `export_runs_is_the_exporter_over_one_unnamed_runtime_source` |
| a record without a source is the line the exporter wrote before the ports | `convert.rs` `a_record_without_a_source_is_the_line_it_was` |
| several sources need labels of their own, not blank | `export.rs` `several_sources_need_labels_of_their_own` |
| `JsonLines`: one line per record, in order; `finish` twice is fine; a put after it is `Internal`; a refused write `Transient` | `sink.rs` `each_record_is_one_line_of_json_in_order`, `a_refused_write_is_transient` |
| `RuntimeSource` passes the source suite (disjoint pages, the cursor, skips counted, the query, the bookkeeping) | `adam-export-testkit` `tests/runtime_source.rs` (`source_conformance!`), and `a_cursor_it_did_not_make_is_invalid` |
| `JsonLines` over a `BufWriter` passes the sink suite (order, `finish` flushes and is idempotent, a put after it refused, a whole record) | `adam-export-testkit` `tests/json_lines.rs` (`sink_conformance!`) |
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
`a_stack_filter_keeps_the_run_that_read_the_manifest`) drive `export_runs` and the `Exporter` over a runtime end to end; the binaries'
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
