# Extensions: the evidence

For people: [Extensions](../../../docs/reference/extensions.md), which says what each card lists, what is served and
where the contract is. This page holds, for the same seventeen rows, the code that lists the URI, the test that proves
it, and the check that keeps the table whole. Checked 2026-11-26 against `main` at `1f8f1a6d`; the tests were found by
reading them, not by running them (no build ran for this page).

## The rows

The URIs are the constants of `crates/adam-a2a/src/extensions.rs`. **Listed by** is the code that puts the entry on the
card. Where two binaries list a URI the same way, one function is named; `adam-coder` and `adam-agent` each have their own
`with_extensions` over `ExtensionConfig`, and both run `adam_service::serve`, which adds the conditional entries.

| URI | Listed by | Proven by | Served by |
|---|---|---|---|
| `https://a2ui.org/a2a-extension/a2ui/v1.0` | `adam_ui::card_extensions` (`crates/adam-ui/src/lib.rs`), through `with_extensions` in `bin/adam-coder/src/app.rs` and `bin/adam-agent/src/agent.rs` | `bin/adam-agent/tests/agent.rs` `the_card_is_the_one_the_folder_declares` (the folder agent's whole list, in order); `bin/adam-coder/src/app.rs` `the_card_from_the_agent_file_equals_the_golden` (`bin/adam-coder/tests/fixtures/agent/card.json`); `crates/adam-ui/tests/a2ui_schemas.rs` `the_cards_a2ui_params_validate_against_agent_capabilities` | `crates/adam-ui` |
| `https://agents.vymalo.com/a2a/extensions/ui-catalog/v2` | `adam_ui::card_extensions` | the two card tests above; `crates/adam-ui/tests/agent.rs` `a_stale_digest_is_read_again_over_the_thread_tools_and_the_form_is_drawn`; `crates/adam-ui/tests/offer.rs` | `crates/adam-ui` |
| `https://agents.vymalo.com/a2a/extensions/thread-tools/v1` | `adam_ui::card_extensions` | the two card tests above; `crates/adam-ui/tests/agent.rs` `a_tool_the_endpoint_lists_is_offered_under_its_name_and_called_through_the_endpoint`; `crates/adam-ui/tests/turn_output.rs`; `bin/adam-agent/tests/agent.rs` `the_card_that_lists_uploads_lists_thread_tools_beside_it` | `crates/adam-ui/src/thread_tools.rs`; no call to the contract's `request_upload` |
| `https://agents.vymalo.com/a2a/extensions/mentions/v1` | `adam_ui::card_extensions` | the two card tests above; `crates/adam-a2a-runtime/tests/vymalo.rs` `the_mentions_of_a_message_are_the_runs_context_and_the_next_task_does_not_inherit_them` | `crates/adam-a2a-runtime/src/vymalo.rs` (the mentions into the run's context); `ask_agent` in `crates/adam-ui/src/thread_tools.rs` |
| `https://agents.vymalo.com/a2a/extensions/steer/v1` | `adam_ui::card_extensions` | the two card tests above; `crates/adam-a2a-runtime/tests/steer.rs` `a_steered_message_is_delivered_to_the_working_task_and_the_model_reads_it` and `steering_over_http_with_the_official_client` | `crates/adam-a2a-runtime` |
| `https://agents.vymalo.com/a2a/extensions/steps/v1` | `with_extensions` (`ExtensionConfig::steps`) in both binaries | the card tests above; `crates/adam-a2a-runtime/tests/steps.rs` `a_client_that_activated_steps_reads_each_step_as_a_report_beside_its_line`; `bin/adam-agent/tests/agent.rs` `an_mcp_tools_step_carries_its_title_input_and_output_without_the_processs_secrets` | `crates/adam-a2a-runtime/src/steps.rs` |
| `https://agents.vymalo.com/a2a/extensions/text-stream/v1` | `with_extensions` (`ExtensionConfig::text_stream`) in both binaries | the card tests above; `crates/adam-a2a-runtime/tests/text_stream.rs` `an_activated_client_reads_chunks_while_the_model_writes_then_the_whole_text_under_the_streams_id`, `over_http_the_header_activates_the_stream_and_the_chunks_carry_their_offsets`, `an_activated_client_reads_the_reasoning_as_chunks_of_a_stream_of_its_own_before_the_reply` | `crates/adam-a2a-runtime` |
| `https://agents.vymalo.com/a2a/extensions/usage/v1` | `with_extensions` (`ExtensionConfig::usage`) in both binaries | the card tests above; `crates/adam-a2a-runtime/tests/usage.rs` `an_activated_client_reads_a_report_per_model_call_on_a_status_with_no_message`, `over_http_both_bindings_send_the_reports_to_an_activated_client_and_say_the_totals`; `bin/adam-coder/tests/e2e.rs` `the_tokens_of_the_coder_and_its_explorer_reach_an_activated_client` | `crates/adam-a2a-runtime` |
| `https://agents.vymalo.com/a2a/extensions/build/v1` | `card_for_build` in `bin/adam-coder/src/app.rs` and `bin/adam-agent/src/agent.rs` (`ExtensionConfig::build`) | `bin/adam-coder/src/app.rs` `the_card_says_which_build_and_which_files_answer`; `bin/adam-agent/tests/agent.rs` `the_card_of_a_folder_says_its_build_and_its_digest`; `bin/adam-agent/tests/binary.rs` `serves_the_card_of_its_folder_and_stops_cleanly_on_sigterm` | nothing to serve: information only |
| `https://agents.vymalo.com/a2a/extensions/access-tier/v1` | the coder's `with_extensions` (`ExtensionConfig::access_tier`, tiers `crate::tier::honoured_up_to(max)`); `adam-agent` never lists it | `bin/adam-coder/src/app.rs` `the_card_from_the_agent_file_equals_the_golden`; `bin/adam-coder/tests/tiers.rs` `the_coders_backend_refuses_a_bad_tier_and_says_the_tier_a_task_works_under`, `by_default_bypass_is_the_deployments_opt_in_and_not_a_tier_a_message_may_name`; `crates/adam-a2a-runtime/tests/access_tier.rs` `a_tier_the_message_names_reaches_the_run_and_the_task_says_it` | `crates/adam-a2a-runtime`, `bin/adam-coder/src/tier.rs` |
| `https://agents.vymalo.com/a2a/extensions/commands/v1` | `crates/adam-assembly/src/card.rs` (the folder's `commands/`) and `crates/adam-service/src/prompt_commands.rs` (the prompts of MCP servers); `ExtensionConfig::commands` returns `None` for none | `bin/adam-agent/tests/agent.rs` `a_folder_serves_its_commands`, `the_prompts_of_an_mcp_server_are_commands_of_the_agent`; `bin/adam-coder/tests/binary.rs` `the_prompts_of_the_extra_mcp_servers_are_commands_of_the_coder`; `crates/adam-service/tests/serve.rs` `serve_expands_the_commands_it_is_given_and_the_card_lists_them`; `crates/adam-a2a-runtime/tests/commands_flow.rs` | `crates/adam-a2a-runtime` (`Commands`) |
| `https://agents.vymalo.com/a2a/extensions/child-sessions/v1` | `adam_service::serve` (`crates/adam-service/src/serve.rs`), from `ChildSessions::card_extension` in `crates/adam-a2a-runtime/src/child_sessions.rs`, only when the role runs the control plane and the backend accepted the helpers | `bin/adam-coder/tests/binary.rs` `a_persons_explorer_round_started_on_a_control_plane_completes_on_the_worker` (and the tests that call `child_sessions_entry`); `bin/adam-agent/tests/binary.rs` (its `child_sessions_entry` and `person_session`); `crates/adam-assembly/tests/child_sessions.rs`; `crates/adam-assembly/tests/remote_sessions.rs` | `crates/adam-a2a-runtime/src/child_sessions.rs` |
| `https://agents.vymalo.com/a2a/extensions/locale/v1` | `adam_service::serve`, when `config.default_locale` is set (`DEFAULT_LOCALE`) | `crates/adam-service/tests/serve.rs` `the_card_lists_locale_exactly_when_the_deployment_names_a_default_language`; `crates/adam-a2a-runtime/tests/vymalo.rs` `the_interface_language_of_an_activated_message_is_the_runs_context_and_none_clears_it` | `crates/adam-a2a/src/locale.rs`, `crates/adam-a2a-runtime/src/vymalo.rs` |
| `https://agents.vymalo.com/a2a/extensions/reasoning/v1` | `adam_service::card_with_reasoning` (`crates/adam-service/src/serve.rs`), and the coder's `router_with_card` through the backend's `reasoning_efforts()`; none for a model that takes no level | `bin/adam-agent/tests/agent.rs` `a_served_agent_lists_its_models_reasoning_levels_and_takes_one`; `crates/adam-a2a-runtime/tests/reasoning.rs` `a_level_the_agent_takes_reaches_the_run_and_its_model_request`, `a_level_the_agent_does_not_take_is_refused_and_writes_nothing`; `bin/adam-coder/tests/agent_files.rs` `the_coder_takes_the_reasoning_levels_of_its_models_profile` | `crates/adam-a2a-runtime/src/backend.rs` |
| `https://agents.vymalo.com/a2a/extensions/model-pick/v1` | `adam_service::serve` and the coder's `router_with_card` (`bin/adam-coder/src/app.rs`), when `service.model_choices()` is non-empty | `crates/adam-service/tests/serve.rs` `the_card_lists_model_pick_exactly_when_the_backend_serves_it`; `bin/adam-agent/tests/agent.rs` `a_served_agent_offers_the_deployments_other_models_and_takes_a_pick`; `crates/adam-a2a-runtime/tests/model_pick.rs` `a_listed_pick_reaches_the_model_request`, `an_unlisted_pick_is_refused_and_writes_nothing` | `crates/adam-a2a-runtime/src/backend.rs` |
| `https://agents.vymalo.com/a2a/extensions/prompt-statement/v1` | `adam_service::serve`, when `config.prompt_statement` (`MODEL_AUTH=injector` or a turn signing key; `crates/adam-service/src/config.rs`) | `crates/adam-service/tests/serve.rs` `the_card_lists_prompt_statement_exactly_behind_the_injector`; `crates/adam-a2a-runtime/tests/prompt_statement.rs` `every_model_call_of_the_run_carries_the_values_and_a_steer_replaces_them`, `without_the_activation_no_call_carries_either_header` | `crates/adam-a2a/src/prompt_statement.rs`, `crates/adam-a2a-runtime/src/prompt_statement.rs` |
| `https://agents.vymalo.com/a2a/extensions/uploads/v1` | `adam_service::serve`, whenever a card exists and none lists it yet (`ExtensionConfig::uploads`, no `params`) | `crates/adam-service/tests/serve.rs` `the_card_lists_uploads_with_no_params`; **nothing proves a service**, because none exists | **partly** (ANR-1015, part 2): the coder's `share_file` (`bin/adam-coder/src/tools/share.rs`) sends a file over `ToolEnv::inline_upload_threshold` (`adam_service::inline_upload_threshold(folder, env)`, wired in `bin/adam-coder/src/serve.rs`) with `adam_ui::Ui::upload` over `adam_mcp::Uploader` (`crates/adam-mcp/src/upload.rs`, ANR-1014), and returns `Artifact::upload_ref`, served as a `data` part by `crates/adam-a2a-runtime/src/convert.rs`; the activation rides the context key `vymalo.uploads` (`crates/adam-a2a-runtime/src/uploads.rs`). Proven by `bin/adam-coder/tests/share_upload.rs`, `crates/adam-ui/tests/uploads.rs`, `crates/adam-a2a-runtime/tests/vymalo.rs` `an_activated_uploads_message_is_the_runs_context_and_a_later_plain_one_clears_it`. **Not served**: `adam-agent` (no tool), a `files: true` MCP result (inline only), the end-to-end through the orchestrator and the parity cells (part 3, ANR-1016); parity gap 10, [#282](https://github.com/vymalo/another-adam-rs/issues/282) |

Rows that say "the card tests above" are listed by the same two functions, and are proven there by the two whole-card
tests: the folder agent's `the_card_is_the_one_the_folder_declares` (its eight URIs, in order, before `build/v1`) and the
coder's golden `bin/adam-coder/tests/fixtures/agent/card.json`, which holds the coder's eleven (the commands entry from its
folder, the five of the screen, `steps`, `text-stream`, `usage`, `access-tier` and `build`). The conditional URIs are in
neither list, by design: their tests are the ones named in their rows.

A claim for a row that this page does not name a test for has none: before you add "proven" to a row, find the test
and read what it asserts.

## The check

`tools/docs-check/check-docs.mjs`, section 7 (CI's `docs` job, no new dependency):

1. reads `crates/adam-a2a/src/extensions.rs` and takes every `pub const <NAME>EXTENSION…: &str = "<uri>"` (the constants
   whose name contains `EXTENSION`; a constant such as `A2UI_BASIC_CATALOG_V1_0` or `TEXT_STREAM_KIND_REASONING` is not
   one). Each of them is a URI the card builder can list;
2. reads every non-test Rust source of `crates/` and `bin/` (a `tests` directory is skipped) for a literal URI of the form
   `https://agents.vymalo.com/a2a/extensions/<name>/v<n>` or `https://a2ui.org/a2a-extension/…`, so an extension written
   as a literal outside the constants file is caught too;
3. requires each URI to appear in a **table row** (a line that starts with `|`) of `docs/reference/extensions.md`, and,
   for the constants, of this page; and fails naming the URI and the page that lacks it:
   `docs/reference/extensions.md: no row for the extension URI <uri> (crates/adam-a2a/src/extensions.rs: NAME)`.

It does not read tests or judge the "served" column: that stays a person's judgment, and the people page's "uploads" row
is the example of one that was not obvious. When the card builder starts listing a URI, add its row to both pages in the
same change, with the test that proves it.

To see it fail: delete the `uploads/v1` row from `docs/reference/extensions.md` and run
`node tools/docs-check/check-docs.mjs`.

## Procedure for a change

| You | Do |
|---|---|
| add an extension to the card | add the constant (it is then in the check), the row on both pages, the contract link or "no contract page yet", and the `adam-a2a-extensions` skill's table |
| make a listed URI served (for example `uploads/v1`) | change its row on both pages, add the test, close parity gap 10 (for `uploads/v1` it is still open for the folder agent and the end-to-end) |
| stop listing one | remove the constant and both rows in the same change |
| find that the system's site adds or lacks a contract page | change the link; the pages were checked on 2026-11-26 against the system's `main` (`docs/api/*.md`) |
