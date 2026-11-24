# adam-confine

The Unix backends of the `Confine` port of `adam-workspace`: the OS limits on a command that
[ADR 0065](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0065-resource-governance-admission-pools-and-os-limits.md)
decision 6 asks for. A command is stopped for its own overuse, and **adam is not**.

## What it does

| Backend | Enforces | Needs |
|---|---|---|
| `Rlimits` | `RLIMIT_FSIZE`, `RLIMIT_NOFILE`, `RLIMIT_CPU`, `RLIMIT_AS` (Linux only), `nice` 10, idle I/O priority (Linux), a process group of its own | any Unix, no rights |
| `SystemdScope` | the same minus `RLIMIT_AS`, plus one transient scope per command with `MemoryMax`, `CPUQuota` and `TasksMax` | a systemd user manager that delegates `memory`, `cpu` and `pids` |
| `best_effort()` | `SystemdScope` when the memory controller is delegated, else `Rlimits` | probes once, at start |

`describe()` says what is enforced: `SystemdScope::detect` makes a real scope and **reads the limits back
from its cgroup**, so it reports `Partial` naming `memory`, `cpu share` or `process count` when a distribution does
not delegate that controller. `Rlimits` is always `Partial`: it caps no CPU rate and no process count (**never
`RLIMIT_NPROC`**, which on Linux counts every process of the user), and `RLIMIT_AS` caps the address space, not
resident memory, so a runtime that reserves much address space (a JVM, Node) needs a higher limit there.

```rust
use adam_confine::best_effort;
use adam_workspace::{Confine, ProcessLimits};

let confine = best_effort();
let mut cmd = tokio::process::Command::new("sh");
cmd.arg("-c").arg("make");
let confined = confine.confine(&mut cmd, &ProcessLimits::new().with_memory(2 << 30))?;
// set stdio, spawn, wait ...
// `confined.finish(Ended::from(status)).await` says `Some(Stopped::Memory { .. })` when the OS stopped it.
# Ok::<(), adam_workspace::ConfineError>(())
```

* A command that passes its memory is killed by the kernel; the scope's `Result` is `oom-kill` and `finish` returns
  `Stopped::Memory`. A file over `RLIMIT_FSIZE` is `Stopped::FileSize` (the signal, or a shell's `128 + signal`).
  Neither is an error: the tool tells the model and the run goes on.
* `stop()` kills the whole scope, which a process that left its process group cannot escape. `finish()` resets the
  scope, so none piles up in the user manager.
* The scope's unit is `adam-cmd-<pid>-<n>` under the slice `adam-commands`.

## `unsafe`

Setting a limit in the child needs `Command::pre_exec`, which is `unsafe`, so the backends are in this small crate,
which does not forbid it. The only `unsafe` is in `src/child.rs`: a closure that makes system calls (`getrlimit`,
`setrlimit`, `setpriority`, `ioprio_set`) and nothing else. Every other crate keeps `forbid(unsafe_code)`.

## Not here

macOS (`DarwinBackground`, with a watchdog for memory) and Windows (`JobObject`) are a separate card; `Enforcement::Partial`
names what a backend leaves out, so they fit the port without changing it. The crate is `cfg(unix)`.

## Tests

`tests/conformance.rs` runs `confine_conformance!` over both backends, and the card's proof: the same 700 MB memory
hog finishes under `NoConfine` and does not under a 256 MB limit. The `scope` cases skip where no user manager makes a
scope; `ADAM_TEST_REQUIRE_SYSTEMD=1` (CI sets it) turns that into a failure and `ci_has_the_full_scope` requires all
three controllers. The process flood runs only against a backend that enforces everything.

```sh
cargo test -p adam-confine
# a machine where the limits are the point: inside a scope of its own
systemd-run --user --scope -p MemoryMax=4G -p TasksMax=4096 cargo test -p adam-confine -- --test-threads 1
```
