# 0034. A turn that asks nothing completes, and a scratch project lives for the thread

Status: **Accepted** (2026-10-11)
Amends [ADR 0013](0013-run-keeps-changes-edit-file-and-scratch-completion.md) (decision 4: a stop with no pull request was
always a question; and its consequence "a scratch project is deleted when its run completes") and
[ADR 0008](0008-a-workspace-holds-several-repositories.md) (decision 5: "deleted with the workspace"; the alternative "a
workspace per conversation"). Builds on [ADR 0003](0003-a-new-task-continues-the-task-it-references.md).

## Context

The owner tested Adam in production on a thread of seven jobs (2026-10-11). Two things cost him time:

* **A finished job showed "blocked".** The final answer of job 7 was "Merged - main is at 1912174", which asks nothing, and
  the task still ended `input-required`. `CoderAgent::step` (`bin/adam-coder/src/agent.rs`) turned **every** stop without a
  pull request into a question (`stop_as_question`) unless a scratch run had shared a file, and ADR 0013 wrote that down: "a
  scratch run that answered a question and shared nothing still waits... there is no tool-written sign that an answer was the
  whole result". The instructions said the same ("the conversation waits for what they say next"). A thread that is only
  being told something looked stuck.
* **The site was rebuilt three times.** A scratch project belongs to a run, the run is a task, and the janitor deletes a
  finished run's workspace at its next sweep. The next job of the thread found nothing and made the files again.

## Decision

1. **A turn that asks nothing completes the task.** When the model stops (a reply with no tool call) and there is no pull
   request, no failure to report and no shared result, the run **completes** unless one of these holds, in which case it
   waits as before (`input-required`, the reply as the question):
   * the reply **asks**: its last paragraph, outside code and links, has a sentence ending in a question mark
     (`asks_the_person`); a question written with `ask_user` always waits;
   * the reply is **empty** or was **cut by the length limit**;
   * **repository work is left undelivered** (`owes_delivery`): a repository's worktree has changes nobody committed or
     commits nobody pushed, or the run pushed and opened no pull request. Scratch projects do not count: they stay for the
     thread (decision 2), so what was not shared is still there for the next message.

   The next message of the thread starts the next task, which continues the conversation (ADR 0003), so a completed task
   loses nothing. Red checks with the budget spent and rejected credentials still fail the run first.
2. **A scratch project lives for the thread, not for the task.** The tasks of one A2A context are one thread, and A2A has no
   "delete the thread". So:
   * a scratch project records the conversation of the run that made it (`ScratchMeta::conversation`);
   * the **first step of a run moves** the scratch projects that other runs' workspaces hold for the same conversation into
     its own workspace (`RunWorkspace::adopt_scratch`), by renaming the directory, under the locks of both runs; a name the
     run already has is left where it is;
   * the janitor still releases a finished run's environment and removes its worktrees, but **keeps its scratch projects**
     until the conversation has had no task for `SCRATCH_IDLE_DAYS` (default **7**; `0` restores the old rule: deleted with
     the run). It measures from the newest run of the conversation in the store, so a thread with an open or recent task is
     never swept, and a run the store does not know (a purged one) is removed whole, as before;
   * `start_scratch` of a name that is already there returns the project, with a note of what it holds.

## Consequences

* A "yes, publish it to acme/site" a day later finds the files. A scratch project is still not an artifact of record: it is
  deleted after the idle time, and what the person wants kept goes to a repository they name.
* A task that only reports ("did the checks pass?", "Merged") is `completed`, not `blocked`. A reply that is a request with
  no question mark ("Tell me the repository") is not read as a question; the instructions say to ask with `ask_user` or end
  with the question.
* `Janitor::new` is unchanged; `Janitor::with_scratch_idle` and `Sweep::kept_scratch` are new (`Sweep` has a public field
  more), and `adam-workspace` gains `RunWorkspace::adopt_scratch`, `RunWorkspace::remove_keeping_scratch`,
  `RunWorkspace::add_scratch_for` and `Scratch::conversation`. No required trait method is added. Metadata written before has
  no conversation: such a project is not adopted and is swept with its run.
* The projects move between the workspaces of one volume. With the `affinity` and `isolated` placements the next task of a
  conversation may be claimed by another worker, whose volume does not have the project: it makes a new one, as it did before
  (as for any file of a run). The `shared` placement and a single worker find it.
* The retained scratch of a finished run holds disk for up to the idle time. The volume must be sized for it; the knob is
  `SCRATCH_IDLE_DAYS`.
* What Adam may do is unchanged: no tool is added, and a repository is still only used on the person's word.

## Alternatives rejected

* **A tool the model calls to say "I am done" (`finish`).** ADR 0013 rejected it too: one more tool to forget. The words of
  the last paragraph are the sign, and the instructions tell the model how to ask.
* **Complete any reply, even with work undelivered.** A stop with changes in a worktree is a model that gave up halfway, and
  completing it would drop the work silently.
* **Read the whole reply for a question mark.** A rhetorical question in a long report would make it a question; whoever asks
  ends with the question.
* **A workspace per conversation** (ADR 0008's alternative). Repository slots are per run on purpose (branch `agent/<run>`);
  moving the scratch projects is smaller than moving the unit.
* **Keep scratch projects forever.** The volume would fill; the idle time is the owner's decision, 7 days.
* **Keep the old rule and ask the model to publish early.** It does not help a person who has not named a repository.
