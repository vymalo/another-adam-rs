# Embed adam in an application: the evidence

For people: [Embed adam in an application](../../../docs/guides/embed-in-an-app.md). That page decides; this one gives
the code each claim stands on, the invariants to check and the proofs. It covers the approvals of ADR 0047 slice 5
and the local agent of slice 7 part 2; the rest of the engine's evidence is in `crates/adam-engine/README.md` (*Tests*).
Checked on 2026-11-27 on the branch that built slice 5, over ADR 0050 slice H8, and on 2026-11-28 on the branch that
built the local agent (re-cited after the asking hook became fail-closed and the guard became exclusive).

## Approvals: where the code is

| Claim | Code |
|---|---|
| the host's hooks reach every agent, before a file's plugins | `EngineBuilder::hooks`, `plugin` (`crates/adam-engine/src/engine.rs:260`, `:268`), given at `start` (`:368`) to `BoundDef::hooks` (`crates/adam-assembly/src/assembly.rs:290`), folded first into each node's set (`:946`) |
| what an approval shows of its call is what the step shows | `EngineBuilder::step_io` (`crates/adam-engine/src/engine.rs:278`); `StepIo::shown_input`, `redacted` (`crates/adam-llm-agent/src/step_io.rs:112`, `:126`); the `input_required` event's question through the agent's redactor (`crates/adam-llm-agent/src/agent.rs:4924`) |
| a run parked on a hook's approval is `AwaitingApproval` | `Shared::approval_of` (`crates/adam-engine/src/engine.rs:814`): parked and waiting, `Conversation::approval` not approved, and `pending_wait.call_id` the same call; `Engine::info` (`:1221`) reads the head for a run the task says is `Waiting`; `RunInfo::awaiting` (`crates/adam-engine/src/types.rs:297`) |
| `approve` checks the call, then sends the plain follow-up | `Engine::approve` (`crates/adam-engine/src/engine.rs:1106`): the owned view, `Finished` for an ended run, `NotAwaitingApproval` (`crates/adam-engine/src/error.rs:54`) unless `approval_of` names the call; the message id `approval:<run>:<call id>:<version>`; `Decision::answer_text` (`crates/adam-engine/src/types.rs:254`) |
| the stream says an approval and its answer from the store | `Follow::approvals` (`crates/adam-engine/src/events.rs:277`); a read on a status, on `input_required` with `"approval": true` (`approval_named`, `:324`) and on the end of the owed call's step (`ends_the_step_of`, `:338`); `Engine::approval_outcome` (`crates/adam-engine/src/engine.rs:1168`) |

## Invariants to check

1. **A yes is known only from the journal or the head.** `approval_outcome` answers `true` when the step
   `tool:<call id>:approved` is journaled (before its transition commits) or the head says `approved`.
2. **A no is never inferred from an absence.** It needs the call's result in the run's history without the approved
   step. An ask heard before its park is committed (or whose commit was lost) reads as no answer yet.
3. **One `ApprovalAsked` per approval in a stream** (`Follow::asked`), and one `ApprovalAnswered` per approval it heard
   of (`Follow::owed`). A cancel before the answer says none.
4. **`approve` delivers nothing it refuses**: the check of the call comes before the submit, so a wrong call id leaves
   the run waiting and its model uncalled.
5. **A refusal never reads as a yes**: a reason that `adam_llm_agent::approves` accepts is sent as `no: <reason>`.
6. **No more than the step**: `ApprovalRequest::arguments` is `StepIo::shown_input`, never the raw arguments, and both
   questions go through `StepIo::redacted`.

## The proofs

`crates/adam-engine/tests/approval.rs`, each seen failing once against the change in the last column, then passing:

| Test | Holds | Red against |
|---|---|---|
| `a_yes_runs_the_same_call_once_across_restarts` (`:305`, Unix) | three engines over one SQLite file: the approval after a restart without asking the hook, the call run once across two restarts, the next engine's model reading its result once | `Decision::Approve` sent as `no`; `approval_of` that reads no approval |
| `a_replay_never_asks_twice` (`:453`) | a `FaultyStore` loses the park's commit: the hook asked once, `ApprovalAsked` once over more than a poll, the lost attempt's ask not taken for an answer, the call run once | `ApprovalAsked` pushed at every read; a no read from an absence |
| `a_no_refuses_the_call_and_the_run_goes_on` (`:541`) | the redacted request, the refusal's words reaching the model, the run's answer, `ApprovalAnswered { false }`, no secret in any event | a refusal sent as `yes`; the raw arguments shown |
| `a_refusal_that_says_ok_is_still_a_no` (`:600`) | `refuse_because("OK")` runs nothing, the model reads `no: OK` | an `ok` reason sent as it is |
| `an_answer_for_another_call_or_no_approval_is_refused` (`:640`) | another call's id, a working run, a run waiting on `ask_user`, an ended and an unknown run refused with their classes; a `send` of `Yes.` is `Answered` | no check of the call id; a `send` to an approval delivered as a steer |

Also: `crates/adam-assembly/tests/plugins.rs` `the_hosts_hooks_run_in_every_agent_before_the_files_plugins` (red against
a fold that leaves the host's set out), `crates/adam-llm-agent/src/step_io.rs` `the_shown_input_is_the_steps_own` (red
against an input not cut), `crates/adam-engine/src/error.rs` `every_backend_refusal_keeps_its_class` (the new variant's
class), and `crates/adam-engine/tests/example.rs` `the_example_asks_before_saving_a_note`.

## Not proven

* **A2A clients see no approval as one.** The A2A front reads the parked question as any `input-required`; the new
  extension of ADR 0047 confirmed decision 4 is not built.
* **The order of a refusal among live events.** A no is not journaled, so its `ApprovalAnswered` comes at the commit of
  the transition that read it, which may be after the model's next words; the example's test allows both places.
* **A race between two answers.** `approve` reads the approval, then submits: an answer delivered by another call in
  between is read first, and the second is the run's next message, as for `answer`.

## Local agent: where the code is (slice 7 part 2)

| Claim | Code |
|---|---|
| the builder option `EngineBuilder::local_agent(dir)` adds file and command tools on top of the screen's tools | `EngineBuilder::local_agent` (`crates/adam-engine/src/engine.rs:300`); the path is opened at `start` (`:368`) and shared with the tools as `State<PersonDir>` (`:452`); the `ToolSet` of the local agent's seven tools (`crates/adam-engine/src/local_agent.rs:377`) is appended to the host's (`:441`) |
| reads pass without asking; writes and commands ask | `AsksBeforeWritingOrCommand::before_tool` (`crates/adam-engine/src/local_agent.rs:49`); reads (`read_file`, `read_files`) fall through to `Verdict::Continue`, the five mutating tools return `Verdict::Ask` always (a missing or empty argument still asks, so the run never bypasses the approval); the question names the path or command, with `cwd` for the command tools, and the patch's paths (`+++ <component>/<path>` or `+++ <path>`, the first component dropped as `git apply -p1` does) up to 8 then "and N more" |
| the directory is never owned, cleaned or restored | `PersonDir`'s `CodeWorkspace::after_write` returns empty (`crates/adam-code-tools/src/person.rs:188`) and its guard settles `FilesChanged` or `Unchanged`, never `Undone` (`crates/adam-code-tools/src/person.rs:287`); the guard is exclusive — a second guard for the same place is refused with the message the tool appends to its error, leaving the first guard's entry untouched |
| the same `Engine::approve` answers the ask | the hook is appended to the host's `HookSet` (`crates/adam-engine/src/engine.rs:442`); the run parks as `RunState::AwaitingApproval` and is answered as any other hook (the approvals of slice 5) |

## Local agent: the proofs

`crates/adam-engine/tests/local_agent.rs`:

| Test | Holds | Red against |
|---|---|---|
| `a_yes_writes_the_file_and_nothing_restores_it` (`:155`) | a yes on a scripted `write_file` writes the file, the file is on disk with the content, and after `shutdown` (engine dropped) the file is still there | a `restore` that puts the file back; a snapshot undo |
| `a_refusal_does_not_write_the_file` (`:203`) | a no on the same scripted `write_file` does not create the file | the hook returning `Continue` (the red proof, see `without_the_asking_hook_a_write_runs_without_approval`) |
| `a_read_does_not_ask` (`:238`) | a scripted `read_file` runs to `Done` with no `RunInfo::approval`; no `ApprovalAsked` is asked | the hook returning `Ask` for reads |
| `a_yes_runs_the_command_and_a_refusal_does_not` (`:262`) | a `run_command` parks on an ask; a yes runs the command and the file appears; a refusal leaves it absent | a `restore` that puts the file back; the hook returning `Continue` |
| `an_approval_for_a_run_that_awaits_none_is_refused` (`:318`) | `approve` of a finished run is `Rejected` (`ErrorClass::Rejected`) | no check that the run awaits |
| `without_the_asking_hook_a_write_runs_without_approval` (`:722`) | with `AlwaysContinue` as the only hook (the local agent's asking hook is not installed), the same `write_file` runs without ever parking: the run goes straight to `Done`, the file exists | `Verdict::Continue` refused |

The hook's fail-closed behaviour, the patch's `-p1` paths, and the `read_files` non-ask: covered by the
integration tests `an_apply_patch_with_an_arbitrary_first_component_still_asks`,
`an_edit_file_asks_and_a_refusal_does_not_edit`, `an_apply_patch_asks_and_a_refusal_does_not_apply`,
`a_run_asks_and_a_refusal_does_not_run`, `a_read_files_does_not_ask` and the bogus-directory checks for
a file and a symlink (`a_bogus_directory_is_refused_at_start`). Unit tests for the hook itself (a
`HookedCall` is built with `tool` and `arguments` JSON) live in `crates/adam-engine/src/local_agent.rs`'s
test module: each of the five mutating tools parks with the path or command named, even when its
argument is missing or empty; `read_file` and `read_files` never park; a patch of more than eight
paths names the first eight and says "and N more".

## Local agent: PersonDir's guard

`crates/adam-code-tools/src/person.rs`:

| Test | Holds | Red against |
|---|---|---|
| `a_second_guard_for_the_same_place_is_refused` (`:442`) | a second guard for the same place is refused with the message the tool appends to its error result, leaving the first guard's entry untouched: it settles `Unchanged`, `after_run` reads it once and clears it, and a fresh guard is taken | `guard` overwriting the entry instead of refusing |
| `a_changing_command_settles_files_changed_and_a_second_guard_is_taken` (`:484`) | a command that adds a file settles `FilesChanged`, the report names the file, the entry is cleared, and a second guard is taken | the report leaking into the next command; `Unchanged` for a real change |
