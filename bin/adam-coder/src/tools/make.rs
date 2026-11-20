//! `run { command, cwd?, repo? }`: make something with a shell command, and keep it.
//!
//! The coder produces files with commands: a build, an export, a render, a generated chart. Until
//! this tool the only command that could was `run_checks`, so a model that needed `npm run render`
//! ran it as a *check*: the file appeared, and the job's gate recorded "npm run render passed" as
//! one of the project's checks. Three tools now share the shell, each for one thing:
//!
//! | Tool | For | Files the command changes | A check? |
//! |---|---|---|---|
//! | `run_command` | looking around | undone, and refused | no |
//! | `run` | making a file or changing the worktree with a command | **kept** | **no** |
//! | `run_checks` | the project's own checks | kept (they are the project's) | yes: a cycle, a `checks` artifact, the gate |
//!
//! `run` runs exactly as `run_command` does: the workspace's environment (the repository's
//! devcontainer when it has one), the login shell, a `cwd` inside the worktree, the same time limit
//! (`CHECK_TIMEOUT_SECS`) and output cap, the secrets of this process hidden from the child, and a
//! missing tool reported as a missing toolchain. It differs in what it is held to afterwards:
//!
//! * **The files are the command's to change.** What it wrote, edited or removed stays, ignored
//!   files (`dist/`, `node_modules/`) included. The result lists what `git status` shows.
//! * **Git is not.** The worktree is snapshotted before the command exactly as `run_command` does
//!   it (the git directory, the `.git` file, `HEAD`, the branch, the refs, the local configuration,
//!   the ignore rules), and a command after which any of that differs is undone **entirely**, its
//!   files included, and refused: a command that moved `HEAD` or rewrote `.git` did more than make
//!   a file, and what it made on the way cannot be told from what it broke. The model is told to use
//!   `commit_and_push` for commits.
//! * **It is never a check.** It spends no check cycle, emits no `checks` artifact and writes no
//!   check record to the run's notes, so the gate cannot see it. (It does move the notes' check
//!   epoch, so a failed check from before it is not reused: its files, ignored ones included, may be
//!   what the check reads.) The files it changes change the
//!   worktree's tree, so a check that passed before no longer covers the code: `commit_and_push`
//!   and `open_pull_request` bind a verdict to a tree, and a changed tree is unchecked until
//!   `run_checks` has run on it (the same rule as for `write_file`).
//!
//! A command that outlives its time limit is killed with its process group, and what it had
//! written by then stays.
//!
//! The coder declares the tool here and its logic is `adam_code_tools::make` (ADR 0047), over the
//! run's slots (`ToolEnv` as a `CommandWorkspace`): the guard is the coder's `SlotGuard` held to
//! `Guarding::Make`, and `ToolEnv::after_run` moves the check epoch and gives the published note.

use adam::prelude::*;

use super::{Outcome, ToolEnv};

/// Make something with a shell command in your worktree and keep what it changes: generate or
/// export a file (`npm run render`, `python chart.py`, `convert a.png b.jpg`), install a project's
/// dependencies, run a formatter, build an artifact. It runs in the workspace's environment (the
/// repository's own devcontainer when it has one, so its tools are there), with the same time limit
/// as the checks, and you get the exit code, the tail of the output and the files that changed.
/// Unlike run_command (for looking, it undoes changes) the files stay, and unlike run_checks it is
/// not a check: it costs no check cycle and the pull request gate never sees it. It cannot touch
/// git: a command that changes HEAD, the branch, a ref, .git or the git configuration is undone
/// completely and refused, so commit with commit_and_push. After it changes the files, run the
/// project's checks again before you commit. Show the person a file it made with share_file.
#[tool(label = "Make files with a command", icon = "execute", access = super::access::makes_files_with_a_command)]
pub async fn run(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// Shell command, run with `bash -lc` (`sh -lc` without bash) in the worktree, in the workspace's environment
    command: String,
    /// Optional sub-directory of the worktree to run in (relative, inside the worktree)
    cwd: Option<String>,
    /// The slot to run in: its name, or the repository's address. Leave out when the workspace has one.
    repo: Option<String>,
    /// Seconds before the command is killed; at most the deployment's limit, which is also the default. Example: {"command":"npm test","timeout":120}
    timeout: Option<i64>,
) -> Outcome {
    adam_code_tools::make::run(&*env, ctx, command, cwd, repo, timeout).await
}
