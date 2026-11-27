# Export runs for training: invariants, gates and evidence

For people: [Export runs for training](../../../docs/guides/export-runs-for-training.md). That page decides; this one gives
the invariants to check on a change to the exporter, the commands to run and where each claim stands in the code. The
decision is [ADR 0085](../../../docs/decisions/0085-runs-are-exported-as-training-records.md); the crate is
[`adam-export`](../../../crates/adam-export/README.md). Written on 2026-11-27 against the working tree on `f69e6ea` plus
the slice E1 and E2 change; line numbers move, so the function names are the anchors.

## Invariants to check on a change

1. **No secret and no person's data beyond the messages.** `record_of` (`crates/adam-export/src/convert.rs:141`) reads
   only these fields of a `Conversation`: `messages` (`:147`, `:153-158`, and `facets_of` at `:187`), `turns`,
   `tool_calls`, `usage` (`:182-184`), `artifacts` by `name` and `mime_type` (`:191-198`), and through `is_compacted`
   (`:207-212`) `compactions`, `shortenings`, `omitted_turns`, `summary`, and through `models_of` (`:215-232`) each
   assistant message's `provider.model` and `model_switch` (`:227`). It never reads `context`
   (`crates/adam-llm-agent/src/conversation.rs:821`; the caller is the `vymalo.caller` key in it, `CALLER_CONTEXT_KEY`
   at `:167`), `prompt_statement` (`:851`) or `session` (`:867`). The driver reads `messages` only to skip an empty
   state (`crates/adam-export/src/export.rs:168`). A change that reads another field must say why in the pull request
   and update both pages.
2. **Every string is cleaned.** Each `String` a record carries from the run passes `ExportOptions::clean`
   (`convert.rs:80-86`): the deployment's `Scrub`, then `scrub_known_shapes` (`crates/adam-export/src/scrub.rs:62`;
   the shapes at `:41`, the URL password at `:57`). Today that is the system prompt, every message's text, a tool
   call's `arguments`, the reasoning, a tool result, a tool's `description`, the error and an artifact's name. Not
   cleaned: a tool's `parameters` schema (copied from the agent's `ToolSpec`), a tool call's id and name, the agent's
   name, `mime_type`. A new string field goes through `clean`.
3. **Ids are pseudonyms.** The run, its parent and its conversation go through `Pseudonyms::of`
   (`convert.rs:170-177`; `crates/adam-export/src/pseudonym.rs:50`), HMAC-SHA256 checked against RFC 4231. No
   `RunId` or conversation id is written anywhere else in a record. `Pseudonyms`' `Debug` hides the key
   (`pseudonym.rs:73`), and so does `ExportOptions`' (`convert.rs:89-97`).
4. **Images stay out.** An image part becomes `IMAGE_PLACEHOLDER` (`convert.rs:20`, `:274`, `:290`); no image bytes or
   image references are written.
5. **Reasoning only when asked.** `reasoning_content` is set only under `include_reasoning` (`convert.rs:262`).
6. **Facets never read what a person wrote**: `facets_of` skips `Message::User`
   (`crates/adam-export/src/facets.rs:147`), and a facet is a name, never a value from the text.
7. **An export is a command.** `export_runs` is called only by the `export` subcommand of `adam-agent` and `adam-coder`;
   no worker component, route or schedule calls it. Check with
   `grep -rn "export_runs" crates bin --include=*.rs`.
8. **Which runs are reached.** `export_agent` lists with `ConversationScope::Prefix(String::new())`
   (`export.rs:110`), and `ConversationScope::contains` is `false` for a run with no conversation id
   (`crates/adam-core/src/store/query.rs:23-29`). A child from `Runtime::start_child` has none
   (`crates/adam-runtime/src/runtime.rs:1536`, "it has no conversation"), so plain subagent runs are not exported;
   both pages say so. Listing them needs a query by parent, which `RunQuery` does not have: a `Store` change
   (Rule 2), not an exporter change.
9. **The schema is stable.** `SCHEMA` is `adam.export/v1` (`crates/adam-export/src/record.rs:11`). A new optional field
   is fine; a changed meaning is a new schema and an *Amended* note on ADR 0085.

## The gates

```sh
cargo fmt --all --check
cargo clippy -p adam-export --all-targets --locked -- -D warnings
cargo test -p adam-export --locked
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
| `--current-tools` adds a system message and tools, `tools_source: current` | `convert.rs` `the_current_shape_adds_a_system_message_and_tools` |
| a failed run's error is scrubbed | `convert.rs` `a_failed_run_keeps_its_error_scrubbed` |
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

## Unverified

* That the OpenAI chat shape of a record is what the chat templates of Qwen and the common SFT tools accept: from
  memory (ADR 0085 decision 2), not checked against each trainer's current documentation.
* The benchmark harness drops and recreates its slot's database before each run (`bench/postgres-init.sql`'s comment,
  `resetDatabase` in `bench/lib/stack.mjs`), so its runs cannot be exported after the fact: *verified* by reading
  them on 2026-11-27, not by running the harness.
