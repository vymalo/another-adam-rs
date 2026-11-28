# The A2A server: evidence to check

For people: [The A2A server](../../../docs/reference/a2a-server.md), which says what a client sees. This page holds the
evidence behind its document section and the procedure a change runs.

## The committed document

`docs/api/a2a-server.openapi.json` is what `openapi::document(&card, flags, signed)` in
`crates/adam-a2a/src/openapi.rs` builds for one fixed card — `bearer: true`, `push: false`, `extended: false`,
`signed: false` — so the file moves only when the code does, never when a deployment's card changes. It is held equal to
the code by a **unit test** in that module (`mod tests`, `the_committed_document_is_the_one_the_code_builds`), not an
integration test: `document` is `pub(crate)` and an integration test cannot call it.

Rewrite the file with the repo's golden convention (`adam-assembly`, `adam-agent-fs` and `adam-plugin` do the same):

```sh
ADAM_UPDATE_GOLDEN=1 cargo test -p adam-a2a --lib the_committed_document_is_the_one_the_code_builds
```

The drift test's failure message names that command. Keys come out sorted because `serde_json`'s `Map` is a `BTreeMap`,
so the file is byte-stable and is reviewed by its diff, not by reading it.

**Proven:** with `info.version` set to `9.9.9`, the test failed with
`docs/api/a2a-server.openapi.json no longer matches crates/adam-a2a/src/openapi.rs; regenerate it with …`; regenerated
afterwards, byte-identical to the first write (vymalo/another-adam-rs#441, squash `f9595722`).

## `start.config` of `child-sessions/v1` (ADR 0054)

For people: [the `start.config` paragraph](../../../docs/reference/a2a-server.md#child-sessions-child-sessionsv1) of the
page above. `parse_config` in `crates/adam-a2a-runtime/src/child_sessions.rs` reads the nine members (seven numbers, and
`tools` and `denyTools` as lists of non-blank strings) and calls the one `SessionBounds::admit` the model's tool calls;
`put_header` writes `vymalo.session.tools` into every round a person starts, **over** the message's own context, and the loop
reads the lists from the round's session header first (`crates/adam-llm-agent/src/session_tools.rs`).

* **What differs between roles is the tool names**, never the numbers. A control plane's bounds (`AgentDef::session_bounds`)
  carry `tools: []`, `tools_complete: false`: shape only. A worker's carry the names of the bound node and of every source
  that declares them (`ToolSource::declared`), and `tools_complete` only when every source of the node does. A declared name
  may be a **shape** (ADR 0054 amended 2026-11-28, ANR-1390): `*__*` stands for a relayed `<server>__<tool>`, and
  `SessionBounds::admit` (`crates/adam-runtime/src/session.rs`, `selects`) admits an entry that selects a concrete declared
  name with its pattern or that a declared shape matches as a string. The coder and the folder agent add the thread-tools
  source, which declares the contract's tools it knows by name and `*__*`, so **their sets are complete**: a `tools` entry
  that selects nothing is refused there, `github__*` is admitted, and a concrete `denyTools` name no tool has is still
  refused (ANR-1319, ANR-1390). A source that answers `Declared::Unknown` still leaves a set incomplete and the check
  shape-only.
* **Tests:** `child_sessions.rs` unit tests `a_start_takes_tools_and_deny_tools_as_lists_of_names`,
  `a_remote_helper_refuses_tools_and_deny_tools`, `a_round_carries_the_tools_its_header_narrows_over_what_the_message_said`;
  `adam-runtime` `session.rs` `an_incomplete_set_refuses_a_concrete_deny_entry_it_does_not_know_and_checks_no_pattern`,
  `a_complete_set_checks_patterns_and_allow_entries_too`, `a_declared_shape_admits_a_relayed_servers_tools`;
  `adam-assembly/tests/session_bounds.rs` `an_agent_with_a_source_has_an_incomplete_set_that_still_catches_a_deny_typo`,
  `a_source_that_declares_a_shape_makes_the_set_complete_and_admits_a_servers_tools`; `bin/adam-agent/tests/agent.rs`
  `a_helpers_bounds_are_complete_and_admit_a_relayed_servers_tools`; `bin/adam-coder/tests/agent_files.rs`
  `the_helpers_bounds_are_complete_and_admit_a_relayed_servers_tools`.

## A session's `skills` (ADR 0054 decision 4, slice 7, `skills` half)

For people: the same [`start.config` paragraph](../../../docs/reference/a2a-server.md#child-sessions-child-sessionsv1) — it
says `skills` is an option of the model's tools and not of `start.config` yet, as `instructions` is.

* `SessionConfig::skills` (`skills` on the wire, camelCase in the header too) is held by `SessionBounds::admit`
  (`crates/adam-runtime/src/session.rs`, `admit_skill_names`): at most `MAX_SESSION_SKILLS` (32) names of at most
  `MAX_SESSION_SKILL_CHARS` (128) characters, none blank, and each one a name of `SessionBounds::skills`. That set is
  `skills::selected_names` (`crates/adam-assembly/src/skills.rs`) from the subagent's `skills:` **alone**, so a control plane
  that binds no agent knows it as a worker does; it is complete by construction. A remote subagent refuses the option by name
  (`REMOTE_REFUSED` in `crates/adam-llm-agent/src/sessions.rs`). `clamp` keeps the selection, and it is a *next round* option
  (`SessionConfig::next_round_only`, `adopted_mid_round`).
* The prompt is composed per request: `Node::prompt` (the folder's text), `Node::catalog` (`skills::PromptCatalog`, whose
  `section(Some(names))` filters the loadable entries and **keeps every preloaded body**) and `Node::tail` (the session tools'
  guide), joined by `adam_llm_agent::system_prompt` at `LlmAgent::system_for` and at `AgentInfo::prompt`, so a root and a
  narrowed session share one renderer. `ToolCtx::skills` carries the list into `load_skill` and `read_skill_file`, which answer
  `SkillError::NotInSession` (a result with `is_error`, never `Err`).
* **Evidence:** `adam-runtime` `session.rs` `a_skill_outside_the_folders_selection_is_refused_with_the_list`,
  `a_skills_entry_is_held_to_its_shape`, `skills_count_as_set_and_survive_a_clamp_and_an_edit`;
  `adam-assembly/src/skills.rs` `a_sessions_selection_leaves_an_unlisted_skill_out_of_the_catalog`,
  `a_sessions_selection_keeps_a_preloaded_skill_in_the_prompt`, `load_skill_refuses_a_skill_the_session_does_not_list`;
  `adam-llm-agent/src/catalog.rs` `the_three_parts_are_joined_with_a_blank_line_and_an_empty_one_is_left_out`, and
  `src/agent.rs` `a_sessions_instructions_follow_the_folders_under_a_heading` and
  `a_sessions_skills_narrow_the_catalog_and_the_text_follows_it`. The refusal was seen red: with the
  `in_session` check deleted from `LoadSkill::call`, `load_skill_refuses_a_skill_the_session_does_not_list` failed on
  `assert!(refused.is_error)` (the call returned the body of `b`). The golden and ADR 0044 prefix tests are unchanged.

## `edit` of `child-sessions/v1` (ADR 0054 decision 11, slice 4)

For people: [the `edit` paragraph](../../../docs/reference/a2a-server.md#child-sessions-child-sessionsv1) of the page
above. `parse_edit` in `crates/adam-a2a-runtime/src/child_sessions.rs` reads `metadata[URI].edit {config}` — one member,
`config`, an object of the same camelCase options `start.config` takes, where `null` is a **reset** — and refuses a message
that asks two of `start`, `close` and `edit`. `asked` answers `Asked::Edit {patch}`, `submit_child` passes it to
`message_to_session`, and that function does what `subagent_edit` does, in the same order and with the same function:
`SessionBounds::clamp` the session's config to the bounds of now, `SessionPatch::apply` the patch over it,
`SessionBounds::admit` the result, and rewrite the refusal's `config.<snake>` words to `edit.config.<camel>`.

* A **working** round gets an `Inbound::new(SESSION_EDIT_KIND, SessionEditRequest {config, by: Person, members})` with the
  message id; an **idle** session gets a round of `editing_round_payload(&header)` through `next_round`. Both count toward
  `max_rounds` (`countable_rounds` counts an editing round: it is not `closing && !closed`), and neither is refused by the
  round cap — as a close is not: an editing round spends no model call, and the cap still bounds the rounds that do.
* The edit is handled **above** the spent check: raising the `tokenBudget` of a session that spent it is what the option is
  for, so a message to a spent session is `UnsupportedOperation` while an edit of it is accepted (a close is exempt there
  too). An empty `edit.config` is `InvalidParams`, as the tool refuses it, so it records no edit and spends no round.
* `SessionEditRequest` is the whole admitted config, so the round applies it and never merges; `by: Person` is what
  `SessionHeader.edits` records.
* **Evidence:** `crates/adam-a2a-runtime/tests/child_sessions.rs` `an_edit_of_an_idle_session_is_a_round_that_calls_no_model`
  (the editing round, `header.config`, `edits[0].by == Person`, the next round runs with it),
  `an_edit_of_a_working_session_is_delivered_into_its_round` (delivered, no new round, the reader applies it),
  `an_edit_past_a_bound_is_refused_and_changes_nothing` (`InvalidParams` naming `edit.config.maxTurns`),
  `an_edit_that_asks_two_things_or_names_no_option_is_refused` (two of three, and `config: {}`),
  `an_edit_raises_the_budget_of_a_session_that_spent_it`; `crates/adam-runtime/src/session.rs`
  `a_patch_sets_resets_and_leaves_the_rest`, `an_edit_of_a_running_round_adopts_next_model_call_options_only`,
  `a_header_keeps_the_last_sixteen_edits`, `an_edit_a_round_never_read_is_applied_by_its_reader`,
  `an_editing_round_says_edit_and_carries_its_header`; `crates/adam-llm-agent/tests/sessions.rs`
  `an_edit_of_a_working_round_changes_its_next_model_call`, `an_edit_of_an_idle_session_is_a_round_that_calls_no_model`,
  `an_edit_past_a_bound_is_refused_and_changes_nothing`, `a_retried_edit_applies_once`,
  `an_edit_of_instructions_applies_from_the_next_round`.
* **Not verified without a compiler:** the five front tests above were written but not run (ANR-216); the runtime and
  `adam-llm-agent` cases are the same.
