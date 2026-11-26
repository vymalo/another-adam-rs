# The A2A server: evidence to check

For people: [The A2A server](../../../docs/reference/a2a-server.md), which says what a client sees. This page holds the
evidence behind its document section and the procedure a change runs.

## The committed document

`docs/api/a2a-server.openapi.json` is what `openapi::document(&card, flags, signed)` in
`crates/adam-a2a/src/openapi.rs` builds for one fixed card — `bearer: true`, `push: false`, `extended: false`,
`signed: false` — so the file moves only when the code does, never when a deployment's card changes. It is held equal to
the code by a **unit test** in that module (`mod tests`, `the_committed_document_is_the_one_the_code_builds`), not an
integration test: `document` is `pub(crate)` and an integration test cannot call it.

Rewrite the file with the repo's golden convention (`adam-assembly`, `adam-agent-fs` and `adam-plugin` do the same):

```sh
ADAM_UPDATE_GOLDEN=1 cargo test -p adam-a2a --lib the_committed_document_is_the_one_the_code_builds
```

The drift test's failure message names that command. Keys come out sorted because `serde_json`'s `Map` is a `BTreeMap`,
so the file is byte-stable and is reviewed by its diff, not by reading it.

**Proven:** with `info.version` set to `9.9.9`, the test failed with
`docs/api/a2a-server.openapi.json no longer matches crates/adam-a2a/src/openapi.rs; regenerate it with …`; regenerated
afterwards, byte-identical to the first write (vymalo/another-adam-rs#441, squash `f9595722`).
