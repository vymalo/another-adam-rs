# 0088: the evidence and the slice proofs

For people: [0088. A tool result is bounded when it is made, and a command that succeeds can say only that](../../../docs/decisions/0088-a-tool-result-is-bounded-when-it-is-made-and-a-command-that-succeeds-says-so.md),
which states the decision. This page holds where each slice is in the code, the invariants to check when it changes, and
the tests that prove it.

Slices T1 and T2 are built (2026-11-28, on `3d1e5b5`): [T1's proofs](#t1-built-2026-11-28), [T2's](#t2-built-2026-11-28). T4's tooling is built ([below](#t4-the-tooling-built-2026-11-28)); its measurement and T3 are not.

## T1, built 2026-11-28

`output` on `run_command`, `run` and `run_checks`; the success line; the output always kept.

| What | Where |
|---|---|
| The closed enum, its parse and its rule | `OutputMode { OnError, Full }`, `OutputMode::of`, `OutputMode::hides` (`crates/adam-code-tools/src/output.rs:59`, `:73`, `:97`); re-exported as `adam_code_tools::OutputMode` |
| The defaults, one constant beside each tool | `run_command`: `inspect::DEFAULT_OUTPUT = Full` (`crates/adam-code-tools/src/inspect.rs:29`); `run`: `make::DEFAULT_OUTPUT = OnError` (`crates/adam-code-tools/src/make.rs:36`); `run_checks`: `tools::checks::DEFAULT_OUTPUT = OnError` (`bin/adam-coder/src/tools/checks.rs:63`) |
| The output always kept | `settle_as` (`crates/adam-code-tools/src/output.rs:230`): `settle`, then a short output that will be hidden is kept through the same `CommandWorkspace::kept()` |
| The success line | `section_as` and `hidden_line` (`crates/adam-code-tools/src/output.rs:262`, `:279`) |
| Where each tool reads `output` | after `timeout_of`, before the place: `inspect.rs:62`, `make.rs:71`, `checks.rs:654` |

### Invariants to check

1. **The line is recognised as a pointer.** It starts with `[`, ends with `]` and holds `read_output {"id":"<id>"}`, so
   `adam_llm_agent::find_pointer` finds it and a shortening keeps `pointer_line(id)`. Changing its wording keeps that
   shape. Test: `a_command_that_succeeded_is_one_line_that_history_shortening_recognises` (`output.rs`).
2. **Nothing in it differs on a replay**: the size is `ShellOutcome::total` (the bytes printed), the id is
   `output_id_of(ctx)`. No duration, no time.
3. **Hiding never loses data.** An output is hidden only when it was kept (`KeptNote::refused` is `None`); a refused keep
   shows the output as `full` does. An empty output (`total == 0`) says `(no output)` and is not kept. Test:
   `a_hidden_output_is_shown_when_it_failed_could_not_be_kept_or_full_was_asked`.
4. **Only a pass is hidden.** `hides` is `ShellOutcome::passed()`: exit 0, no timeout, not stopped. A missing tool (127)
   and every refusal (`touched_git`, `unverified`, `changed_the_worktree`, a check not run again) are as before.
5. **Only the text changes.** `run_checks`' `checks` artifact, `CheckRecord::tail`, the cycles and the epoch read
   `ShellOutcome`, never the rendered text; `run`'s `after_run` and its `git status` list follow the line unchanged.
6. **No store-less case.** `CommandWorkspace::kept()` is required, so every host of these tools has a keeper. An agent
   with no `KeptStore` only meets `MAX_RUN_KEPT_BYTES` through `keep_long` (MCP results), which T1 does not touch.
7. **The prompt prefix.** The specs are pinned (`bin/adam-coder/tests/fixtures/tool-specs/{run,run_checks,run_command}.json`,
   regenerated with `ADAM_UPDATE_SNAPSHOTS=1`; restore the key order of the untouched files after), the tool table
   (`tests/fixtures/tool-tokens.txt`, `UPDATE_GOLDEN=1`), and `BASELINE_TOOLS_BYTES` (`tests/first_request.rs`)
   re-measured at 64049 B. The coder's instructions gained one sentence; the body is just under the 30,000 characters
   `the_prompt_names_no_opencode_and_stays_under_30000_chars` allows.

### Tests

| Test | Proves |
|---|---|
| `output.rs` unit tests: `the_output_mode_is_the_calls_or_the_tools_default`, `a_command_that_succeeded_is_one_line_that_history_shortening_recognises`, `a_hidden_output_is_shown_when_it_failed_could_not_be_kept_or_full_was_asked`, `a_command_that_succeeded_and_printed_nothing_says_so` | the parse, the line, its pointer, the fall-backs |
| `crates/adam-code-tools-testkit/tests/run_command.rs`: `a_look_shows_its_output_by_default_and_with_full`, `a_look_that_succeeded_with_on_error_is_one_line_and_its_output_is_kept`, `a_look_with_on_error_shows_a_failure_and_says_when_nothing_was_printed`, and the `quiet` row of `a_refusal_before_the_guard_has_no_note` | `run_command` over a scripted workspace |
| `crates/adam-code-tools-testkit/tests/run.rs`: `a_run_that_succeeded_is_one_line_by_default_and_its_output_with_full`, `a_run_that_failed_shows_its_output_by_default`, `a_run_that_succeeded_printing_nothing_says_so`, and the `loud` row of `a_refusal_before_the_guard_has_no_note_and_no_after_run` | `run` over a scripted workspace |
| `bin/adam-coder/tests/kept_output.rs`: `a_command_that_succeeds_is_one_line_with_on_error_and_read_output_reads_it`, `on_error_shows_a_failure_says_no_output_and_another_value_is_refused`, `run_checks_shows_the_same_bounded_view` | the three tools in the coder's worktrees, read back with `read_output`, each default |

Existing tests whose expected text changed (the default of `run` and `run_checks` is now `on_error`): in the testkit's
`run.rs`, the `RENDERED` constant became `rendered(ctx)` and `FULL`; in the coder's `command_results.rs`, the `run`
expectations of a success (`hidden`, `NO_OUTPUT`), with `output: "full"` where a test is about the output itself; in
`tools.rs` and `environment.rs`, `output: "full"` on the calls whose assertions read the output.

## T2, built 2026-11-28

The MCP bound follows the tool-result bound; shortenings counted in the run's usage.

| What | Where |
|---|---|
| The bound, the deployment's | `McpPolicy::result_bytes` / `result_bytes_value` (`crates/adam-mcp/src/policy.rs:350`, `:356`), default `DEFAULT_RESULT_BYTES` = 16 KiB, floor `MIN_RESULT_BYTES` = 256 (`policy.rs:37`, `:41`); `MAX_RESULT_BYTES` is a deprecated alias of the default (`src/text.rs`) |
| The coder sets it | `WorkerConfig::mcp_policy` = `mcp.policy().result_bytes(check_output_tail)` (`bin/adam-coder/src/config.rs:905`), used for the folder's servers and the thread-tools endpoint (`bin/adam-coder/src/serve.rs:229`, `:251`). `adam-agent` keeps the default |
| Where a result is bounded | `Plan::result_bytes` (`crates/adam-mcp/src/servers.rs:573`) into `McpTool`; `keep_overflow(ctx, output, bound)` (`src/tool.rs:471`): an error is cut at `bound`, a success over it kept; `map_result(.., result_bytes, ..)` (`src/tool.rs:519`) pretty-prints structured content past it; `Endpoint` takes it from its policy (`src/once.rs:230`) |
| The first page within the bound | `adam_llm_agent::keep_long_within` (`crates/adam-llm-agent/src/kept.rs:682`): the page is asked again with fewer lines (`fewer_lines`, `:768`, in proportion and strictly fewer, so it ends) until header, page and final line fit; `fitting_page` (`:750`) for the two cases with no store. `keep_long_cut` is `keep_long_within(.., usize::MAX)`: unchanged for every other caller |
| The count | `adam_runtime::ShorteningUsage { count, chars_removed }` (`crates/adam-runtime/src/usage.rs:346`); `Conversation::shortening_usage` (`crates/adam-llm-agent/src/conversation.rs:657`); `history::Shortened { first, chars_removed }` returned by `record_shortenings_from` (`src/history.rs:362`, `:438`); counted with an `info` line where the shortening is recorded (`src/agent.rs:8265`) |
| The report | `shortenings_report`, the `shortenings` member of the task's `usage/v1` object (`crates/adam-a2a-runtime/src/usage.rs:199`, `:152`) |

### Invariants to check

1. **What the model reads of an MCP result is at most the bound**, header and final line included, except that a page
   always holds one line (folded at 2,000 characters), which matters only for a bound under about 8 KiB.
2. **Paging is unchanged.** The final line of the shown page names the offset after the last line shown, so
   `read_output` reads on from there and the pages give the whole text back (the tests reassemble it).
3. **A run that never shortens writes the bytes it always did**: `shortening_usage` is
   `skip_serializing_if = "ShorteningUsage::is_empty"`, and the `usage/v1` object has no `shortenings` member then.
4. **The count is a function of the committed state**, like the shortening: a retried model call does not count again.
5. **The run's own history only.** A child's `shortening_usage` is not merged into its parent's (the child-answer code is
   ADR 0087's to change); a continuation starts with none.

### Tests

| Test | Proves |
|---|---|
| `adam-mcp` `src/tool.rs`: `a_result_over_16_kib_is_kept_and_a_first_page_within_16_kib_returned` (was `a_result_over_64_kib_is_kept_and_its_first_page_returned`), `a_configured_bound_is_the_one_a_result_is_kept_and_shown_within`, `a_result_over_the_bound_is_whole_until_it_is_kept`, `an_error_result_is_cut_as_it_always_was_and_a_short_one_is_untouched` (16 KiB and a set 4 KiB); `src/policy.rs`: `the_result_bound_is_16_kib_unless_the_deployment_sets_one_and_never_under_its_floor` | the default and a configured bound, for kept, unkept and error results |
| `adam-mcp` `tests/http.rs`: `the_result_bound_the_policy_sets_is_the_one_a_result_is_shown_within`, `big_result_without_room_to_keep_it_shows_its_first_page_and_says_so` | end to end over streamable HTTP |
| `adam-coder` `src/config.rs`: `the_mcp_result_bound_is_check_output_tail_bytes` | the coder's bound is `CHECK_OUTPUT_TAIL_BYTES` |
| `adam-llm-agent` `tests/prefix.rs`: `a_shortening_is_counted_in_the_runs_usage` (the exact characters, from the requests), `without_a_budget_nothing_is_shortened_and_the_state_has_no_member_for_it`, `a_retried_model_call_sends_the_same_request_after_a_shortening` (counted once); `src/history.rs`: `the_first_changed_message_is_reported` (and the characters removed) | the count, and nothing new without one |
| `adam-runtime` `src/usage.rs`: `shortenings_count_and_sum_and_say_nothing_until_there_is_one`; `adam-a2a-runtime` `src/usage.rs`: `shortenings_are_a_member_beside_the_totals_only_when_there_were_some`, `tokens_only_is_the_object_it_always_was` | the type and the `usage/v1` member |

## T4, the tooling, built 2026-11-28

Decision 5's comparison is one command once the owner has two ledgers of the same pinned tasks, one per default
(`bench/lib/numbers.mjs`, `bench/bin/bench.mjs`):

```sh
node bench/bin/bench.mjs numbers --ledger after.jsonl --against before.jsonl
```

Invariants to check when it changes:

* **Read-only.** No configuration, no network, no child process, no write; `--against` is refused inside a work tree like
  `--ledger`. `numbers.mjs` imports only `./ledger.mjs`.
* **Only tasks both ledgers finished** are compared, matched per agent on the `task` of each run's `Planned` record; the
  others are listed as left out, never folded in.
* **Every attempt counts.** Calls are keyed by attempt and call number, so a resumed run's attempts no longer overwrite
  each other (the fix this slice made to calls per task).
* **The verdict states decision 5's rule, untuned**: `read_output` calls per task against input tokens per task, with no
  threshold.

What a ledger cannot give: which tool's output a `read_output` call read (the meter keeps a call's name, not its
arguments), the tokens hidden outputs saved apart from every other difference between the two series, names past the
first 64 of one answer (`MAX_TOOL_NAMES`), and the cache share of the whole prompt (the figure is the fixed prompt's).

| Proof | What it shows |
|---|---|
| `bench/test/numbers.test.mjs`: the new figures (a child session, a failed call, a call with no usage, an unfinished run), a resumed run across attempts, the comparison and its left-out lists, the verdict cases, the printed rows, the CLI with `--against` and its two exit-1 cases, and the read-only run under `--permission` with the network made to throw | the figures, the comparison and the guarantee |

The result table in `bench/NUMBERS.md` ("ADR 0088: hidden outputs") waits for the owner's runs.
