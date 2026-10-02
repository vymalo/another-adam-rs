# 0013. The coder has `run` (a command that keeps its changes) and `edit_file`; scratch work that shared a result completes without a pull request

Status: **Accepted** (2026-10-02), with plan 10 of 2026-10-02. The scratch rule (decision 3) is the owner's decision 2 of that
plan: "the coder may finish without a PR when a result was asked for; it shares the files and offers publishing in one
sentence." The owner's words that started it: "Not bad, but not complete. The agent doesn't have enough freedom." **Built:**
the tools, the completion rule, the two budgets of check cycles, the instructions and the tests.

## Context

The owner's coder chat of 2026-10-02 showed three blockers, all in the coder and none in the model:

* **It could not run a command that makes a file.** `run_command` undoes every change to the worktree (it is for looking), so
  the coder ran `npm run render` through `run_checks`. The file appeared, and the job's gate recorded "`npm run render`
  passed" as one of the project's checks: a tool misused for a job it was not made for, and a gate that read it as proof.
* **`apply_patch` failed on the first try twice.** A unified diff needs line counts and context that a model writes badly;
  each retry succeeded, and each failure was a failed step on the person's screen.
* **The run ended `blocked` after the work was done.** The scratch project had the result and the person had it asked for; the
  instructions ("Ending your turn") said that anything but a pull request, a question or an answer "does not complete the run",
  and the code agreed: a stop without a pull request parks the run as a question. A run that was asked for a drawing, not for a
  change to a repository, could never be finished.

A fourth, smaller one: the check budget (`MAX_CHECK_CYCLES`, 3) was used up in the chat on the coder's own wrong assumptions
about a script it was building from nothing.

*Verified 2026-10-02 by reading the code at commit `0e44c14`:* `run_command` (`bin/adam-coder/src/tools/inspect.rs`) snapshots
the worktree and git state before a command and undoes and refuses any change after it; `run_checks` records every run in the
run's notes (`RunNotes::record_check`), spends a cycle on a failure, emits the `checks` artifact, and the gate of
`open_pull_request` binds a verdict to a tree through those notes; `CoderAgent::step` turns a stop with no pull request into
a question (`stop_as_question`) unless the run failed.

## Decision

1. **Three tools share the shell, one job each.**

   | Tool | For | Changes to the files | A check? |
   |---|---|---|---|
   | `run_command` | looking around | undone, and refused | no |
   | `run { command, cwd?, repo? }` | making a file or changing the worktree with a command | **kept** | **no** |
   | `run_checks { command, cwd?, repo? }` | the project's own checks | kept | yes: a cycle, a `checks` artifact, the gate |

   `run_command` stays as it is. `run` runs exactly as it does: the run's environment (the repository's devcontainer, ADR
   0010), the login shell, a `cwd` inside the worktree, `CHECK_TIMEOUT_SECS` and the output cap, the process's secrets hidden
   from the child, a missing tool reported as a missing toolchain. The three descriptions say which is which.

2. **`run` keeps files and nothing of git.** The worktree is snapshotted before the command as `run_command` does it (git
   directory, `.git` file, `HEAD`, branch, refs, local configuration, `info/exclude`); after it, everything but the files is
   compared. A command that changed any of it is **undone entirely, its files included, and refused**: one that moved `HEAD`
   or rewrote `.git` did more than make a file, and what it made on the way cannot be told from what it broke. The model is
   told to commit with `commit_and_push`. This is a guard against accidents, as `run_command`'s is, not isolation (the refs
   and configuration are shared with every run on a mirror, and what other runs change meanwhile is left alone, as there).
   **`run` is never a check:** no cycle, no `checks` artifact, nothing written to the run's notes, so the gate cannot see it.
   What it changes changes the worktree's tree, so the verdict of a check that passed earlier no longer covers the code
   (`RunNotes::checked` is by tree): the result says to run the checks again, and `commit_and_push` binds no verdict to a tree
   nobody checked. The result lists what `git status` shows (files the project ignores are kept too and are not listed).

3. **`edit_file { path, old, new, replace_all?, repo? }` replaces exact text.** The path is confined as `write_file`'s is (no
   `..`, no absolute path, nothing inside `.git`, no symlink on the way), the file must exist and be UTF-8 text of at most
   1 MiB, and the write is `write_file`'s (a new file renamed over the old, the mode kept). It **fails and changes nothing**
   when `old` is not in the file, and then shows the **closest region**: the lines of the file that most nearly are `old`
   (lines of `old` that are in the file, ignoring the blanks at their ends, vote for the window they would sit in; failing
   that, the most alike pair of one of the longest lines and a line of the file, by character pairs), with their numbers, two
   lines of context, the first line where they differ, and a hint when only blanks differ or the file has CRLF endings. It
   fails when `old` is in the file more than once and `replace_all` is not set, and says how many times and at which lines.
   `apply_patch` stays, for several changes at once.

4. **Scratch work that shared a result completes without a pull request** (owner decision 2). When the model stops (a turn
   without a tool call) and there is no pull request, the run **completes** if all of these hold, each of them written by a
   tool and none of them said by the model:

   * the run shared a file (`share_file` notes `<slot>/<path>` in `RunNotes::shared`): the result was handed over;
   * the workspace has scratch projects and nothing else: no repository worktree is in it, so nothing could be pushed, and a
     project published into a repository (`publish_scratch` adds its slot) is repository work;
   * no repository is in play: the person named none (`named::listed` of the notes, which leaves out a word like `src/main.rs`
     that only reads like one), the run created none, pushed nothing and continues no branch.

   Anything else is what it was: a stop with no pull request is a question and the run waits, and a run that must fail
   (credentials rejected, a spent check budget) fails first, whatever it shared. A model that needs an answer before it can
   deliver asks with `ask_user`, which parks the run whatever was shared. The instructions say it in the owner's words: "You
   delivered what was asked. When the person asked for a result (a file, an answer), not for a change to a repository, share
   it and finish; offer publishing in one sentence." Repository work (a repository named, a change requested to one) still
   needs its pull request.

   ```mermaid
   stateDiagram-v2
     [*] --> Stopped: the model replies without a tool call
     Stopped --> Failed: credentials rejected, or the check budget spent and the last check red
     Stopped --> Done: a pull request was opened
     Stopped --> Done: scratch work only, a file shared, no repository in play
     Stopped --> WaitingForTheAnswer: anything else (a question, an answer, work not handed over)
     WaitingForTheAnswer --> Stopped: the person writes again
     Done --> [*]
     Failed --> [*]
   ```

   The rule is `delivered_a_result` and the branch of `CoderAgent::step` that calls it (`bin/adam-coder/src/agent.rs`).

5. **Scratch projects get five check cycles, repositories keep three, counted apart.** A failed run counts against the budget
   of the kind of slot it ran in (`CheckRecord::scratch`, `ChecksNotes::scratch_failures`), a budget is judged by the last run
   of its own kind, and the refusal of `run_checks`, `commit_and_push` and `open_pull_request`, and the verdict of a run that
   ends, use the budget of their slot. `MAX_CHECK_CYCLES` (3) is unchanged; **`SCRATCH_CHECK_CYCLES`** (5, at least 1) is new.
   Notes written before this are repository work. The prompt tells the model both numbers (`{{scratch_check_cycles}}`) when the
   agent folder declares the var, which the shipped one does; a folder written before it does not, and keeps working (a var the
   code supplies and the folder does not declare is an error, so the code supplies it only when it is declared), and the tool
   says the limit when it is reached.

## Consequences

* **The coder can generate files, edit without a diff, and finish scratch work.** Its tool list is twenty (seventeen of the
  workflow and three of the screen), and its instructions and every changed tool's spec change, which changes what a replayed
  run would have been shown, as in any change to a tool: the snapshots are regenerated in the same commit.
* **The gate is not weakened.** A check that passed is for its tree, `run` changes the tree and records nothing, and a pull
  request is still opened only for code a check passed on. What changes is that nothing needs to be disguised as a check to
  make a file.
* **A scratch project is deleted when its run completes** (ADR 0008, decision 5), and a run that completes without a pull
  request completes. The offer to publish is then an offer for a **follow-up**: a person who answers "yes, to acme/fib" starts a
  task that continues the conversation (ADR 0003), and the coder has to make the project again in a new scratch project from
  what it wrote and what it shared; the instructions say so. This costs a rebuild; keeping a completed run's scratch project
  for a while would avoid it and is not built (the janitor sweeps a finished run's workspace at its next sweep, ADR 0008).
* **A scratch run that answered a question and shared nothing still waits**, as every answer without a pull request did. There
  is no tool-written sign that an answer was the whole result and not a question, and the owner's rule is about handing over
  files; `share_file` is that sign. A deployment that wants answers to complete too would add an explicit tool.
* **A person who answers the model's offer** is in a completed thread: that is the orchestration layer's to continue, as it
  does for a finished pull-request task.
* **`edit_file` is repeat-safe by being exact:** a repeat after a crash between the write and the journal finds `old` gone and
  says so, and the result of the first call is what the model had.
* **A folder written before `scratch_check_cycles` keeps working** and tells the model one number (its own `max_check_cycles`)
  while the tools enforce both.

## Alternatives considered

* **Let `run_command` keep its changes.** Rejected: its refusal is what keeps a look from being an edit, and the checks of
  the owner's chats depend on exploring without side effects. A second tool with a different promise is clearer than a flag.
* **A flag on `run_checks` (`record: false`).** Rejected: the model that misused `run_checks` would still have the tool
  described as "checks"; two jobs under one name is how the gate was polluted.
* **Keep what a git-touching `run` made and undo only the git part.** Rejected: restoring `HEAD` under a worktree whose files
  belong to another commit leaves a state nobody chose. Undoing all of it is simple to say and to trust.
* **Fuzzy edits (apply the closest match).** Rejected: an edit that lands somewhere the model did not name is worse than one
  more turn. The closest region is for the next try, not for a guess.
* **A tool the model calls to say "this is the result" (`finish`).** Not built: it is one more tool and one more thing to
  forget. A file shared from scratch work with no repository in play is the same signal, already written by a tool.
* **Complete any scratch run that stops.** Rejected: "write a script, I'll give you the repository later" is a stop that
  asks for a repository, and completing it would drop the project the person means to publish.
* **One budget of five for everything.** Rejected: the repository's three is the owner's bar for changes to a project that
  has its own checks; the extra tries are for building from nothing.

## Verified and unverified

* *Verified 2026-10-02*, by tests in this repository: `run` keeps a file it made, lists what changed, is no check (no artifact,
  no cycle, nothing in the notes, any number of failing runs) and leaves a check that passed before stale for the gate
  (`bin/adam-coder/tests/tools.rs`, `run_keeps_what_a_command_makes_and_is_never_a_check`); a commit, a branch, a ref, a
  configuration key and a rewritten `.git` file are each undone with the files the command made and refused
  (`run_undoes_a_command_that_touches_git_entirely_and_refuses_it`); it times out and keeps what it wrote, keeps to its `cwd`
  rule, reports a missing tool and hides the process's secrets (`run_respects_the_timeout_the_cwd_rule_and_a_missing_tool`);
  `edit_file` replaces once and everywhere, refuses two places and says where, shows the closest region on a miss, keeps the
  mode and the path rules (`bin/adam-coder/src/tools/files.rs` and `tests/tools.rs`); the two budgets are counted apart
  (`bin/adam-coder/src/tools/notes.rs`, `src/agent.rs`, `tests/tools.rs`, `src/config.rs`); the completion rule holds in each
  combination (`delivered_a_result` in `src/agent.rs`); and, through the runtime on memory (and PostgreSQL where it is
  configured), a scratch run that makes a file with `run` and shares it completes `Completed` with the file as an A2A artifact,
  no pull request and no check, while one that did not share it, and one in a repository, still wait (`tests/e2e.rs`).
* *Unverified:* that the closest-region choice is the best on a real model's near misses (the heuristic is tested on
  indentation, a changed word and a file with CRLF endings); and how the orchestration layer shows a task that completed with a
  file and no pull request (its side of the contract, ADR 0032 of that repository).
