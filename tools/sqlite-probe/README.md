# sqlite-probe

A CI probe for [ADR 0053](../../docs/decisions/0053-sqlite-a-store-service-a-database-per-agent-and-a-static-coder.md)
decision 2: `adam-store-sqlite` and CrateStack's embedded SQLite runtime (`cratestack-sqlite` →
`cratestack-rusqlite`) must resolve to **one** `rusqlite` and **one** `libsqlite3-sys`, so an
application that keeps its own state on cratestack links one `sqlite3`.

The probe is a workspace of its own (`[workspace]` in its `Cargo.toml`) and pins the resolution in
its `Cargo.lock`. Its test runs `cargo tree --features probe` and asserts one of each, and that both
sides are present (so a lost dependency cannot pass vacuously). Neither dependency is built.

```sh
cargo test --manifest-path tools/sqlite-probe/Cargo.toml --locked
```

CI runs it as the `sqlite-probe` job, part of the required `lint` check. A bump of `rusqlite` in
the main workspace that leaves cratestack behind fails here, which is the point: the two must move
together.
