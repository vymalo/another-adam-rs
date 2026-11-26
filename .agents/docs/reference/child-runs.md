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
| S8 | `a_message_to_a_working_round_is_steered`, `closing_a_working_session_lets_the_round_finish` |
| S9 | `a_session_that_is_not_ours_is_refused_in_words` |
| S10 | `a_remote_subagent_starts_a_round_of_its_registered_agent` |
| S11 | `a_report_that_ends_mid_turn_reaches_the_next_model_call`, `reports_during_a_turn_keep_the_prefix`, `a_closing_round_does_not_hide_the_report_of_the_round_before` |
| S12 | `a_report_during_the_final_model_call_reopens_the_turn` |
| S13 | `a_report_is_found_in_the_store_when_its_notice_is_lost`, `a_lost_notice_while_the_parent_is_parked_costs_one_turn`, `a_duplicate_report_notice_matches_nothing` |
| S14 | `a_persons_round_is_reported_as_the_persons`, `a_persons_round_is_reported_in_a_later_task`, `a_fresh_task_tells_only_the_persons_rounds_that_ended_after_it_began` |
| S15 | `a_report_read_by_the_model_is_not_appended_again`, `a_run_without_sessions_lists_only_at_its_first_model_call` |
| S16 | `a_journal_from_before_reports_replays_to_the_same_history`, `a_replay_appends_the_same_reports`, `a_replay_of_a_stale_answer_counts_each_round_once` |
| S17 | `each_limit_refuses_in_words`, `an_idle_expired_session_refuses_a_send_as_closed`, `an_expired_sessions_report_is_still_told_once` |
| S18 | `a_persons_round_that_asks_the_person_is_blocked_and_the_answer_resumes_it`, `a_child_run_that_is_no_shown_session_cannot_ask`, `a_shown_round_asks_the_person_who_answers_it_in_the_child_context` |

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

## Remote tasks

The failure interleavings of a remote (`a2a:`) subagent's task. The cases: [remote tasks](../../../docs/reference/child-runs.md#remote-tasks-the-same-wait-without-a-message).

| # | Test |
|---|---|
| 1 | `a_restart_mid_wait_resumes_polling_without_sending_again` (`adam-assembly`), `a_new_process_keeps_polling_without_starting_the_task_again` (`adam-llm-agent`) |
| 2 | `a_send_whose_response_is_lost_is_retried_under_the_same_message_id` |
| 3 | the restart case above |
| 4 | `a_task_that_never_ends_is_given_up_on_after_the_limit`, `a_task_that_outlives_its_timeout_is_an_error_result_without_another_look` |
| 5 | `a_remote_task_that_fails_is_an_error_result_and_the_parent_goes_on`, `a_remote_task_that_is_canceled_is_an_error_result`, `a_remote_that_needs_input_is_an_error_result_because_nobody_can_answer` |
| 6 | `a_wrong_token_is_an_error_result_not_a_failed_run_and_the_token_stays_out_of_it`, `a_card_that_points_the_token_at_another_origin_is_refused_before_anything_is_sent` |
