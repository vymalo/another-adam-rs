# 0026. A failure the base has too is not the run's

Status: **Accepted** (2026-10-07)
Builds on [ADR 0013](0013-run-keeps-changes-edit-file-and-scratch-completion.md) (the check budgets, and why scratch work has
its own) and [ADR 0021](0021-the-coder-is-adam-a-general-agent-that-can-code.md) (the run notes are the root run's).

## Context

The owner exported production threads of Adam (image `sha-6478fbc`). In one, `yarn typecheck` and `yarn check` failed on
errors that were **already in the repository** before Adam touched it. Adam read the output, could not make them pass without
fixing unrelated code, ran the check three times, spent its three check cycles and ended `failed` with no pull request. A
check that is red on `main` is the repository's, and a run that cannot fix it should still deliver the change it was asked for.

## Decision

1. **A failing check is run once more on the base**, in a repository's worktree. After `run_checks` ends with a non-zero
   exit code (not a timeout, a signal or a missing tool), the same command is run on `origin/<base>`:
   `Worktree::add_base_checkout` makes a detached checkout in `<run's workspace>/.adam-base/<slot>` (a second worktree of the same
   mirror, inside the directory the environment binds, so it works in a devcontainer and a run pod), the command runs in the
   **same environment session** (`EnvSession`, reused), and `remove_base_checkout` removes the checkout, its links and the
   mirror's entry afterwards, also when the command fails.
2. **The installed dependencies are linked in.** A fresh checkout has no `node_modules` or `target`, so `yarn typecheck`
   would fail on the base for that reason alone and a real failure would pass for a pre-existing one. What the worktree has
   and git ignores (`git ls-files --others --ignored --exclude-standard --directory`) is symlinked into the checkout.
3. **It fails there too: pre-existing.** `CheckRecord::preexisting` and `base_commit` are set; the failure does **not** count a
   cycle (`RunNotes::record_check`); the tool's answer says plainly that the command fails on `origin/<base>` too, shows the
   base's output beside the run's, says whether they are identical, and that the change must not make it worse; the `checks`
   artifact has `passed: false`, `preexisting: true` and `base_commit` (the contract is in the coder README, and
   `commit_and_push` keeps both when it binds the report to the pushed commit). A failure that **passes** on the base is a normal
   failure that costs a cycle, and the answer says the base passes.
4. **The coder's own gate lets it through.** `open_pull_request` does not need `accept_red_checks` when the decisive check on the
   pushed tree is a pre-existing failure, and the pull request body says that the command fails on the base too. Without this the
   symptom would stay: the prompt reserves `accept_red_checks` for the person's explicit consent, so Adam would still open
   nothing. The orchestrator's gate (another repository) reads `preexisting` from the artifact.
5. **Once per command, directory and base commit.** The result (`BaseResult`: failed or passed, exit code, output tail) is kept in
   the run notes (`ChecksNotes::base`, at most 16) and saved as soon as it is known, so a command that fails again, a
   replayed call (a lost journal write) and a subagent's call do not run the base again. The notes are the **root run's**
   (`ToolCtx::root_run_id`), and the key holds no call id; the call-keyed parts, `CheckRecord::call_id` and `counted`, still
   use `ToolEnv::call_key`, so a subagent's calls are not replays of the root's (ADR 0021).
6. **Why not scratch.** A scratch project has no base: it is built from nothing, so every red it has is its own, and it already
   has the larger budget for that (ADR 0013). A timeout is not asked again either, since the base would cost the limit once
   more and a hang is as likely the change's.

## Cost and limits

* **One extra run per failing command** (per directory and base commit), as long as `CHECK_TIMEOUT_SECS` (the same limit), while the
  change's run is already done. A command that fails the same way on every call costs two runs in all, not two per call.
* **It compares exit codes, not errors.** A command that fails on the base and fails with more errors on the change is
  pre-existing by this rule. The model is told not to make it worse and is shown both outputs; a reviewer sees `preexisting`
  and the base's commit. Comparing outputs in general is not possible (timings, paths, ordering).
* **The base is checked with the worktree's dependencies**: a change that added a dependency leaves it installed on the base,
  which is harmless for a failure, and a change that upgraded one is checked against the base's code with the new one.
* A leftover checkout of a process that died is replaced at the next base run; the janitor removes the run's workspace, and with
  it `.adam-base`, when the run is over.
* Not decided here: whether the orchestrator's gate accepts `preexisting: true` (its repository). Until it does, a consumer
  that gates on `passed` sees `false`, as before.

## Alternatives rejected

* **`git stash`, or checking the base out in the worktree itself.** A crash between the two steps loses the run's changes.
* **A copy of the worktree.** It copies `node_modules` and `target` for every check.
* **Telling the model to run the command on the base itself.** It cannot be made to, and its own checkout would be an
  unguarded git operation (`run_command` refuses them).
* **Not counting any red the model says is unrelated.** A claim of the model is not evidence; the base's run is.
