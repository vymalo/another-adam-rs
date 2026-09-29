# adam-agent-fixture

Test fixture, not published. It is the smallest crate that uses the authoring layer's build step:
`build.rs` calls `adam_agent_fs::build("agent").emit()` and `src/lib.rs` is
`adam::include_agent!();`. Its agent directory is
[`adam-agent-fs`'s `tests/fixtures/valid`](../adam-agent-fs/tests/fixtures/valid), so the fixture
and the parser tests prove their claims on the same files.

`tests/embedded.rs` is the end-to-end proof of slice S5: the manifest embedded by the build script
equals the manifest that `Dir` reads from the same directory at run time (packages, digests and
resource bytes), and `verify()` accepts the generated digests.

`cargo test -p adam-agent-fixture`.

[`adam-assembly`](../adam-assembly/README.md) uses it too (a dev-dependency): its tests bind the embedded agent and
the same files read from the directory, and run the agent end to end on a `MockModel`.
