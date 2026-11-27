# Hook programs: the invariants and the evidence

For people: [Hook programs](../../../docs/reference/hooks-protocol.md), the wire format and what a program may
decide. This page is what to check when you change `adam-hook-exec` or the protocol, and the proof behind each
claim.

## The invariants

1. **No I/O under the state lock.** `Program::state` guards the child, the writer queue and the backoff, and
   nothing else. A writer thread owns the child's stdin and takes lines from a bounded queue
   (`WRITE_QUEUE`); a reader thread owns its stdout and hands each reply to the request waiting for it. A
   program that stops reading fills the queue and is treated as broken: a slow program cannot hold the loop
   (`crates/adam-hook-exec/src/program.rs`, `request`, `write_lines`, `read_lines`).
2. **A line is at most 256 KiB each way** (`protocol::LINE_MAX`). The read is capped with `take(LINE_MAX + 1)`,
   so one line cannot grow this process's memory; the content length is compared without the trailing newline.
   A request over the bound is refused before it is queued (`Error::Malformed`).
3. **Fail closed, and the class says what to do.** A missed deadline is `Transient`; three in a row kill the
   program. A closed output, a broken line or a dead child kills it at once. While it is down, every deciding
   point is `Transient` (`State::broken`, `State::miss_deadline`). A verdict the point does not allow, an
   action (slice H1d, not built) and an unusable configuration are `Invalid`.
4. **One kill advances the backoff once.** `broken` returns early when the child is already gone and
   `down_until` is set: sixteen waiters woken by one `fail_all` must not walk the backoff to 30 s.
5. **The child sees only what it should.** `env_clear`, then `SAFE_VARS` (the same list as `adam-mcp`'s, which
   is private there), then the configuration's variables, each checked to be a plain upper-case name and not
   `PATH`, `HOME`, `LANG`, `TMPDIR`, `LD_*`, `BASH_ENV`, `ENV` or `IFS` (`config::check_env_name`). It runs in
   an empty private directory (`private_dir`, mode 0700, removed on drop) or one the deployment names.
6. **Payloads are scrubbed whole** (`HookExec::ask`), and a rewrite is applied only when the scrubbed payload
   equals the real one — a program cannot write a redaction marker into a real call. A verdict's reason is
   scrubbed too: it is recorded and read by the model. A malformed reply's serde detail is logged, never put in
   `HookError.reason`, which the journal and the model read.
7. **The key.** A deciding hook has no key from the loop (the shipped `Plugin` trait has no `HookCtx`), so it
   gets `<plugin>/<point>/<id>`, unique per request; a replay sends no request at all. An **observer** gets the
   event's own `header.key`, stable across replays, so a program that dedupes on it does not write twice.
8. **The deadline.** `HookLimits` is per `HookSet`, not per plugin, so the adapter enforces decision 8's 2 s (at
   most 30 s) itself. A host whose set deadline is shorter than the adapter's will see a compliant program fail
   after the fact: build the set with `HookSet::with_limits`.

## The tests, and what each one is worth

| Claim | Test | How it fails without the guard |
|---|---|---|
| A verdict round-trips, a rewrite is applied | `tests/exec.rs`, `a_rewrite_of_a_tool_call_is_applied`, `a_message_and_a_tool_answer_are_rewritten`, `a_compaction_context_and_a_model_request_are_rewritten` | drop the `*arguments =` assignment |
| Garbage denies | `garbage_denies` (`Invalid`) | return `Continue` on `Fault::Malformed` |
| A dead program is transient, and the set gives up | `a_dead_program_is_transient_and_the_set_gives_up`: the class, and the program asked exactly once | map `Fault::Closed` to `Continue` |
| A run fails when its program never answers | `tests/replay.rs`, `a_run_fails_when_its_program_never_answers`: the run ends `Failed`, the tool never ran | answer `Continue` on a closed program |
| Only the safe variables | `the_child_sees_only_the_safe_variables`: every key is `SAFE_VARS` or configured | remove `env_clear` |
| A configuration cannot override one | `a_configuration_may_not_override_a_safe_variable_or_add_a_dangerous_one` | drop `check_env_name` |
| A rewrite over a scrubbed span is refused | `a_rewrite_over_a_scrubbed_span_is_refused` | drop the `clean != *arguments` check |
| A missed deadline is transient | `a_slow_program_misses_its_deadline` | drop the `recv_timeout` |
| Three deadlines kill, and it restarts | `three_deadlines_in_a_row_kill_the_program`, `a_program_that_dies_is_restarted_after_its_backoff` | keep `child` after `broken`, or never restart |
| Sixteen calls at once do not cross | `sixteen_calls_at_once_each_get_their_own_reply` | reply to the first waiter for every id |
| A replay sends no request | `tests/replay.rs`, `a_replay_sends_no_request_for_the_recorded_step`: the program's log holds `a=1, b=2` | call the hook outside the journal |

The test program is `tests/program/main.rs` (bin `adam-hook-exec-test-program`); `autotests = false` and the two
`[[test]]` entries are declared in `Cargo.toml` because cargo would otherwise discover `tests/program/main.rs` as
a third integration test.

## The gates

`cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p
adam-hook-exec`, `cargo nextest run -p adam-hook-exec`, `node tools/docs-check/check-docs.mjs`, and
`cargo doc -p adam-hook-exec` (a broken intra-doc link is a red `rustdoc` job).

## Not built

The deployment's file and switch (`ADAM_HOOKS_FILE`, `HOOKS_ALLOW_PROGRAMS`) and the wiring into `adam-agent`
and `adam-coder` are slice **P2**; the chart's `hooks.file` is **P3**. Actions are **H1d**: a reply that asks for
one is refused with `Unsupported`, never dropped.
