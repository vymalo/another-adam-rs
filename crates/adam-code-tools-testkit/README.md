# adam-code-tools-testkit

The conformance cases every implementation of [`adam-code-tools`](../adam-code-tools/README.md)'s
`CodeWorkspace` and `CommandWorkspace` must pass, and `DirWorkspace`, the reference implementation over named directories
([ADR 0047](../../docs/decisions/0047-adam-rs-can-be-embedded-in-an-application.md), decision 7).

## Where it sits

The **testkit** of the workspace ports of the file and search tools and of the commands. An implementation lists it as a
dev-dependency and runs the same cases: the coder's `ToolEnv` in `bin/adam-coder/tests/code_workspace.rs` (both sets),
`DirWorkspace` in this crate's `tests/dir.rs` and `tests/commands.rs`. It turns on `adam-code-tools`'s `test-util` (for
`GitAt::plain`), so it is a library for tests, never a runtime dependency.

## API at a glance

| Item | What |
|---|---|
| `code_workspace_conformance!(harness)` | one `#[tokio::test]` per case, each over a `Harness` built by the expression (awaited inside the test when it is a future); the consumer needs `tokio` with `macros` and `rt-multi-thread` as a dev-dependency |
| `Harness` | `workspace` (`DynCodeWorkspace`), `ctx` (a `ToolCtx` it answers for), `writable` (the name of **the only** place a tool may write, so also the one it works in with no name), `read_only` (`Option`: one it may only read; without one its case passes with nothing checked), `secret` (a value its redactor holds), `alive` (what must live as long as the harness: a fixture, a temporary directory) |
| `cases::*` | the six cases as `async fn(&Harness)`: the writable place is given for a read and a write, under its name, over a directory that exists, and its root is the directory its `git` is asked in; with no name, a tool works in the writable place; an unknown name is an answer for the model (an error result), never a `ToolError`; a read-only place is given for a read and never for a write; the redactor scrubs the secret (a failure says the secret's length, never the secret); a write is followed by the hook (`after_write` on the writable place twice: each `Ok`, its text empty or starting with a space or a line break, and never holding the secret) |
| `DirWorkspace` | `new(secrets)`, `place(name, dir)`, `read_only_place(name, dir)`: places found by name, or with none the only writable one (as the coder finds a slot); `git` discovered from the directory; nothing follows a write (`after_write` returns an empty text). As a `CommandWorkspace`: commands run in this process's own environment (`LocalSession`, reported as `local` usage), within `DEFAULT_LIMITS` (30 s, 16 KiB shown) or `with_limits(CommandLimits)`, and a long output is kept in memory (`adam_llm_agent::MemoryKept`, scrubbed by the workspace's redactor); a person's directory is never undone (ADR 0047, decision 7), so its guard is `Unwatched` and nothing follows a command (`after_run` returns an empty text) |
| `detached_ctx()` | a `ToolCtx` of no run, for a harness |
| `command_workspace_conformance!(harness)` | one `#[tokio::test]` per case of `command_cases`, each over a `CommandHarness` built by the expression, as `code_workspace_conformance!` |
| `CommandHarness` | `workspace` (`DynCommandWorkspace`), `ctx`, `writable` (a place a tool may write, where the commands run), `secret` (a value its redactor holds, with no `'`: the cases put it in a shell command), `alive` |
| `command_cases::*` | the thirteen cases as `async fn(&CommandHarness)`, each command run through `adam_code_tools::commands::run` with `shell::shell_spec` in the writable place: a command runs there (a file it writes appears in the place, which the case removes first); `session` reports the session to the call's usage, for an environment `Meter::of` names; a long output that holds the secret is shown as a head and a tail and kept by `output::settle`, neither holding the secret, and the kept text read back page by page from `kept()` has `[redacted]`, never the secret, and its last line; `timeout_of` refuses 0, a negative timeout and one over `limits().timeout`, and gives the limit when none is asked; a short output is shown whole, scrubbed (`short [redacted]`), and nothing is kept. **The guard**, taken on the writable place and settled after the command: a look that changes nothing is `Unchanged` (or `Unwatched`); a look that writes a file is never `Unchanged`, and when it was undone and restored the file is gone; a make that writes a file keeps it (`FilesChanged` or `Unwatched`); a make that changes nothing is `Unchanged` (or `Unwatched`); a make that adds a branch in a place with a `HEAD` is `Undone { restored: true }` and the branch is gone, or `Unwatched` (the case removes it; a place with no repository passes with nothing to check, and one whose `.git` git cannot read fails, so it never passes unchecked). `after_run` twice: `Ok`, empty or starting with a line break, no secret; `refusal_note`: empty or starting with a line break, no secret; `missing_tool_answer` for a command the shell cannot find: `None`, or a text that is not empty and holds no secret. They do not reach an output longer than what is captured, which reaches `kept()` unscrubbed: run `adam_llm_agent::kept_outputs_conformance` over the keeper too |

## Tests

`tests/dir.rs` runs the cases over `DirWorkspace` with one writable and one read-only place. `tests/write.rs` runs the write tools over a recording workspace: once per successful write the hook is called and its
text appended, a refused write or edit does not call it, a failing hook is the tool's error with the file still
written, and a read-only place a broken workspace gives for a write is refused with nothing written; `apply_patch`, in a git
repository the test makes, calls the hook once with the paths it changed, and neither a patch that does not apply nor a
read-only place calls it; a patch into `.git`, one that makes a symlink and a binary patch are refused before `git`
writes anything, with nothing written and no hook called. `tests/find.rs` runs `adam_code_tools::find`'s three tools through the port over `DirWorkspace` and a git
repository of its own (a listing, a glob, a search that neither shows nor finds the secret, an unknown place), so the
tools are tested over a workspace that is not the coder's. `tests/files.rs` does the same for `adam_code_tools::files`'s
`read_file` and `read_files` (a page of a file, a missing file named with its closest path, two files in one result, a
path that leaves the place by `..` or a symlink).
`tests/commands.rs` runs the command cases over `DirWorkspace`, with a 2 KiB view so that a long output is a few hundred lines.
`tests/run_command.rs` runs `adam_code_tools::inspect::run_command` over a scripted workspace (`DirWorkspace` with a guard
that answers as told): an unchanged or unwatched look is the result with no note and the progress line `running: ...`; files a
guard kept are said, with `changed files: ...`; an undone or unreadable look is refused with the note, restored or not, with
`undid a change made by: ...`; a refusal of the guard is the answer with the note and nothing runs; the refusals before the
guard (a blank command, a timeout, an unknown place, a `cwd` outside it) have no note and take no guard; a missing tool is
the workspace's answer with `the workspace lacks a tool`, or the command's result when it has none; the command is scrubbed
and a failed one carries its repeat key; and `after_run` is never called.
`tests/run.rs` does the same for `adam_code_tools::make::run`, the guard taken for a `Make`: files the guard saw change are
listed from `git status` (in a repository the test makes: as git lists them, the first entry without its leading space, a
known odd answer kept as it is; with no repository, nothing listed and said as ignored files) with the text of `after_run`
after them and `changed files: ...`; an unchanged run says git sees no change, and an unwatched one is the result alone,
both with `ran: ...`; a run that touched git (`undid a change to git made by: ...`) or after which git could not be read
(`could not check git after: ...; undid it`) is refused with the note, restored or not, and without the text of
`after_run`; a refusal of the guard is the answer with the note and nothing runs; the refusals before the guard (also a
place that may only be read) have no note, take no guard and call no `after_run`; for every run that ran, the calls are
the guard, `after_run` once, then `settle`, in that order; a failing `after_run` is the tool's error and the guard is not
settled (a known gap, kept); a missing tool is the workspace's answer or the result; the command is scrubbed and a failed
one carries its repeat key.
`cargo test -p adam-code-tools-testkit` (needs `git`); nothing needs a database.

## See also

[`adam-code-tools`](../adam-code-tools/README.md), [`adam-coder`](../../bin/adam-coder/README.md).
