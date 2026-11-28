# 0088: the evidence and the slice proofs

For people: [0088. A tool result is bounded when it is made, and a command that succeeds can say only that](../../../docs/decisions/0088-a-tool-result-is-bounded-when-it-is-made-and-a-command-that-succeeds-says-so.md),
which states the decision. This page holds where each slice is in the code, the invariants to check when it changes, and
the tests that prove it.

Slice T1 is built (2026-11-28, on `3d1e5b5`): [its proofs](#t1-built-2026-11-28). T2, T3 and T4 are not.

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
