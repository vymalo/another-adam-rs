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

It holds what the tools are built on and, so far, the tools that only read: `list_files`, `find_files` and
`search_files` (`find`), then `read_file` and `read_files` (`files`). The write tools (`write_file`, `edit_file`,
`apply_patch`) come next; `run_command` and the rest are still the coder's, and `adam_coder::tools::{shell, sandbox, access}`, `adam_coder::redact` and
`adam_coder::tools::gitcli` re-export these modules, so the coder's API and its tests are unchanged.

## API at a glance

| Module | What |
|---|---|
| `shell` | Running a command for a tool: `run_in`, `run_timed`, `run_captured` (cwd confined to the worktree, a timeout that kills the whole process group, output capped to its tail), `shell_spec`, `resolve_cwd`, `cwd_key`, `spawn_prepared`, `kill_group`, `login_shell`; `ShellOutcome` (`passed()`, `repeat_key()`), `KeptNote`, `RunError`; a command the shell cannot find is `missing_tool` → `MissingTool`, with `project_dependency_hint`; `KEPT_OUTPUT_MAX`, `SCRUB_OVERLAP` |
| `sandbox` | `Meter`: the time a run's commands run in a metered sandbox (a run pod), reported as usage on the call's `ToolCtx` ([ADR 0043](../../docs/decisions/0043-sandbox-time-as-usage.md)); `Meter::of(session)` is `None` where nothing is metered |
| `access` | The scopes the tools share with the other calls of a model turn ([ADR 0049](../../docs/decisions/0049-the-tool-calls-of-a-turn-run-at-the-same-time.md)): `FILES`, `COMMANDS`, `LSP`, and the access functions of the tools with two uses (`runs_a_command`, `makes_files_with_a_command`, `stops_a_command`, `lists_the_commands`, `asks_a_language_server`) |
| `confine` | Where a tool may read or write: `confine(root, path, Access)` → `Confined` (`rel()`; it cannot be built outside the crate), or the reason the model is told. An empty path, a NUL byte, an absolute path, `..`, any `.git` component (any case) and the root itself are refused; a read follows only the links that stay inside the place; a write refuses any link on the way. The file tools open with `open_for_read(root, path)` and `locate_for_write(root, path, create)` → `Located` (read-only accessors), which **check the path themselves** as `confine` does, against the root as it was opened (`SlotDir`), never its name; `parts_of` and `is_dot_git` are the rule's parts |
| `find` | **The first tools over the port**: `list_files`, `find_files` and `search_files` as functions of a `&dyn CodeWorkspace` and a `ToolCtx` (one place by `place(Read)`, its nested instructions before the page, its redactor over every line before it is matched). Their declarations (name, description, the model's arguments) are the caller's: the coder's `tools::find` declares them and delegates here. Bounded by what is done, sorted by bytes, a timeout an error and never a partial page (`within`, `DEADLINE` 30 s); `search_files_in` behind `test-util` |
| `files` | **The read tools over the port**: `read_file` (one page of a text file in the paging convention, at most 2000 lines and 50 KiB, `offset`/`limit` or the old `start_line`/`end_line`; an image offered to a model that sees the workspace's images; a missing file names the closest paths `git` lists) and `read_files` (the first page, 400 lines or 8 KiB, of 1 to 16 files in one result of at most 32 KiB, each file that cannot be shown one line; room for a block of nested instructions, `MAX_INSTRUCTIONS_BLOCK`, kept so that a claimed block is always shown), as functions of a `&dyn CodeWorkspace` and a `ToolCtx`, declared by the coder's `tools::files` as `find`'s are; `READ_FILES_MAX`, `READ_FILES_LINES`, `READ_FILES_PAGE_BYTES`, `READ_FILES_TOTAL_BYTES`; `read_in` behind `test-util`. `binary_notice` and `similarity` are public, hidden, for the coder's write tools (`write_file`, `edit_file`, `apply_patch`), which move here next |
| `git` | Where the tools ask `git`: `GitAt` (`slot(&Slot)`, `worktree(&Worktree)`; pinned to the slot's repository as the workspace's metadata names it, and **refused** when it cannot tell, so no `git` is started), `git_stdout`, and the one question every file tool asks, `list_files` (tracked and untracked files, without what git ignores, sorted by bytes, at most `MAX_LISTED_PATHS` paths and `MAX_LISTED_BYTES` of answer) → `ListError` (`Unavailable`, `TooMany`, `Failed` with no path of this machine in it); `list_files_within` with other limits. The coder's git writes and restores stay in `adam-coder` and ask at the same `GitAt` |
| `workspace` | **The port**, `CodeWorkspace` (`DynCodeWorkspace`): `place(ctx, repo, Intent)` → `Place`: the place `repo` names, or with none **the only place that may be written** (a read-only place is used when named, never by default); for `Intent::Write` never a read-only one. `Err` is the tool's answer, an error result for the model or a `ToolError` (`Outcome`). `redactor()`. `nested_instructions(ctx, place, path, Target)`, provided (`None` by default): a block returned is **claimed** (counted as shown, so the tool shows it) and at most `MAX_INSTRUCTIONS_BLOCK` (12 KiB). `Place::new(name, GitAt)` takes its root from the `GitAt`, so the directory a path is checked against is the one `git` writes in; `read_only()`, `with_handle(T)` and `handle::<T>()` (exactly a `T`, for the implementation's own methods), `name()`, `root()`, `path()`, `git()`, `is_writable()`. `Intent` (`Read`, `Write`), `Target` (`File`, `Dir`, `Either`). The coder implements it over the slots of a run (`ToolEnv`); the conformance cases are [`adam-code-tools-testkit`](../adam-code-tools-testkit/README.md). **To come:** a required hook after a write, with the file tools (a stated break for this repository's implementations); the commands' trait, which extends this one; and, with a host's implementation (slice 7), a `GitAt` for a directory no workspace metadata describes |
| `redact` | `Redactor`: the process's secrets replaced by `[redacted]` (`REDACTED`) in everything that leaves it, with their Base64 forms; `new`, `add` (a secret learned later, the latest `MAX_ADDED` kept), `add_fixed`, `add_env_values`, `scrub`, `scrub_string`, `scrub_value`, `failure_text` (`MAX_FAILURE_TEXT`), `step_io`, and what a stream that scrubs in pieces needs (`longest`, `margin`, `partial_secret_len`); a clone shares its secrets, so one added later is scrubbed by every clone. Values shorter than `MIN_SECRET_LEN` are not registered. `Redactor::from_config` (the secrets of the coder's configuration) and `RedactingCredentials` stay in `adam-coder` |

## Features

| Feature | What |
|---|---|
| `test-util` | `find::search_files_in`, `files::read_in`, `GitAt::plain(dir)` (discover the repository from the directory), `GitAt::refused(dir)` and `GitAt::refused_at(root)`, for the tests of a crate that uses the tools; off by default. **Only ever a dev-dependency**: `plain` finds the repository through the `.git` a run's commands write, which the pin exists to prevent |

## Tests

Next to the code: `shell.rs` (the cwd confinement, the timeout that kills the group, the output cap, the
missing-tool recognition, a run in a session of the caller's own), `find.rs` (the three tools' pages, limits, order, escapes and what a search cannot tell of a secret: the coder's tests, moved), `files.rs` (a read's pages, its one pass over a file, the old range names, a long line cut, a binary file or a directory not read, a read out of time, the closest paths, and paging that reassembles the file, a property test: the coder's tests, moved), `confine.rs` (the rules on both uses, a link that stays inside or leaves, a write through a link and a new file; the coder's file tools test the rest, with races, until they move), `git.rs` (the listing: tracked and untracked merged and sorted by bytes, a `.git` of any case left out, a name that is not UTF-8, too many paths or bytes stops `git`, no path of this machine in what `git` said) and `redact.rs` (every position of a split secret,
the Base64 forms, clones that share what is added later, the one-pass scrub against the sequential one, a megabyte
against 650 needles). `cargo test -p adam-code-tools` (needs `git`); nothing needs a database.

## See also

[`adam-code-tools-testkit`](../adam-code-tools-testkit/README.md) (the conformance cases of `CodeWorkspace`),

[`adam-coder`](../../bin/adam-coder/README.md) (the tools that use these), [`adam-workspace`](../adam-workspace/README.md)
(the environment a command runs in), [ADR 0047](../../docs/decisions/0047-adam-rs-can-be-embedded-in-an-application.md).
