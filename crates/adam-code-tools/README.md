# adam-code-tools

The file and command tools of an adam-rs agent, as a library
([ADR 0047](../../docs/decisions/0047-adam-rs-can-be-embedded-in-an-application.md), decision 7). They move here
from [`adam-coder`](../../bin/adam-coder/README.md) in steps, behind a `CodeWorkspace` port that the coder
implements over its worktrees and a host can implement over a person's own directory (slice 7).

## Where it sits

Below the coder and above [`adam-workspace`](../adam-workspace/README.md), which it uses **without** its `github`
feature: the GitHub code host and the App's credentials stay the coder's. It depends on `adam-llm-agent` and
`adam-runtime`, not on the `adam` facade, so it does not bring `adam-assembly` and its HTTP client. Unix only for
now: the commands run through a login shell and kill a process group (Windows is slice 8).

**This step** holds what the tools are built on. The tools themselves (`read_file`, `edit_file`, `search_files`,
`run_command`, ...) are still the coder's, and `adam_coder::tools::{shell, sandbox, access}`, `adam_coder::redact` and
`adam_coder::tools::gitcli` re-export these modules, so the coder's API and its tests are unchanged.

## API at a glance

| Module | What |
|---|---|
| `shell` | Running a command for a tool: `run_in`, `run_timed`, `run_captured` (cwd confined to the worktree, a timeout that kills the whole process group, output capped to its tail), `shell_spec`, `resolve_cwd`, `cwd_key`, `spawn_prepared`, `kill_group`, `login_shell`; `ShellOutcome` (`passed()`, `repeat_key()`), `KeptNote`, `RunError`; a command the shell cannot find is `missing_tool` → `MissingTool`, with `project_dependency_hint`; `KEPT_OUTPUT_MAX`, `SCRUB_OVERLAP` |
| `sandbox` | `Meter`: the time a run's commands run in a metered sandbox (a run pod), reported as usage on the call's `ToolCtx` ([ADR 0043](../../docs/decisions/0043-sandbox-time-as-usage.md)); `Meter::of(session)` is `None` where nothing is metered |
| `access` | The scopes the tools share with the other calls of a model turn ([ADR 0049](../../docs/decisions/0049-the-tool-calls-of-a-turn-run-at-the-same-time.md)): `FILES`, `COMMANDS`, `LSP`, and the access functions of the tools with two uses (`runs_a_command`, `makes_files_with_a_command`, `stops_a_command`, `lists_the_commands`, `asks_a_language_server`) |
| `git` | Where the tools ask `git`: `GitAt` (`slot(&Slot)`, `worktree(&Worktree)`; pinned to the slot's repository as the workspace's metadata names it, and **refused** when it cannot tell, so no `git` is started), `git_stdout`, and the one question every file tool asks, `list_files` (tracked and untracked files, without what git ignores, sorted by bytes, at most `MAX_LISTED_PATHS` paths and `MAX_LISTED_BYTES` of answer) → `ListError` (`Unavailable`, `TooMany`, `Failed` with no path of this machine in it); `list_files_within` with other limits. The coder's git writes and restores stay in `adam-coder` and ask at the same `GitAt` |
| `redact` | `Redactor`: the process's secrets replaced by `[redacted]` (`REDACTED`) in everything that leaves it, with their Base64 forms; `new`, `add` (a secret learned later, the latest `MAX_ADDED` kept), `add_fixed`, `add_env_values`, `scrub`, `scrub_string`, `scrub_value`, `failure_text` (`MAX_FAILURE_TEXT`), `step_io`, and what a stream that scrubs in pieces needs (`longest`, `margin`, `partial_secret_len`); a clone shares its secrets, so one added later is scrubbed by every clone. Values shorter than `MIN_SECRET_LEN` are not registered. `Redactor::from_config` (the secrets of the coder's configuration) and `RedactingCredentials` stay in `adam-coder` |

## Features

| Feature | What |
|---|---|
| `test-util` | `GitAt::plain(dir)` (discover the repository from the directory), `GitAt::refused(dir)` and `GitAt::refused_at(root)`, for the tests of a crate that uses the tools; off by default. **Only ever a dev-dependency**: `plain` finds the repository through the `.git` a run's commands write, which the pin exists to prevent |

## Tests

Next to the code: `shell.rs` (the cwd confinement, the timeout that kills the group, the output cap, the
missing-tool recognition, a run in a session of the caller's own), `git.rs` (the listing: tracked and untracked merged and sorted by bytes, a `.git` of any case left out, a name that is not UTF-8, too many paths or bytes stops `git`, no path of this machine in what `git` said) and `redact.rs` (every position of a split secret,
the Base64 forms, clones that share what is added later, the one-pass scrub against the sequential one, a megabyte
against 650 needles). `cargo test -p adam-code-tools` (needs `git`); nothing needs a database.

## See also

[`adam-coder`](../../bin/adam-coder/README.md) (the tools that use these), [`adam-workspace`](../adam-workspace/README.md)
(the environment a command runs in), [ADR 0047](../../docs/decisions/0047-adam-rs-can-be-embedded-in-an-application.md).
