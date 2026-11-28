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

## A3: the broker adapter and the config (2026-11-28)

Built at `opencode/anr-1310-broker-adapter` from `origin/main` `7fd99b9b` on 2026-11-28. Paths are at that
commit; re-read them before a later slice.

| Piece | Where | What it promises |
|---|---|---|
| `BrokerIssuer` | `crates/adam-run-cert-broker/src/lib.rs` | the `RunCertIssuer` of **one run** over the broker's contract v1: `POST {url}/internal/runs/{run}/certificate`, `{"v":1,"csr":"<base64 DER>"}` in, `{"v":1,"chain":[...]}` out, the projected token from the file **re-read on every call**, the anchors of `RUN_CERT_CA_FILE` beside the system roots |
| `http_client` | same | one client a host builds once: no redirect, 30 s, `tls_certs_merge` for the anchors; a bad PEM is `BrokerError::CaFile` (`Invalid`) at startup |
| `BrokerError`, `TokenError`, `AnswerError` | same | the URL/anchors (`Invalid`), the token (`Unauthenticated`; it carries the path, never the contents), the answer (version, size, JSON, base64, chain, status) |
| `RunCertSettings`, `RunCertIssuerKind` | `crates/adam-service/src/config.rs` | `RUN_CERT_*`: off when all unset, a problem naming the variable when half-set, `broker` built and `openbao` refused until A6, the fraction parsed as `n/d`; `issuer(run)` and `keyring(subject)` |
| `RenewFraction`, `BadRenewFraction` | `crates/adam-core/src/turn/run_cert/keyring.rs` | the renewal point as an exact fraction (default `TWO_THIRDS`), `RunKeyring::with_renew_fraction` |

**Why a new crate.** `adam-core` is the port and must stay free of an HTTP client and a TLS stack:
`tools/embedded-deps/check.sh` forbids `reqwest`, `rustls`, `aws-lc-rs` and `ring` in `adam-engine`'s tree, and
the A2 notes record that `x509-cert` was chosen so no crypto provider enters `adam-core`. The adapter is a crate
of its own (`adam-run-cert-broker`) on the `reqwest`/rustls-on-aws-lc-rs already in the tree — the model client's
stack, not a second one — and `adam-service` depends on it only for `RunCertSettings`. The port is unchanged: no
driver type in a trait signature.

**The token.** `issue` opens `RUN_CERT_TOKEN_FILE` every time, trims the trailing whitespace of a mounted secret,
and refuses an empty or unreadable file without sending. The header is marked sensitive; the token is never in
`Debug` (the issuer shows the endpoint's origin, the run and the file's path), never in a log line, and never in
an error. The sentinel test captures our target's log and asserts the sentinel is absent while the mock asserts it
was presented.

**The refusals.** `400` → `Invalid`, `401`/`403` → `Refused` (`Unauthenticated`), `429`/`5xx` → `Unavailable`
(`Transient`), another status → `Internal`. The answer's problem document is never echoed: an error carries the
status, not the body. A wrong `v` is `Internal` (`AnswerError::Version`).

**The contract fixtures.** ANR-1306's v1 fixtures (the system's `docs/api/fixtures/run-certificate-v1/`) are
copied into `crates/adam-run-cert-broker/tests/fixtures/run-certificate-v1/` — the JSON bodies only, no README —
so the request and the answer cross the wire byte for byte and the six refusals are tested for their class. The
fixed answer cannot pass the conformance suite's `two_requests_get_two_chains`, so that mock mints a leaf per CSR
with `FakeRunCertIssuer`; the fixed answer is the shape test.

**A contract gap.** v1 allows up to 8 certificates; `CertificateChain::MAX_CERTIFICATES` is 4. A longer chain is
`Internal`; real chains are 2 or 3. Raising the port's bound is a later change, not a silent truncation.

### The deliberate failures (each seen red once)

1. **Half-set config.** The required-variable loop no longer pushed its problem: `RUN_CERT_ISSUER is not named: []`,
   exit 101 (`a_half_set_run_certificate_names_the_variable_and_is_78`). The process would have started with run
   certificates silently off.
2. **Token sentinel.** `issue` logged `tracing::debug!(token = %token.expose_secret(), …)`: `a rendering leaks the
   token: 2026-11-28T00:10:44Z DEBUG adam_run_cert_broker: RED 2: the broker presents its token
   token=anr1310-projected-token-SENTINEL-do-not-log`, exit 101.
3. **The `v` check.** `answer.v != CONTRACT_VERSION` became `answer.v == 0`: the case panicked with
   "called `Result::unwrap_err()` on an `Ok` value: CertificateChain { kid: …, certificates: 2 }"
   (`a_wrong_version_is_refused`), exit 101.

Restored, the three crates' suites are green except the pre-existing `hooks::tests::an_unknown_member_is_refused`
(below).

### What ran, and what did not

Crate-scoped with `CARGO_TARGET_DIR=/run/media/selast/ai-ide-v2/claude/opencode/anr1310-target` and
`CARGO_BUILD_BUILD_DIR=/run/media/selast/ai-ide-v2/claude/opencode/anr1310-build`, no `heavy`, no workspace build:

| Command | Exit |
|---|---|
| `cargo fmt --all --check` | 0 |
| `cargo check -p adam-core -p adam-service -p adam-run-cert-broker --all-targets` | 0 |
| `cargo check -p adam-agent --all-targets` | 0 (its `ModelConfig` literal gains `run_cert: None`) |
| `cargo test -p adam-core -p adam-run-cert-broker -p adam-service` | red on `hooks::tests::an_unknown_member_is_refused`, pre-existing |
| `cargo test -p adam-core -p adam-run-cert-broker -p adam-service -- --skip hooks::tests::an_unknown_member_is_refused` | 0 |
| `cargo clippy -p adam-core -p adam-service -p adam-run-cert-broker --all-targets -- -D warnings` | 0 |
| `cargo metadata --locked` | 0 |
| `node tools/docs-check/check-docs.mjs` | 0 |

**The pre-existing red.** `an_unknown_member_is_refused` writes a hooks file with `"alow"` and expects the problem
to name it; `parse_file` in `crates/adam-service/src/hooks.rs` formats only "a member is wrong at line … column …".
The same code and test are at `7fd99b9b` (`git show 7fd99b9b:crates/adam-service/src/hooks.rs`), so it is not A3's;
it is a separate ticket.

Not run locally: the workspace clippy with the feature list, `cargo nextest run --workspace`, `cargo test -p adam
--test ui`, `tools/embedded-deps/check.sh` (it builds `adam-engine`'s tree), the MSRV job, and CI. CI judges them.
`cargo tree -p adam-core -i ring` and `-i aws-lc-rs` are empty (*checked 2026-11-28*); the broker crate brings
`reqwest` with rustls on aws-lc-rs, which is the model client's stack.

### What the next slice must add

1. The wiring (R3): `ModelConfig::run_cert` → one `RunKeyring` per run, `KeySource::Run` on the request path, and
   the person the SAN names taken from the run's prompt statement.
2. A keyring-level test of the whole path over the broker adapter (the conformance suite proves the adapter; R3
   proves the request path chooses it).

## A4: the sidecar (2026-11-28)

Built at `opencode/anr-1311-run-signer` from `origin/main` `06d6e7c5` on 2026-11-28. Paths are at that
commit; re-read them before a later slice.

| Piece | Where | What it promises |
|---|---|---|
| `Signer`, `run`, `serve` | `bin/run-signer/src/proxy.rs` | one fallback route for every method and path; the order run → statement → target → body → keyring → signature → gateway, so a request refused early is never signed and never forwarded; `serve` bounds the **drain** only (a shutdown starts a grace timer), so a 50 ms grace does not stop a server that has run for 200 ms |
| `SignerConfig`, `Limits` | `bin/run-signer/src/config.rs` | `RUN_SIGNER_*`: loopback-only listen, an upstream URL with no query, fragment or credentials, and the client — no proxy from the environment, no redirect, the connect and read timeouts, the merged anchors; `Debug` shows the scheme, host, port and a CA name, never a path, a credential or a certificate byte |
| `Broker`, `from_process_env` | `bin/run-signer/src/broker.rs` | both halves from one environment, **every** problem in one `ConfigError`; one broker issuer per run from `RunCertSettings`, carrying its `RUN_CERT_RENEW_FRACTION` |
| `IssuerFactory` | `bin/run-signer/src/issuer.rs` | one issuer per run; `renew_fraction()` defaulted to two thirds; the `Arc` blanket impl forwards it |
| `Keyrings` | `bin/run-signer/src/keyrings.rs` | one `RunKeyring` per run, with the factory's fraction; a run keeps its person; the registry is LRU-bounded, so the hot run is the last to go |
| `statement::person_and_jti` | `bin/run-signer/src/statement.rs` | the person and the prompt id read through `adam_model::PromptStatement`, the same form check the agent applies (a compact JWS, three base64url segments with a non-empty header and signature, at most 8 KiB); its own JWS parser is gone |
| `PromptStatement::person` | `crates/adam-model/src/prompt.rs` | the statement payload's `sub` as a non-empty string, `NoPerson` (documented, no content quoted) when there is no statement or no `sub` |
| `main` | `bin/run-signer/src/main.rs` | tracing, `from_process_env()`, `run()`, `fail()`: 0 a clean shutdown, 78 the configuration, 71 the port |

### What the tests prove

| Acceptance | Test |
|---|---|
| the signature verifies against the leaf the sidecar really signed with, over the exact bytes the gateway received | `a_signature_it_adds_verifies_against_the_runs_leaf` (`tests/proxy.rs`) |
| no `X-Run-Id` means no signature and nothing forwarded, and the 400 body holds neither the run id nor the statement | `a_request_without_a_run_id_is_refused_and_nothing_is_forwarded` |
| a target `Url::parse` would rewrite (`..`, `%2e%2e`, `\`) or that is not `/`-rooted (`OPTIONS *`) is 400, and nothing is forwarded; a normal target with a query is forwarded unchanged | `a_target_the_url_would_rewrite_is_refused_and_nothing_is_forwarded`, `a_normal_target_is_forwarded_unchanged` |
| the process exits 78 for an empty environment, naming both halves | `an_empty_environment_exits_78_naming_both_halves` (`tests/binary.rs`) |
| the process exits 78 for a non-loopback bind and 71 for a taken port | `a_non_loopback_listen_exits_78`, `a_port_that_cannot_be_bound_exits_71` |
| the token file's content reaches no line of stdout or stderr, and SIGTERM is exit 0 | `the_token_is_on_no_line` |
| a `503` carries `type: unavailable`, a `400` `about:blank` | `a_503_is_typed_unavailable_and_a_400_is_not` |
| a run or a statement named twice is 400 and nothing is forwarded; the validated headers go out once each and only the first envelope | `a_request_that_names_its_run_twice_is_refused`, `…_its_statement_twice_…`, `the_validated_headers_go_out_once` |
| a renewal between the key and the signature is retried once, and a surviving conflict is 503 | `a_renewal_between_the_key_and_the_signature_is_retried_once`, `a_surviving_conflict_is_a_503` |
| `RUN_CERT_RENEW_FRACTION` reaches the keyring | `a_factorys_renew_fraction_reaches_its_keyring` |
| at the cap, a hit keeps its run | `a_hit_makes_the_run_the_most_recently_used` |
| a proxy in the environment is not used for a signed request | `a_proxy_in_the_environment_is_not_used` (`tests/proxy_env.rs`) |
| the drain, not the serving, is bounded by the grace; a request in flight finishes | `the_shutdown_grace_bounds_the_drain_not_the_serving`, `a_request_in_flight_finishes_inside_the_grace` |

### What the tests do not prove

* **The real broker and the real gateway.** The sidecar's issuer is a fake CA in every test here; the
  broker's HTTP round trip, its token and its refusals are `adam-run-cert-broker`'s (A3), and no
  gateway verifies a signature this sidecar made. A signature the gateway rejects would still be a
  200 from this process.
* **The deployment.** No chart value, no projected token, no anchor volume, no pod: nothing here
  proves the sidecar runs beside an agent, that `MODEL_BASE_URL`/`MODEL_AUTH=injector` point at it,
  or that loopback-only holds in a namespace.
* **The in-process path.** A worker choosing `KeySource::Run` per run (R3) is not built; the sidecar
  is a separate process in front of the model.
* **`sub` is the person.** Nothing verifies the statement: the first request for a run fixes its
  person (see the sidecar's README), and a test can only show the sidecar believes what it is told.
* **A real clock.** Expiry and renewal at a real leaf's dates are the keyring's tests, not these.

### What ran, and what did not

Run in the worktree under `heavy` (`CARGO_TARGET_DIR=/run/media/selast/ai-ide-v2/claude/target-anr-1311`,
`CARGO_BUILD_BUILD_DIR=/run/media/selast/ai-ide-v2/claude/cargo-build-anr-1311`, `CARGO_BUILD_JOBS=4`):

| Command | Exit |
|---|---|
| `cargo metadata` (once, to write the lock for the new dev-dependency), then `cargo metadata --locked` | 0 |
| `cargo fmt --all` | 0 |
| `cargo clippy -p adam-run-signer --all-targets -- -D warnings` | 0 |
| `cargo test -p adam-run-signer` | 0 (41 lib, 10 `config`, 14 `proxy`, 4 `binary`, 1 `proxy_env`) |
| `node tools/docs-check/check-docs.mjs` | 0 (`docs OK`, 19 people/agent page pairs) |

**Not run locally:** the workspace clippy with the feature list, `cargo nextest run --workspace`, the
workspace's doc tests beyond this crate, `cargo test -p adam --test ui`, the MSRV job, CI, and — the
one that matters most for A4 — the image build and `docker/coder/test/static-smoke.sh`, which is what
would prove `run-signer` is one of the seven binaries the image starts with no configuration. CI
judges them.


### The failures seen first (first runs of the new tests, not mutations)

Unlike A1 to A3, no mutation was run for A4: the failures recorded below are first runs of new tests
against the code as it was, not a hand-made mutation. Each was re-run green after the change.

| Case | The red seen first, with the pre-fix code |
|---|---|
| R1, the grace | `a request 200 ms after start against a 50 ms grace` panicked with `reqwest::Error { kind: Request, …, ConnectError("tcp connect error", 127.0.0.1:…, Os { code: 111, kind: ConnectionRefused, message: "Connection refused" }) }`: the whole server sat in a timeout counted from startup, so it stopped serving and exited 0 fifty milliseconds in |
| R2, the target | `a_target_the_url_would_rewrite_is_refused_and_nothing_is_forwarded`: `assertion left == right failed: /v1/../admin/keys: 0`, `left: 404`, `right: 400` — the rewritten path was forwarded to the gateway, which answered 404 for a route it has no mock on |
| R5, the type | `a_503_is_typed_unavailable_and_a_400_is_not`: `left: String("about:blank")`, `right: "unavailable"`, over `{"type":"about:blank","title":"Service Unavailable","status":503,…}` — a refusal `adam-model` reads as having no type, so the run was never parked |
| R7, the headers | `a_request_that_names_its_run_twice_is_refused`: `left: 404`, `right: 400` — two `X-Run-Id` headers were accepted, the first read and forwarded, the request signed and sent |
| R13, the process | `an_empty_environment_exits_78_naming_both_halves`: `RUN_SIGNER_UPSTREAM is named by none: reading the configuration: invalid configuration:\n  - run certificates are required: …` — the certificate half was parsed first, so the signer's half was never read |

### What the next slice must add

1. The in-process path (R3): a worker chooses `KeySource::Run` per run on the request path, with the person
   the SAN names taken from the run's prompt statement. The sidecar is the other way to the same `x5c`.
2. The deployment that runs the sidecar: the chart's projected token and anchor, `MODEL_BASE_URL` and
   `MODEL_AUTH=injector` pointed at it, and the image's smoke test proving it starts with no configuration.
