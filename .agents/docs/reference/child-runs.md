# Child runs: the tests of each interleaving

For people: [Child runs and remote tasks](../../../docs/reference/child-runs.md), which says what happens in each case
and what the design does. This page holds the evidence: how the tests make a case happen, and the
test of every row. A row's number is the number on the page for people.

**Check a change** to child runs, sessions or remote tasks against this page: a new interleaving gets a row on both
pages, with the same number, and a test that makes it happen without sleeping.

## Failure interleavings of a local child

Each one has a test that makes it happen without sleeping: steps are held at gates, the 60 s timer is
moved with a `ManualClock`, and a fault is scripted on one run of the `FaultyStore` (`fail_run`,
`fail_run_after_apply`). Names are in `crates/adam-runtime/tests/runtime.rs` (memory, PostgreSQL and
MongoDB) and `crates/adam-llm-agent/tests/child_runs.rs`. The cases: [failure interleavings of a local child](../../../docs/reference/child-runs.md#failure-interleavings).

| # | Test |
|---|---|
| 1 | `a_lost_notice_is_recovered_by_the_timer`, `a_lost_notice_is_recovered_when_the_timer_fires` |
| 2 | `a_lost_terminal_ack_sends_no_notice_and_the_timer_recovers`, `a_lost_terminal_ack_is_recovered_when_the_timer_fires` |
| 3 | `a_duplicate_notice_is_ignored`, `only_the_awaited_childs_notice_answers` |
| 4 | `cancelling_a_waiting_parent_cancels_its_child`, `cancelling_the_parent_cancels_the_child_too`, `a_running_child_and_its_child_are_told_within_one_poll`, `cancel_task_reaches_the_subagent_run` |
| 5 | `a_failed_or_cancelled_child_tells_its_parent_why`, `a_failing_child_is_an_error_result`, `a_cancelled_child_is_an_error_result`, `a_child_that_is_gone_is_an_error_result_after_a_purge`, `a_purged_child_reads_as_gone` |
| 6 | `a_parent_that_loses_its_lease_does_not_start_or_resume_twice`, `a_worker_that_loses_its_lease_while_answering_changes_nothing` |
| 7 | `a_notice_that_beats_the_park_is_not_lost`, `a_notice_that_beats_the_park_is_used`, `a_retry_finds_the_notice_before_the_wait_is_recorded` |
| 8 | `a_parent_woken_early_parks_again`, `the_parent_looks_at_the_child_when_the_timer_fires` |
| 9 | `a_message_while_waiting_queues_behind_the_result` |
| 10 | `only_the_awaited_childs_notice_answers`, `child_status_reads_only_the_callers_children` |
| 11 | `start_child_records_the_parent_and_is_idempotent` |
| 12 | the split `front` and `back` runtimes of cases 1, 2, 5 and 6 |
| 13 | `a_childs_notice_that_lands_while_a_run_finishes_does_not_reopen_it`, `a_notice_that_lands_while_the_parent_finishes_costs_no_model_turn` |
| 14 | `a_child_past_its_deadline_fails_and_wakes_at_it`, `a_child_past_its_limit_is_an_error_result_for_its_parent` |
| 15 | `a_child_created_after_the_cancel_never_steps`, `a_child_follows_a_parent_that_failed_but_not_one_that_is_done`, `a_done_parent_that_was_purged_leaves_its_open_child_running`, `a_child_parked_on_a_question_is_cancelled_when_its_parent_is`, `a_grandchild_under_a_parked_child_follows`, `a_healthy_parked_child_is_woken_without_stepping_the_agent` |
| 16 | `a_child_created_after_the_cancel_never_steps` |
| 17 | `a_claimed_run_that_is_cancelled_already_is_not_stepped` (`src/worker.rs`), `a_parent_cascade_and_a_deadline_run_the_hook` |

## A session's limits (ADR 0054, slice 1c)

For people: [a session's own limits](../../../docs/reference/child-runs.md#sessions-adr-0045-slices-1-to-4). The header's
`config` is read where each limit is enforced, never raised: `LlmAgent::limits_of` (`max_turns`, `max_tool_calls`),
`live_model` (`max_output_tokens`, `0` of the folder = the session's, never `0`), `parallel_of` (`max_parallel_tool_calls`),
`history_budget` with `compaction::effective_window` (`context_window`: `min` with the model's window, held to
`20,000 + cap + 8,000`), and `Send::call` / `round_within` in `sessions.rs` (`max_rounds`, `time_limit`, read from the header
clamped by `SessionBounds::clamp` **before** the round-limit check).

| Limit | Test |
|---|---|
| `max_output_tokens` | `session_limits.rs`: `max_output_tokens_of_the_header_is_the_cap_of_the_request`, `..._never_raises_the_agents_and_is_never_zero` |
| `max_turns` | `session_limits.rs`: `max_turns_of_the_header_ends_the_round_at_its_closing_turn`, `max_turns_of_the_header_never_raises_the_agents` |
| `max_tool_calls` | `session_limits.rs`: `max_tool_calls_of_the_header_refuses_a_turn_that_would_go_over_it` |
| `max_parallel_tool_calls` | `session_limits.rs`: `max_parallel_tool_calls_of_the_header_narrows_how_many_run_at_once`, `..._never_raises_the_agents` |
| `context_window` | `compaction.rs`: `a_sessions_context_window_starts_compaction_sooner`, `..._is_the_minimum_with_the_models_never_instead_of_it`, `a_sessions_window_that_leaves_no_budget_...`, `a_sessions_floor_follows_its_narrower_output_cap`; unit tests in `compaction.rs` |
| `time_limit` | `sessions.rs`: `a_sessions_time_limit_is_the_deadline_of_its_round` |
| `max_rounds` | `sessions.rs`: `a_sessions_own_max_rounds_is_enforced_by_a_send` |
| a config beyond the bounds is refused; one within is recorded | `sessions.rs`: `a_start_config_beyond_the_bounds_is_refused_and_one_within_is_recorded` |
| `tools`, `deny_tools` (slice 3): `SessionBounds::admit` (`admit_tool_entries`: shape always; against `SessionBounds::tools` by the rule of `tools_complete`) -> the session header's `config` (the truth) and `vymalo.session.tools` in each round's context (`round_message`, `put_header`, from `SessionConfig::tools_context`) -> `SessionNarrowing` (`crates/adam-llm-agent/src/session_tools.rs`), applied **by the loop**, first, at both points: `apply_sources`/`offered_by` (the request) and `step_work` (the call), with or without sources; `read_output` stays | `sessions.rs`: `a_hidden_tool_is_not_offered_and_a_call_to_it_is_refused_unrun`, `a_sources_tool_is_narrowed_like_the_agents_own`, `deny_tools_wins_over_tools_and_a_pattern_selects`, `round_two_carries_the_tools_its_config_narrows`, `a_session_without_tools_carries_an_empty_value`, `a_bad_tools_list_is_refused_and_a_remote_subagent_takes_none`, `list_and_read_show_the_tools_a_session_narrows`; `session_tools.rs` unit tests (`an_oversized_context_that_drops_the_key_leaves_the_header_narrowing_in_force`, `a_value_of_the_wrong_shape_fails_closed`); `adam-runtime` `session.rs` (`a_tools_pattern_that_matches_no_bound_tool_is_refused_with_the_list`, `an_incomplete_set_refuses_a_concrete_deny_entry_it_does_not_know_and_checks_no_pattern`, `a_complete_set_checks_patterns_and_allow_entries_too`, `a_server_pattern_selects_the_tools_of_an_mcp_server`, `without_known_tools_an_entry_is_admitted_by_its_shape_alone`); `adam-a2a-runtime` `child_sessions.rs` unit tests; `adam-assembly/tests/session_bounds.rs`: `a_worker_bounds_hold_the_names_of_the_subagents_tools_and_a_definitions_do_not`, `an_agent_with_a_source_has_an_incomplete_set_that_still_catches_a_deny_typo`, `a_source_that_declares_its_tools_makes_the_set_complete_and_catches_an_allow_typo` (ANR-1319), `a_source_that_declares_a_shape_makes_the_set_complete_and_admits_a_servers_tools` (ANR-1390); `adam-runtime` `a_declared_shape_admits_a_relayed_servers_tools`; `bin/adam-agent/tests/agent.rs` `a_helpers_bounds_are_complete_and_admit_a_relayed_servers_tools`; `bin/adam-coder/tests/agent_files.rs` `the_helpers_bounds_are_complete_and_admit_a_relayed_servers_tools`. **Both shipped agents have `tools_complete: true`** (`BoundDef::bound_tool_names`): the thread-tools source declares the contract's tools it knows by name and the shape `*__*` of the `<server>__<tool>` tools it relays (`crates/adam-ui/src/thread_tools.rs`, `declared` -> `Declared::All`, test `the_thread_tools_declare_their_contract_tools_and_the_shape_of_a_relayed_one`); `admit`'s `selects` admits an entry a concrete declared name matches or a declared shape matches as a string, so `github__*` is admitted and a typo is refused (ANR-1390) |
| a group's loader goes with the tools of its server (ANR-1304): `kept_loaders` in `crates/adam-llm-agent/src/session_tools.rs` keeps `load__group__<server>` when **every** allow-list names the loader or a tool of that server (`<server>__<tool>`), among the agent's own tools (`LlmAgent::own_names` in `agent.rs`, `self.specs`: the members and the loaders are own tools of the agent, `crates/adam-assembly/src/def.rs` `tools.extend(groups.loader_tools())`); `SessionNarrowing::holds_back` checks `deny_tools` first, so a denied loader stays withheld. Read from the header's config, the context's copy of the same lists and the agent's fixed tool set — all fixed for the round — so the array does not move between turns (ADR 0044); exact for a `when` MCP server's group, and a group whose members do not share its name prefix is not covered (`Group::new`, tests only) | `session_tools.rs`: `a_loader_is_kept_with_the_tools_of_its_server_and_a_deny_list_still_wins`; `adam-assembly/tests/groups.rs`: `a_session_narrowed_to_a_servers_tools_keeps_its_group_loader` (red before the fix: `load__group__docs` was withheld and the load refused), `a_session_that_selects_nothing_of_a_server_keeps_no_loader`; `crates/adam-llm-agent/tests/prefix.rs`: `a_narrowed_sessions_prefix_holds_and_its_group_loader_stays` |
| `report_schema` (ADR 0054 slice 9): admitted by `report_schema_admissible` in `crates/adam-runtime/src/session.rs`; checked at the final answer by `LlmAgent::report_gate` with `check_report` / `report_correction` in `sessions.rs` (two corrections, `Conversation::report_corrections`; called at the turn's words, an announced answer, the `final_answer` branch of `step_unsigned`'s `Flow::Next` arm, and the closing turn; a correction clears `announced`) | `session_limits.rs`: `a_matching_answer_is_the_report_and_costs_no_extra_turn`, `a_mismatch_gets_two_corrective_turns_and_then_fails_the_round_with_the_reason`, `a_corrected_answer_ends_the_round_as_the_report`, `the_problems_a_correction_tells_are_the_first_three`, `a_correction_is_a_turn_of_max_turns_and_an_answer_at_the_closing_turn_must_still_match`, `a_round_without_a_report_schema_ends_with_whatever_it_answers`, `a_tools_final_answer_is_held_to_the_report_schema_and_never_ends_the_round_unchecked`, `a_corrected_answer_replaces_the_announced_one_that_did_not_match`; `sessions.rs`: `a_report_schema_is_refused_at_start_when_it_cannot_be_enforced_and_recorded_when_it_can`, `a_remote_subagent_refuses_a_report_schema`; unit tests in `session.rs` (`a_report_schema_...`) |
| round 2 carries the config, clamped to the bounds of now | `sessions.rs`: `round_two_carries_the_config_clamped_to_the_bounds_of_now`; `SessionBounds::clamp` in `crates/adam-runtime/src/session.rs` |
| an edit (ADR 0054 decision 11, slice 4): `SessionPatch` (`crates/adam-runtime/src/session.rs`, a member `null` is a reset — `double_option`, since serde's `Option` reads `null` as absent) merged by `SessionPatch::apply` over the header's config **clamped to the bounds of now**, admitted by `SessionBounds::admit`; a working round gets `SESSION_EDIT_KIND` and `LlmAgent::apply_edit` writes `Conversation::round_config` (`SessionConfig::adopted_mid_round`, *next model call* only) beside the header's pending config; an idle session gets `editing_round_payload` and `header.editing`, ended at once by `step_unsigned`; `read_latest` applies an edit a round never read; `SessionHeader::record_edit` keeps the last `MAX_SESSION_EDITS` (16) | `adam-runtime` `session.rs`: `a_patch_sets_resets_and_leaves_the_rest`, `an_edit_of_a_running_round_adopts_next_model_call_options_only`, `a_header_keeps_the_last_sixteen_edits`, `an_edit_a_round_never_read_is_applied_by_its_reader`, `an_editing_round_says_edit_and_carries_its_header`; `sessions.rs`: `an_edit_of_a_working_round_changes_its_next_model_call`, `an_edit_of_an_idle_session_is_a_round_that_calls_no_model`, `an_edit_past_a_bound_is_refused_and_changes_nothing`, `a_retried_edit_applies_once`; `crates/adam-llm-agent/src/sessions.rs` unit test `a_config_member_set_to_null_is_named_and_returns_to_the_folders` |
| the control plane's bounds are a worker's **but for the tool names** (ANR-1093; `without_tools(..)` in the test strips `tools` and `tools_complete` before comparing): one function, `crates/adam-assembly/src/bounds.rs` `bounds_of`, called by `Assembly` and `AgentDef::session_bounds` | `adam-assembly/tests/session_bounds.rs`: `a_definition_computes_the_bounds_a_worker_binds_without_binding`, `the_roster_of_the_bounds_is_the_workers`, `the_definitions_parallel_limit_is_the_workers_unless_it_says_another`; `adam-agent/tests/agent.rs`: `a_control_plane_knows_the_bounds_a_worker_binds`, `a_start_config_on_a_control_plane_is_admitted_by_the_workers_bounds`; `adam-service` `every_role_reads_max_parallel_for_the_session_bounds` |

## Session interleavings

Each has a test in `crates/adam-llm-agent/tests/sessions.rs` (memory, PostgreSQL and MongoDB, with no fixed sleeps; the report
is read from the store, the notice is only a wake-up). The cases: [session interleavings](../../../docs/reference/child-runs.md#session-interleavings).

| # | Test |
|---|---|
| S1 | `a_retried_try_starts_nothing_twice` |
| S2 | `every_step_replays_from_the_journal_to_the_same_history` |
| S3 | `a_lost_notice_is_recovered_by_the_timer` |
| S4 | `a_duplicate_notice_is_ignored` |
| S5 | `a_read_with_wait_parks_and_resumes_once_with_the_report`, `polling_is_refused_and_waiting_is_not` |
| S6 | `a_start_returns_at_once_and_the_turn_end_waits_for_the_report` |
| S7 | `cancelling_the_parent_leaves_a_kept_open_session_idle` |
| S8 | `a_message_to_a_working_round_is_steered`, `closing_a_working_session_lets_the_round_finish`, `an_edit_of_a_working_round_changes_its_next_model_call` |
| S9 | `a_session_that_is_not_ours_is_refused_in_words` |
| S10 | `a_remote_subagent_starts_a_round_of_its_registered_agent` |
| S11 | `a_report_that_ends_mid_turn_reaches_the_next_model_call`, `reports_during_a_turn_keep_the_prefix`, `a_closing_round_does_not_hide_the_report_of_the_round_before` |
| S12 | `a_report_during_the_final_model_call_reopens_the_turn` |
| S13 | `a_report_is_found_in_the_store_when_its_notice_is_lost`, `a_lost_notice_while_the_parent_is_parked_costs_one_turn`, `a_duplicate_report_notice_matches_nothing` |
| S14 | `a_persons_round_is_reported_as_the_persons`, `a_persons_round_is_reported_in_a_later_task`, `a_fresh_task_tells_only_the_persons_rounds_that_ended_after_it_began` |
| S15 | `a_report_read_by_the_model_is_not_appended_again`, `a_run_without_sessions_lists_only_at_its_first_model_call` |
| S16 | `a_journal_from_before_reports_replays_to_the_same_history`, `a_replay_appends_the_same_reports`, `a_replay_of_a_stale_answer_counts_each_round_once` |
| S17 | `each_limit_refuses_in_words`, `an_idle_expired_session_refuses_a_send_as_closed`, `an_expired_sessions_report_is_still_told_once`, `an_edit_past_a_bound_is_refused_and_changes_nothing`, `an_edit_of_an_idle_session_is_a_round_that_calls_no_model`, `a_retried_edit_applies_once` |
| S18 | `a_persons_round_that_asks_the_person_is_blocked_and_the_answer_resumes_it`, `a_child_run_that_is_no_shown_session_cannot_ask`, `a_shown_round_asks_the_person_who_answers_it_in_the_child_context`; a hook's `Ask` in a child: `a_child_reads_ask_as_deny` (`crates/adam-llm-agent/tests/approval.rs`, a run whose start names a `root_run`; the predicate is `Conversation::may_ask_person` and `is_unattended`, read by `approvable` in `crates/adam-llm-agent/src/agent.rs`) |
| S19 | `crates/adam-assembly/tests/subagents.rs`: `a_one_shot_start_that_waits_is_a_session_closed_after_its_round`, `two_calls_of_one_turn_are_two_sessions_with_their_own_conversations`, `a_parent_parked_on_a_subagents_own_tool_of_before_is_answered_after_the_upgrade`, `a_hosts_one_shot_tool_finds_a_child_started_before_it_was_a_session`, `a_parent_restarted_mid_wait_still_gets_the_childs_answer`; `crates/adam-llm-agent/tests/model_park.rs`: `a_start_that_waits_carries_its_rounds_park_and_a_message_wakes_the_round_through_it`; `crates/adam-a2a-runtime/tests/child_sessions.rs`: `a_one_shot_session_is_listed_and_takes_no_message_from_the_person`; `crates/adam-export/tests/export.rs`: `a_subagents_run_is_a_record_of_its_own_linked_to_its_parent` |
| S20 | `each_mode_is_kept_in_the_header_and_decides_what_follows_the_round`, `keep_open_maps_onto_mode_and_a_disagreement_is_refused`, `a_model_started_session_asks_only_when_the_client_shows_child_sessions` (a sealed round cannot ask); `crates/adam-a2a-runtime/tests/child_sessions.rs`: `a_sealed_or_one_shot_session_of_the_agent_refuses_the_persons_message_and_steer`; the header's decoding: `a_header_carries_its_mode_and_one_of_before_reads_as_the_mode_keep_open_meant`, `a_person_writes_to_an_open_session_or_their_own` (`crates/adam-runtime/src/session.rs`) |
| S21 | `a_start_that_waits_answers_its_first_report_once` |
| S22 | `a_filed_mode_is_the_default_and_the_ceiling_of_a_start`, `a_filed_wait_is_a_default_a_call_may_turn_off`, `a_filed_session_changes_only_its_roster_line`; the unit `a_start_may_narrow_the_filed_mode_and_never_widen_it` (`crates/adam-llm-agent/src/sessions.rs`); `crates/adam-assembly/tests/subagents.rs`: `the_definition_carries_each_subagents_session_block`, `a_start_gets_the_filed_mode_says_it_in_the_roster_and_may_not_widen_it`; the loader: `crates/adam-agent-fs/tests/rules.rs` (`session_on_a_subagent_is_read_local_and_remote_and_absent_by_default`, `a_bad_session_block_stops_the_subagent_with_its_line`, four rule cases) |
| S23 | `an_edit_changes_no_mode_and_names_the_ceiling` (`crates/adam-llm-agent/tests/sessions.rs`; `Edit::mode_refusal` and `edit_mode_arg` in `crates/adam-llm-agent/src/sessions.rs`); `crates/adam-a2a-runtime/tests/child_sessions.rs`: `a_sealed_or_one_shot_session_of_the_agent_refuses_the_persons_message_and_steer` (sends an edit too; `refused_to_the_person` runs before the edit in `message_to_session`) |
| S24 | `deleting_a_working_session_hides_it_at_the_moment_it_stops` (a `CancelWatch` event sink lists the conversation on the status event of the cancel, so the race window is observed without a sleep), `deleting_a_closed_session_writes_the_tombstone` (`crates/adam-llm-agent/tests/sessions.rs`); `deleting_a_round_that_already_ended_writes_the_tombstone` (`crates/adam-runtime/tests/runtime.rs`: the round ends before the commit, so it is tombstoned without a cancel); the listing filter: `a_deleted_session_is_not_listed`, `a_deleted_session_is_not_a_session` (`crates/adam-runtime/src/session.rs`) |

## Remote session interleavings

Each has a test in `crates/adam-assembly/tests/remote_sessions.rs` (memory, and PostgreSQL when set). The cases: [remote session interleavings](../../../docs/reference/child-runs.md#remote-session-interleavings).

| # | Test |
|---|---|
| R1 | `a_remote_session_keeps_its_context_across_rounds` |
| R2 | `a_message_to_a_working_remote_session_is_steered_when_the_card_lists_steer` |
| R3 | `a_message_is_queued_and_sent_as_the_next_task_when_the_card_does_not` |
| R4 | `a_restart_mid_round_resumes_polling_without_sending_again` |
| R5 | `a_lost_send_response_is_retried_under_the_same_message_id` |
| R6 | `a_remote_round_that_fails_reads_failed_and_the_session_stays_usable` |
| R7 | `a_question_is_reported_and_the_next_round_answers_by_task_id` |
| R8 | `closing_a_remote_session_is_local` |
| R9 | `cancelling_a_remote_session_sends_cancel_task`, `cancelling_the_parent_sends_cancel_task`, `a_round_past_its_limit_cancels_the_remote_task` |
| R10 | `a_lost_cancel_is_sent_by_the_next_round` |
| R11 | `a_remote_session_takes_mode_and_wait_as_a_local_one_does`; the one-shot use (`one_shot`, `wait: true`) end to end in `crates/adam-assembly/tests/remote.rs` (ADR 0089 slice C4: every case of the remote's own tool, converted), and a remote under a local subagent in `a_local_subagent_calls_a_remote_subagent_declared_in_its_own_directory` |

## Remote tasks

The failure interleavings of a task a tool waits on (`ToolError::AwaitRemote`). Since ADR 0089 slice C4 no tool of the
assembly does (a remote subagent is a remote session, above), so the cases are proved on a scripted tool in
`crates/adam-llm-agent/tests/remote_tasks.rs`, and for a remote subagent by the same cases as a round in
`crates/adam-assembly/tests/remote.rs` (named after each row). The cases: [remote tasks](../../../docs/reference/child-runs.md#remote-tasks-the-same-wait-without-a-message).

| # | Test |
|---|---|
| 1 | `a_new_process_keeps_polling_without_starting_the_task_again` (`adam-llm-agent`); as a round, `a_restart_mid_wait_resumes_polling_without_sending_again` (`adam-assembly`) |
| 2 | as a round, `a_send_whose_response_is_lost_is_retried_under_the_same_message_id` (`adam-assembly`) |
| 3 | the restart case above |
| 4 | `a_task_that_outlives_its_timeout_is_an_error_result_without_another_look` (`adam-llm-agent`); as a round, `a_task_that_never_ends_is_given_up_on_after_the_limit` (a failed report, and the remote task is cancelled) |
| 5 | `a_task_that_failed_is_an_error_result_and_the_run_goes_on` (`adam-llm-agent`); as a round, `a_remote_task_that_fails_is_a_failed_report_and_the_parent_goes_on`, `a_remote_task_that_is_canceled_is_a_failed_report`, `a_remote_that_needs_input_reports_its_question` |
| 6 | as a round, `a_wrong_token_is_a_failed_report_not_a_failed_run_and_the_token_stays_out_of_it`, `a_card_that_points_the_token_at_another_origin_is_refused_before_anything_is_sent` |
