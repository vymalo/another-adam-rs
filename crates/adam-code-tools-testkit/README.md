# adam-code-tools-testkit

The conformance cases every implementation of [`adam-code-tools`](../adam-code-tools/README.md)'s
`CodeWorkspace` must pass, and `DirWorkspace`, the reference implementation over named directories
([ADR 0047](../../docs/decisions/0047-adam-rs-can-be-embedded-in-an-application.md), decision 7).

## Where it sits

The **testkit** of the workspace port of the file and search tools. An implementation lists it as a
dev-dependency and runs the same cases: the coder's `ToolEnv` in `bin/adam-coder/tests/code_workspace.rs`,
`DirWorkspace` in this crate's `tests/dir.rs`. It turns on `adam-code-tools`'s `test-util` (for
`GitAt::plain`), so it is a library for tests, never a runtime dependency.

## API at a glance

| Item | What |
|---|---|
| `code_workspace_conformance!(harness)` | one `#[tokio::test]` per case, each over a `Harness` built by the expression (awaited inside the test when it is a future) |
| `Harness` | `workspace` (`DynCodeWorkspace`), `ctx` (a `ToolCtx` it answers for), `writable` (the name of a place a tool may write), `read_only` (`Option`: one it may only read; the case is skipped without), `secret` (a value its redactor holds), `alive` (what must live as long as the harness: a fixture, a temporary directory) |
| `cases::*` | the four cases as `async fn(&Harness)`: a writable place is given for a read and a write, over a directory that exists; an unknown name is an answer for the model (an error result), never a `ToolError`; a read-only place is given for a read and never for a write; the redactor scrubs the secret |
| `DirWorkspace` | `new(secrets)`, `place(name, dir)`, `read_only_place(name, dir)`: places found by name, or the only one; `git` discovered from the directory |
| `detached_ctx()` | a `ToolCtx` of no run, for a harness |

## Tests

`tests/dir.rs` runs the cases over `DirWorkspace` with one writable and one read-only place.
`cargo test -p adam-code-tools-testkit`; nothing needs a database.

## See also

[`adam-code-tools`](../adam-code-tools/README.md), [`adam-coder`](../../bin/adam-coder/README.md).
