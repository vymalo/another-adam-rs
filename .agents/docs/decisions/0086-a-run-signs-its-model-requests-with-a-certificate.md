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

## R2a: the keyring (2026-11-28)

Built at `opencode/anr-1309-run-keyring` from `origin/main` `274186ab` (A1). Paths are at that commit; re-read
them before a later slice.

| Piece | Where | What it promises |
|---|---|---|
| `RunKeyring` | `crates/adam-core/src/turn/run_cert/keyring.rs` | `TurnKeys` + `RunKeys` over any `DynRunCertIssuer`: one Ed25519 key per run made from `getrandom` with the seed wiped, a CSR, the chain cached, renewal at two thirds, fail closed on expiry |
| `RunSubject` | same | the SAN's two halves: a person of 1–128 URI unreserved characters and a `RunId`; `san()` is `urn:another:person:<sub>:agent:<run>`; `BadRunSubject` is `Invalid` |
| `RunClock`, `SystemClock` | same | the instant the renewal and the expiry read; `testkit::TestClock` sets it by hand |
| `RunKeyError` | same | `Expired` and `NoRandomness` → `Transient`; `Renewal(RunCertError)` → the port's class; `Request`/`Chain` → `Internal`; `fail` maps `Unauthenticated` to `TurnKeyError::Refused` and the rest to `Unavailable` |
| `x509` (private) | `crates/adam-core/src/turn/run_cert/x509.rs` | PKCS#10 out (`RequestBuilder`-free: `CertReqInfo` + `setOfVec<Attribute>` with an `extensionRequest`), validity and SPKI in, a fake CA's leaf |
| `FakeRunCertIssuer` | `crates/adam-core/src/turn/run_cert/testkit.rs` | now a **fake CA**: parses the request's DER (a non-CSR is `Invalid`), mints a real X.509 leaf for that key with a serial from the request's hash and a window from its clock, answers leaf + its own certificate |
| `TestClock` | same | a clock a test sets and advances |

**The renewal, concretely.** `live()` holds the mutex across the issuer's answer, so two model calls never make
two keys. `now >= renew_at` (where `renew_at = notBefore + 2/3 × (notAfter − notBefore)`) makes a new
`SigningKey`, a new CSR and a new chain in one step: after the swap only the new `kid` signs (`sign` answers
`NotSigning` for the old one), so the run never signs with two keys at once and a leaked key stops working at the
renewal. A failed renewal keeps the old `Live` in place, sets `retry_at = now + RENEW_RETRY_SECS` (15 s) and
returns it with `KeyStatus::Renewing`; calls inside that window return it without asking the CA again. Past
`not_after` the retry window is ignored and a failed renewal is an error, so no call is signed with an expired
leaf.

**The chain is checked before it is cached**: the leaf's `subject_public_key_info` must be the key just made
(`x509::certifies`), its window must be a window, and it must not already be expired when it arrives. Anything
else is `RunKeyError::Chain` (`Internal`).

### The CSR crate

`x509-cert` 0.3.1 (RustCrypto formats, published 2026-11-20, *verified 2026-11-28* on
<https://docs.rs/x509-cert/0.3.1/> and the crate's Cargo metadata): it builds `CertReq`/`CertReqInfo` and reads
`Certificate`/`Validity`, and it signs through `signature` 3's traits, which `ed25519-dalek` 3.0.0 already
implements — the workspace's Ed25519, so no second crypto provider. `rcgen` was rejected because every version
needs `ring` or `aws-lc-rs` for its own hashing, and `tools/embedded-deps/check.sh` forbids both in
`adam-engine`'s tree. Enabling `ed25519-dalek`'s `pkcs8` and `alloc` features is what makes
`VerifyingKey: EncodePublicKey` and `Signature: SignatureBitStringEncoding` hold. Licences (Apache-2.0 OR MIT)
are already in `deny.toml`'s allow list.

### The tests

`crates/adam-core/src/turn/run_cert/keyring.rs` (unit tests, module scope so they can build a `Live` from a
known seed):

| Test | What it proves |
|---|---|
| `a_run_key_signs_and_its_leaf_is_its_own` | the leaf's SPKI is the key set's key, the signature verifies, another kid is `NotSigning` |
| `the_csr_names_the_person_and_the_run_with_a_critical_san` | the `extensionRequest` attribute holds one **critical** `subjectAltName`, URI `urn:another:person:st-abc123:agent:<run>` |
| `nothing_renews_before_two_thirds_and_a_new_key_comes_at_two_thirds` | at 2 h − 1 s the same kid; at 2 h (of a 3 h leaf) a new kid and a new public key, the old kid `NotSigning` |
| `a_failed_renewal_keeps_the_old_leaf_signing` | a down CA at two thirds: the old kid signs with `Renewing`; after `RENEW_RETRY_SECS` the new key arrives |
| `an_expired_leaf_with_no_renewal_refuses_with_a_class` | past `notAfter` with a down CA: `Unavailable`/`Transient` whose source is `RunKeyError::Renewal`, and `sign` refuses too |
| `a_chain_for_another_key_is_refused` | a CA that certifies another key is `RunKeyError::Chain` |
| `no_key_a_csr_or_a_chain_is_rendered` | a seed spelling `SENTINEL` appears in no `Debug`/`Display` of the `Live`, the chain, the key, the CSR, the keyring or the errors, in text, base64url or hex |
| `a_subject_is_a_uri_component_or_it_is_refused`, `each_error_has_its_class` | the SAN cannot be bent; the classes are pinned |

### The deliberate failures (each seen red once)

1. **Renewal.** `live()`'s `Some(live) => now >= live.renew_at` became `Some(_) => false`:
   `assertion `left != right` failed: the key was not renewed at two thirds` (same kid on both sides), exit 101.
2. **Fail closed.** The expired-renewal arm returned the held leaf instead of the error:
   `called `Result::unwrap_err()` on an `Ok` value: CurrentKey { …, status: Renewing }`, exit 101.
3. **Sentinel.** `Live`'s `Debug` printed `URL_SAFE_NO_PAD.encode(self.key.to_bytes())`:
   `a rendering leaks the key material: Live { kid: …, key: "YW5yMTMwOS1rZXktbWF0ZXJpYWwtU0VOVElORUwhISE", … }`,
   exit 101.

Restored, `cargo test -p adam-core` exits 0 (49 + 5 + 3 + 31 + 1 tests).

### What ran, and what did not

Crate-scoped with `CARGO_TARGET_DIR=/run/media/selast/ai-ide-v2/claude/opencode/anr1309-target` (`/tmp`'s tmpfs
was full: `Disk quota exceeded`), no `heavy`, no workspace build:

| Command | Exit |
|---|---|
| `cargo fmt --all --check` | 0 |
| `cargo check -p adam-core --all-targets` | 0 |
| `cargo test -p adam-core` | 0 |
| `cargo clippy -p adam-core --all-targets -- -D warnings` | 0 |
| `cargo metadata --locked` | 0 (`Cargo.lock` changed: five new packages, `x509-cert` 0.3.1, `der` 0.8.3, `der_derive` 0.8.0, `pkcs8` 0.11.0 and `spki` 0.8.1) |
| `node tools/docs-check/check-docs.mjs` | 0 |

Not run locally: `tools/embedded-deps/check.sh` (it builds `adam-engine`'s tree, not crate-scoped), the
workspace clippy with the feature list, `cargo nextest run --workspace`, `cargo test -p adam --test ui`, the
MSRV job, and CI. CI judges them. `cargo tree -p adam-core -i ring`/`-i aws-lc-rs` is empty (*checked
2026-11-28*), which is what the embedded check would assert.

### What the next slice must add

1. The HTTP adapter (R2b): the CSR over `RUN_CERT_ISSUER_URL`, the token from `RUN_CERT_ISSUER_TOKEN_FILE`,
   the anchor from `RUN_CERT_ISSUER_CA_FILE`, a refusal `Refused`, an outage `Unavailable`.
2. The wiring (R3): one `RunKeyring` per run, `KeySource::Run` chosen by the request path, the process
   `TurnKeys` still signing the history, and the person the SAN names taken from the run's prompt statement.
3. A backoff that survives a restart, if the deployment needs one: the retry window is in memory, so a
   restarted process asks the CA at once.
