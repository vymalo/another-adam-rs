# adam-confine-testkit

The conformance suite of `adam_workspace::Confine`. `confine_conformance!(path::to::make)` writes four cases into a
module of the implementing crate; `make` returns the `DynConfine` to test.

| Case | Asked of |
|---|---|
| a confined command still runs, in its directory, with its variables, and ends unstopped | every backend |
| a file over `file_size` is stopped and `finish` says `Stopped::FileSize` | every backend that is not `Enforcement::None` |
| a memory hog is stopped before it finishes (and `finish`, if it names a reason, says `Stopped::Memory`) | a backend that does not list `memory` among what it misses |
| a flood of 300 processes is held to `max_processes` | `Enforcement::Full` only |

The hog and the flood only run against a backend that claims to cap them, so the suite never starts one on a machine
that nothing protects. `run(confine, limits, dir, script)` is public, for a test of one's own.

`adam-confine` runs it over `Rlimits` and `SystemdScope`.
