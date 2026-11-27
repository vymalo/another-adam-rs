# 0086: the ports, the suite and the deliberate failure

For people: [0086. A run signs its model requests with a certificate](../../../docs/decisions/0086-a-run-signs-its-model-requests-with-a-certificate.md),
which states the decision. This page holds the evidence: where each piece is, what the suite proves, the mutation
that made it red, and what the next slice must add.

Built at `opencode/anr-1308-run-certs` from `origin/main` `b4e0e324` on 2026-11-28. Paths are at that
commit; re-read them before a later slice.

## What is built, and where

| Piece | Where | What it promises |
|---|---|---|
| `CertificateRequest` | `crates/adam-core/src/turn/run_cert.rs` | the CSR as adam carries it: DER, 1 to 8192 bytes; `Debug` shows the length only; `BadCertificateRequest` is `Invalid` |
| `RunCertIssuer` | `crates/adam-core/src/turn/run_cert.rs` | `issue(&CertificateRequest) -> Result<CertificateChain, RunCertError>`; `kind()` for `Debug`; no driver type |
| `RunCertError` | `crates/adam-core/src/turn/run_cert.rs` | `Invalid` → `Invalid`, `Unavailable` → `Transient`, `Refused` → `Unauthenticated`, `Internal` → `Internal` |
| `RunKeys` | `crates/adam-core/src/turn/run_cert.rs` | `TurnKeys` for one run; no method, no blanket implementation |
| `KeySource` | `crates/adam-model/src/signed.rs` | closed `Process` (default) or `Run`; `SignedRequests::with_key_source` chooses |
| fake issuer | `crates/adam-core/src/turn/run_cert/testkit.rs` | a chain derived from the request, for tests and for the suite |
| suite | `crates/adam-core/src/turn/run_cert/testkit.rs` | `run_cert_issuer_conformance!`, three cases |
| suite on the fake | `crates/adam-core/tests/run_cert.rs` | the macro is exercised, not only documented |

**No driver type** in either trait: the request and the chain are `adam-core`'s own types, and the error is a
`BoxError` behind a class. `RunKeys` has no method, so a `TurnKeys` implementation needs no change; the marker
has no blanket impl, so a process key cannot be passed as `KeySource::Run`.

## The suite's cases

| Case | What it proves | Why it exists |
|---|---|---|
| `an_issued_chain_is_a_chain_adam_can_carry` | 1 to 4 certificates, each 1 to 2048 bytes | the chain becomes `x5c` in a signature header, which is bounded (`CertificateChain`) |
| `the_chain_is_named_by_its_leaf` | `kid == base64url(SHA-256(leaf))` | a verifier finds the leaf by the signature's `kid` (ADR 0068 slice T1); an adapter must not invent one |
| `two_requests_get_two_chains` | two CSRs get two leaves | the chain is bound to the request's key: an issuer that ignores the request would certify another key |

The suite does not test the error paths: an adapter's own tests do, because the suite has no way to make a CA
fail. The classes are pinned in `each_error_has_its_class` in `run_cert.rs`.

## The deliberate failure

`leaf_of` in `run_cert/testkit.rs` was changed to ignore the request and answer a fixed leaf
(`b"adam-testkit-leaf/v1:another-key"`), the shape of a chain for another key:

```
cargo test -p adam-core --test run_cert
test two_requests_get_two_chains ... FAILED
panicked at crates/adam-core/src/turn/run_cert/testkit.rs:
assertion `left != right` failed: two different certificate requests got the same leaf: the chain is not bound to the request's key
test result: FAILED. 2 passed; 1 failed
exit 101
```

Restored, the same command exits 0 with 3 passed. The other two cases stayed green under the mutation, which is
why the binding case exists: bounds and the `kid` rule cannot see a chain for the wrong key.

## What ran, and what did not

Run in the worktree with `CARGO_TARGET_DIR=/tmp/opencode/anr1308-target` (crate-scoped, no workspace build, no `heavy`):

| Command | Exit |
|---|---|
| `cargo fmt --all --check` | 0 |
| `cargo check -p adam-core -p adam-model -p adam-a2a --all-targets` | 0 |
| `cargo test -p adam-core -p adam-model -p adam-a2a` | 0 |
| `cargo clippy -p adam-core -p adam-model -p adam-a2a --all-targets -- -D warnings` | 0 |
| `npm --prefix tools/docs-check ci` | 0 |
| `node tools/docs-check/check-docs.mjs` | 0 (`19 people/agent page pairs`) |

Not run locally: the workspace clippy with the feature list, `cargo nextest run --workspace`, the workspace's
doc tests beyond these three crates, `cargo test -p adam --test ui`, the MSRV job, and CI. CI judges them.

## The next slice must prove

1. An issuer adapter: a CSR goes over HTTP with `RUN_CERT_ISSUER_URL`, a token from the file, and the trust
   anchor from `RUN_CERT_ISSUER_CA_FILE`; a refusal is `Refused`, an outage `Unavailable`.
2. The renewal clock: at two thirds of the leaf's life a **new key** and a **new CSR**, never the same key
   re-issued; the old key signs until the new one is in hand; a failed renewal leaves it signing and a request
   it cannot sign is not sent.
3. The wiring: one `RunKeys` per run, `KeySource::Run` chosen by the request path, and the process `TurnKeys`
   still signing the history.
4. The red case for each: a chain for another key, a renewal that reuses the key pair, an issuer that is down.
