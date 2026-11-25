# adam-core

The seam every durable-state backend implements: the `Store` trait, the run
and journal types, and an in-memory reference store. Also the turn key port
`TurnKeys` and the turn signature of [ADR 0068](../../docs/decisions/0068-adam-signs-its-own-history.md)
(slice T1), and RFC 8785 canonical JSON (`jcs`).

## Where it sits

Ports and adapters: this crate is the **port** for durable state. Adapters
are [`adam-store-postgres`](../adam-store-postgres/README.md) and
[`adam-store-mongodb`](../adam-store-mongodb/README.md); the reference adapter
`MemoryStore` lives here. Every adapter is checked by the shared suite in
[`adam-store-testkit`](../adam-store-testkit/README.md). It depends on no
database driver.

## API at a glance

| Item | What |
|---|---|
| `Store` (trait) | `migrate`, `create_run`, `load_run`, `load_head` (a run as an observer reads it: the head when it is current, else the state; defaults to `load_run`), `run_header` (a run without its state, from one projected read where the adapter can), `commit_run` (compare-and-swap on `version`), `open_run_for_conversation`, `journal_get`/`journal_put`/`journal_list`, `claim_due` (with a `ClaimScope` and the runs the caller is `busy` with), `renew_lease`, `release_lease`, `release_worker_leases` (every run lease a worker holds, so a restarted process claims its runs at once; defaults to a no-op), `lease_until` (when a run's lease ends, if it has one), `activity(agents, now)` (`StoreActivity`: how many open runs hold a live lease, and the earliest due time of an open run nobody holds; no run content; defaults to an `Invalid` error, so it breaks no implementer; `activity_conformance!` proves it), `purge_finished` (also deletes the push configs, body rows and blob references of the runs it purges, and collects unreferenced blobs), `list_runs` and `count_runs` (a caller's runs, newest update first, by keyset: `RunQuery`, `ConversationScope`), the five **push-notification** methods `push_put`, `push_list`, `push_delete`, `push_claim_due`, `push_commit` (see *Push configurations*), the five **blob** methods `keeps_blobs`, `blob_stored`, `put_blob`, `get_blob`, `purge_blobs` (see *File bytes*), and the three **body** methods `keeps_bodies`, `body_list`, `gc_body` (see *Transcripts*) |
| `DynStore` | `Arc<dyn Store>`, the handle the runtime holds |
| `RunRecord`, `NewRun`, `RunUpdate`, `RunStatus`, `RunId` | a run and how to create or advance one |
| `BodyRow`, `BodyWrite` | one transcript row (`idx`, `message`) and what a commit writes (`generation`, `rows`, `writer`), keyed `(run, generation, idx)` in the store (see *Transcripts*) |
| `head_body`, `set_head_body` | the `(generation, len)` a v5 head's `body` names, and the store's write into it: the runtime leaves the generation `null` and the store resolves it |
| `BlobId` | the content address of a file artifact's bytes: a SHA-256 as 64 lower-case hex, opaque in this crate (see *File bytes*) |
| `RunHeader` | a run without its state: `run_header`'s answer, with `claimed(now)` (a runnable run and a live lease) for a reader that only needs the run's own bookkeeping. `adam_core::claimed(status, lease_until, now)` is that rule, shared with a runtime view |
| `JournalEntry` | the recorded outcome of one step, keyed by `(run, seq)` |
| `ClaimScope` | `Any` (default) or `Pinned`, the scope of a `claim_due`. Closed: no `#[non_exhaustive]` |
| `Lease` | a run claimed by a worker until a deadline |
| `RunQuery`, `ConversationScope` | what `list_runs` and `count_runs` select: always scoped (`Prefix` of an owner's conversations, or `Exact`), optionally by run statuses and last update, one page after a `(updated_at, id)` position |
| `NewPushConfig`, `PushRecord`, `PushProgress`, `PushState` | an A2A push-notification configuration and how far its delivery got; `PushState` (`Active`, `Done`, `GaveUp`) is closed on purpose |
| `StoreError`, `StoreResult` | `AlreadyExists`, `NotFound`, `Conflict`, `ConversationBusy`, `NonDeterminism`, `InvalidInput`, `Corrupt`, `Backend { class, source }`; `#[non_exhaustive]`, see *Errors* |
| `Timestamp`, `now`, `truncate_ms`, `add_ttl` | the instant every `Store` value carries: a `jiff::Timestamp` whose serde bytes are the ones `chrono::DateTime<Utc>` wrote ([ADR 0057](../../docs/decisions/0057-time-is-jiff-with-the-bytes-chrono-wrote.md)) |
| `storable` | the one rule every adapter applies before it writes a JSON value: a NUL in a string or a key becomes U+FFFD, so a state or a journal payload is the same value in every backend ([ADR 0051](../../docs/decisions/0051-a-nul-is-stored-as-the-replacement-character.md)) |
| `MemoryStore` | in-memory `Store`, the reference implementation of the suite |
| `SecretSource` (trait), `DynSecretSource`, `SecretError` | where a secret that agent files name comes from ([ADR 0047](../../docs/decisions/0047-adam-rs-can-be-embedded-in-an-application.md), decision 5): `secret(name)` gives a `SecretString` or `Missing`, `NotUnicode`, `Refused` (`#[non_exhaustive]`: a reader treats a reason it does not know as `Refused`); it is synchronous, so a source answers from memory (a host fetches a vault's secrets first); `kind()` names the source, and `Debug` of a `dyn SecretSource` shows only that, never a value, so a host's source needs no `Debug`; used by `adam-assembly`'s `AgentDef::secrets` and `adam-mcp`'s `Env::source` |
| `ProcessEnv`, `NoSecrets` | the process environment (a server's default), and a source that refuses every name, set or not (an embedded engine's default) |
| `jcs::canonicalize` (also `adam_core::canonicalize`) | RFC 8785 (JCS) of a JSON value: what a card signature (`adam-a2a`, which re-exports it) and a turn signature are made over. Moved here from `adam-a2a` so the engine has it |
| `TurnKeys` (trait), `DynTurnKeys`, `<dyn TurnKeys>::ephemeral()` | the turn key port: `current` (the key that signs now, `CurrentKey`), `sign(kid, input)` (an Ed25519 `Signature`, never the private key), `key_set` (the `KeySet` it verifies with), `kind`; see *Turn keys and signatures* |
| `CurrentKey`, `KeyIdentity`, `KeyStatus`, `CertificateChain`, `KeyId`, `Signature` | how the signing key is named (`Id`, or `Certificate`: a chain that rides as `x5c`), whether it is `Active` or `Renewing` (both enums closed), a key id (printable ASCII, at most 256 bytes) |
| `Ed25519TurnKeys` | `TurnKeys` over one Ed25519 key: `from_pkcs8_pem`, `from_files` (the PEM and the verify JWKS a server mounts), `ephemeral` |
| `KeySet`, `PublicKey` | the public keys by `kid` and the revoked ids, read from and written as a JWKS (`from_jwks`, `to_jwks`); a private member is refused |
| `sign_detached`, `verify_detached`, `SignatureType`, `MAX_SIGNATURE_LEN` | the detached EdDSA JWS of a payload as `adam-turn/v1`, `adam-head/v1` or `adam-step/v1`; **`SignatureType::Request` (`adam-request/v1`) is a new variant of the closed enum**: a `match` on it needs one more arm |
| `sign_attached`, `verify_attached` | the same JWS with the payload attached (`header.payload.signature`): the header and the signing input are the detached form's |
| `ModelCall`, `request_payload`, `sign_request`, `sign_request_at`, `verify_request`, `VerifiedRequest`, `body_sha256`, `TURN_SIGNATURE_HEADER`, `REQUEST_NONCE_LEN`, `REQUEST_MAX_AGE_SECS`, `REQUEST_MAX_SKEW_SECS` | the request signature ([ADR 0068](../../docs/decisions/0068-adam-signs-its-own-history.md), *Amended 2026-11-25 (the request signature)*): the payload `{typ, run, method, path, body_sha256, prompt_jti, iat, nonce}` attached, signed now with a random nonce (`sign_request`) or at a given `iat` and nonce for a test vector; `verify_request(set, jws, call, now)` checks the form, the key and the signature, each member against the request (`Mismatch { member }`), then the window, 300 s before to 60 s after (`Expired`). `ModelCall`'s `Debug` shows the body's length only |
| `turn_payload`, `sign_turn`, `verify_turn`, `TurnPosition`, `ChainHash`, `ConversationHash`, `turn_signature`, `set_turn_signature` | a turn's payload `{typ, run, cnv, idx, prv, msg}`, the chain `prv`, and the member `extra_content.adam.turn_signature` |
| `TurnKeyError`, `TurnKeyConfigError`, `KeySetError`, `TurnSignatureError` | see *Errors* |
| `turn::testkit`, `turn_keys_conformance!` | the cases every `TurnKeys` passes, and `testkit::Certified`: a key named by a certificate chain, whose signatures carry `x5c` |
| `testing` (`#[doc(hidden)]`) | `test_env`, `skipped`, `require_db`: gate for database-backed tests, not part of the supported API |

```rust
use adam_core::{MemoryStore, NewRun, RunStatus, RunUpdate, Store};
use serde_json::json;

let store = MemoryStore::new();
store.migrate().await?;
let run = store
    .create_run(NewRun::new("support-bot", json!({"turn": 0})).conversation("chat-42"))
    .await?;
let run = store
    .commit_run(run.id, run.version, RunUpdate::new(RunStatus::Parked, json!({"turn": 1})))
    .await?;
```

`create_run` and `commit_run` return the record **without its state** (`state` is `null`): the caller
supplied it, and the state is the largest member of the record. Read the run for its document — and
note **what that document is**: in the contract release the reads that return a record
(`load_run`, `load_head`, `claim_due`, `open_run_for_conversation`, `list_runs`) answer the v5
**head** for a run whose head is current, not the whole agent state. A consumer that decoded
`RunRecord::state` as the agent state must use `Runtime::state(run)` (the joined state) or
`Runtime::view(run).state` (the head) from `adam-runtime`, or join the head with
`body_list(run, generation)` itself.
Of the run writes, `journal_put` is the exception: it returns the payload it stored, which is what the
loop decodes the step's outcome from (a store may have replaced a NUL in it,
[ADR 0051](../../docs/decisions/0051-a-nul-is-stored-as-the-replacement-character.md)); the push
methods return the cleaned configuration and cursor too.

The semantics (CAS commits, first-writer-wins journal, leases, one open run
per conversation) are described in the [architecture](../../docs/architecture.md#the-mental-model).

## Claim scope and the run owner

`Store::claim_due(agents, worker, scope, now, ttl, limit)` takes a closed
`ClaimScope`:

| Scope | Claimable runs | Owner |
|---|---|---|
| `Any` (default) | every due run without a live lease | neither read nor written: claiming works as before owners existed |
| `Pinned` | due runs without a live lease **and** with no owner or with `worker` as owner | the first claim of a run without an owner sets `worker` as the owner |

A run's *owner* is scheduling data next to the lease, not part of `RunRecord` or the run state.
`release_lease` and `commit_run` leave it alone, so a released run comes back only to the same
worker. It is never cleared: a pinned run whose owner is gone is stranded (nothing adopts it;
see [ADR 0002](../../docs/decisions/0002-workspace-placement.md)). `adam-core` does not know
about `Placement`; the host maps `Placement::pins_runs()` to `ClaimScope::Pinned`
([`adam-host`](../adam-host/README.md)). The signature change is breaking for anyone who
implements `Store`; the conformance cases in
[`adam-store-testkit`](../adam-store-testkit/README.md) prove an implementation.

`Store::release_worker_leases(worker)` drops every run lease `worker` holds, so a restarted process
claims its runs at once instead of waiting for each TTL. Like `release_lease` it does not clear the
run's owner, so a released pinned run still comes back only to the same worker, and a lease held by
another worker is untouched. The caller must be the only live process using `worker` (in this
repository's deployment `WORKER_ID` is the pod name): the sweep releases every lease that id holds.
The method is **defaulted to a no-op**, so it breaks no implementer: a
store that does not implement it waits for the TTL. `MemoryStore` implements it; the suite
`worker_leases_conformance!` in [`adam-store-testkit`](../adam-store-testkit/README.md) proves an
implementation.

## Listing runs

`Store::list_runs(&RunQuery)` returns one page of runs ordered by `updated_at` descending and then `id` descending (what A2A's
`ListTasks` needs: most recently updated first, made total by the id), starting strictly after `query.after`, and
`Store::count_runs` how many match the filters (ignoring `after` and `limit`). The scope is required, so a listing cannot leak
another owner's runs by leaving a filter out: `ConversationScope::Prefix` for everything of an owner (the A2A server uses
`<subject>:`), `Exact` for one conversation. Every adapter serves a page from an index (Postgres compares the conversation id
in the `"C"` collation so a prefix is a range, MongoDB uses an anchored escaped prefix). Two more **required** `Store` methods:
breaking for implementers, proved by the `list_runs_*` and `count_runs_*` conformance cases.

## Reading a header

`Store::run_header(id)` answers a `RunHeader` — the run's id, agent,
conversation, parent, status, wake time, version, timestamps and lease, **never its state** — or
`None`. A reader that only needs the run's own bookkeeping (a parent check, a
step's cancel watch, a listing that builds its view from the record it already has) reads this
instead of `load_run`: the state is the largest member of a run, and for an `LlmAgent` it is the
whole transcript. `RunHeader::claimed(now)` applies the same rule as `RunView::claimed`: only a
runnable run, and only a lease that has not run out.

The method is **defaulted**, so it breaks no implementer: the default reads `lease_until` and
`load_run` and projects the record (two reads, in that order, for the same interleaving reason
`Runtime::view` reads them in that order). Both shipped adapters override it with one statement that
projects the header columns and the lease, and the conformance case
`run_header_matches_the_record` proves an implementation agrees with `load_run` + `lease_until`.

## File bytes

A file artifact's bytes are content-addressed: `adam_runtime::blob_id(&bytes)` is their SHA-256 as a
`BlobId`, and the runtime uploads them before the commit that names them, so they survive the later
commits of that run and are collected once no run references them. This crate keeps the address and
the four **defaulted** `Store` methods; it has no hash dependency, so it never computes a digest
itself.

| Method | What |
|---|---|
| `keeps_blobs()` | whether the store keeps file bytes out of a run's state. `false` (the default) means the bytes live in the state, as before |
| `blob_stored(id)` | whether the bytes of `id` are already there; defaults to `false` ("put it"). The runtime checks it before it sends a file again, so a file an earlier commit uploaded is not re-sent |
| `put_blob(id, bytes)` | store `bytes` under `id`; idempotent, and the bytes behind an id never change. A store that keeps none answers `InvalidInput` |
| `get_blob(id)` | the bytes, or `None` when none are stored (never written, or collected). A store that keeps none answers `InvalidInput`, as `put_blob` does |
| `purge_blobs(before)` | delete the blobs written before `before` that no run references; returns how many |

`RunUpdate` gained a public `blobs: Vec<BlobId>` field: **the ids the commit adds**, a delta the
store unions with the ids it already recorded, so a commit that names none keeps the earlier ones.
**A hand-built `RunUpdate { .. }` literal stops compiling**: add `blobs: Vec::new()` or use
`RunUpdate::new(..)` / `.blobs(..)`, which keep their meaning. `RunUpdate::new` is unchanged and
sets it empty, so every existing caller compiles.

`purge_finished` keeps its signature and extends its contract: a run's references go with it, and
the blobs no surviving run references and that were written before its `before` are deleted in the
same sweep. A digest shared by several runs survives until the last one is purged.

## Transcripts: the head and the body

The contract release of [ADR 0055](../../docs/decisions/0055-a-runs-state-is-a-head-and-an-append-only-transcript.md)
keeps a run's transcript apart from its head: the runtime writes the head, the rows and — for a store
that keeps bodies — the tombstone (`state = null`) on every commit, and **every read that returns a
record answers the same document**: the head when `head_version == version`, the whole `state`
otherwise. `load_run`, `load_head`, `claim_due`, `open_run_for_conversation` and `list_runs` all
follow that rule; `Runtime::view` is one `load_head` and no body, and `Runtime::state` joins the head
with the generation it names. A store that keeps no bodies is not written the contract: the runtime
keeps the whole v4 `state` for it, exactly as before.

| Method | What |
|---|---|
| `keeps_bodies()` | whether the store keeps a transcript beside the head (`body_list`, `RunUpdate::body`). `false` (the default) means `RunUpdate::state` is the whole run, exactly as before |
| `body_list(run, generation)` | the rows of one generation, ordered by `idx`; empty when it does not exist. A store that keeps none answers empty |
| `gc_body(run, keep_generation, before)` | delete the superseded generations of `run` written before `before`; returns how many rows. The generation a head names is never collected. A store that keeps none answers `0` |
| `load_head(id)` | the run as an **observer** reads it: its `state` is the v5 head when the store has a current one (`head_version == version`), and the whole state otherwise (a v4 run, or a head a **v4** commit did not rebuild; a tombstoned run — `state = null` — serves its head). Defaults to `load_run`, which answers the same document in the contract release, so a store that keeps no head answers exactly as it does today. `Runtime::view` reads it and never a body; `Runtime::state` joins the head with the generation it names (`body_list`) |

`BodyRow { idx, message }` is one row; `BodyWrite { generation, rows, writer }` is what a commit
writes. A generation is **append-only**: `generation: None` appends `rows[old_len..]` to the current
head's generation (or starts one when there is no head), and `Some(new)` is a compaction that writes
the whole transcript under `new`. `writer` is the attempt's id: a commit that loses the CAS deletes
the rows (and references) that writer added, so a loser cannot poison the next append. The head names
`{generation, len}`; the store resolves the generation and patches the head.

`RunUpdate` gained two public fields: `head: Option<Value>` (the v5 head this commit writes; the
runtime also passes it for a delivery, a cancel or a park, which re-encode the head they read, so the
head stays current) and
`body: Option<BodyWrite>` (the rows this commit writes; `None` writes none). **A hand-built
`RunUpdate { .. }` literal stops compiling**: add `head: None, body: None` or use
`RunUpdate::new(..)`, which keeps its signature and sets both, and the `.head(..)` / `.body(..)`
builders. A commit that writes rows writes them **before** the CAS, and an adapter checks that all
of them landed: a head must never name a generation that is missing a row.

`commit_run` stores `state` as it is given: the runtime writes `null` (the tombstone) for a commit
that writes a head to a store that keeps bodies, and the whole v4 state otherwise, so a store that
keeps no bodies keeps today's bytes exactly. A store does not decide the format.

`purge_finished` keeps its signature and extends its contract: a run's body rows go with it.

## Turn keys and signatures

[ADR 0068](../../docs/decisions/0068-adam-signs-its-own-history.md) has adam sign the turns it
produces and verify every history that comes back. Slice T1 is this crate's part: the port, its
Ed25519 implementation and the form. **Nothing signs or verifies a run yet** (slice T3), and no
variable is read (`TURN_SIGNING_KEY_FILE`, `TURN_SIGNING_KEY_ID`, `TURN_VERIFY_KEYS_FILE` are slice
T2's; `Ed25519TurnKeys::from_files` reads the files they will name).

* **The port.** A host gives a `TurnKeys`: adam asks it for the current key, then to sign bytes with
  it, and for the key set. A private key never crosses the port, so a keychain, a KMS or an HSM can
  sign. The key is named by an id (`vault:<name>#v<n>`, or its RFC 7638 thumbprint) or by a
  certificate chain, and says whether it is `Renewing`: a key a PKI issues fits as it is, and
  `tests/turn_signature.rs` has one (`WithChain`) that passes the conformance suite.
* **The form.** `BASE64URL(header)..BASE64URL(signature)`, with the header the RFC 8785 form of
  `{"alg":"EdDSA","kid":…,"typ":…}` (plus `x5c` for a chain). `verify_detached` accepts that form
  only: one encoding of the header, exactly those members, 64 bytes of signature, at most
  `MAX_SIGNATURE_LEN` (16 KiB) in all, refused before decoding when longer.
* **A turn.** The payload is the RFC 8785 form of `{typ, run, cnv, idx, prv, msg}`: `cnv` the
  SHA-256 of the conversation id (the run's id when it has none), `prv` the chain over every
  message before it (`h(-1) = SHA-256(JCS({typ: "adam-chain/v1", run, cnv}))`,
  `h(i) = SHA-256(h(i-1) ‖ SHA-256(JCS(message i)))`), `msg` the message without its
  `extra_content.adam` member. The signature rides in that member, at the message's top level, not
  in a provider's members (which a model switch drops).
* **Revocation.** A `KeySet` refuses a revoked id whether its public key is listed or not; a signing
  key the set revokes is refused at start.
* **Ed25519 here is pure Rust** (`ed25519-dalek`, strict verification, weak keys refused), because
  the engine's tree may hold neither `aws-lc-rs` nor `ring` (`tools/embedded-deps/check.sh`). It
  gives the same bytes as `aws-lc-rs` (a test in `adam-a2a`).
* **Cost** (`cargo run -p adam-core --release --example turn_signature_timing`, one core of a
  2.1 GHz Xeon, 2026-11-25): a signature 20 µs, a verification 49 µs; a 2 KB turn signed with its
  RFC 8785 form 45 µs, verified 67 µs; the chain over 1,000 such turns 7.3 ms.

## Push configurations

A2A push notifications must survive a restart and a second replica, so the webhook a client
registered for a task, and how far its delivery got, are kept in the store beside the run
([ADR 0030](../../docs/decisions/0030-a2a-push-notifications-list-tasks-extended-card-signatures.md)).
The store knows nothing of A2A: `config` and `cursor` are opaque JSON, and what it owns is the
scheduling, as for runs.

| Method | What |
|---|---|
| `push_put(NewPushConfig)` | create or replace `(run, id)`; the run must exist (`NotFound`). New: `Active`, version 1, due at once. Replacing resets state, attempts and error, drops the lease and bumps the version |
| `push_list(run)` | the run's configs, by id |
| `push_delete(run, id)` | idempotent; whether it existed |
| `push_claim_due(agents, worker, now, ttl, limit)` | lease active configs that are due, earliest `next_attempt_at` first; exclusive while the lease lives |
| `push_commit(run, id, expected_version, PushProgress)` | compare-and-swap on the version, drops the lease; `Conflict` on a stale version, `NotFound` when deleted |

A config goes with its run (`purge_finished`). **The store holds what the client gave it, webhook
credentials included, and does not encrypt them**: protect the database like the runs. The five
methods are new **required** methods of `Store`: breaking for anyone who implements it (the
conformance cases `push_*` in [`adam-store-testkit`](../adam-store-testkit/README.md) prove an
implementation).

## Runs the caller is stepping

`Store::claim_due(.., busy, ..)` never returns a run listed in `busy`, whatever its lease says, and
such a run takes no slot of `limit`. The runtime passes the runs it is stepping. A step can outlive
its lease (a renewal that failed, a clock that jumped), and a claim of that run by the worker that
still holds it would lease it a second time: the claim returns a snapshot that the running step is
about to make stale, so a second step could start on it, and the release at the end of the first
step (it matches the worker, not the claim) would clear the new lease. Another worker is not
affected: it claims the run once the lease has expired, as before. The conformance case
`claim_skips_busy_runs` in [`adam-store-testkit`](../adam-store-testkit/README.md) proves an
implementation, for both scopes. The signature change is breaking for anyone who implements
`Store`.

## Time

Every timestamp in this crate is [`Timestamp`](src/time.rs), a newtype over `jiff::Timestamp`
(ADR 0057). Stores keep milliseconds (`truncate_ms`), because BSON dates are millisecond-precision
and every adapter normalizes to it so they behave the same. The newtype's serde writes RFC 3339
with `Z` and chrono's `AutoSi` digits (0, 3, 6 or 9, the shortest exact one), which is what a
state or a push cursor written before the migration holds: jiff's own serde trims every trailing
zero and would change those bytes. `Timestamp` derefs to `jiff::Timestamp`; adding a
`jiff::SignedDuration` saturates at `Timestamp::MAX`/`MIN`. `truncate_ms` floors, as chrono's
`timestamp_millis` did, so a pre-1970 value with a fractional millisecond stores the same
millisecond it stored before the migration.

## Errors

`StoreError` implements `adam_error::Classify`; callers decide from the class,
never from the variant (see [`adam-error`](../adam-error/README.md)).

| Variant | Class |
|---|---|
| `AlreadyExists`, `ConversationBusy` | `Rejected` |
| `NotFound` | `NotFound` |
| `Conflict` | `Conflict` |
| `NonDeterminism`, `Corrupt` | `Corrupt` |
| `InvalidInput` | `Invalid` |
| `Backend { class, source }` | the `class` the adapter chose |

The turn errors carry no key, no signed content and no signature; they name a key by its id:

| Error | Class |
|---|---|
| `TurnKeyError::NotSigning` (the port does not sign with that key: ask for the current one again) | `Conflict` |
| `TurnKeyError::Unavailable`, `Refused` | `Transient`, `Unauthenticated` |
| `TurnKeyConfigError` (a key file that cannot be read, is not an Ed25519 PKCS#8 private key, a public key, a revoked signing key, a bad verify set) | `Invalid`; `NoRandomness` is `Internal` |
| `KeySetError`, `BadKeyId`, `BadCertificateChain` | `Invalid` |
| `TurnSignatureError` (unsigned, too long, malformed, another type, an unknown or revoked key, does not verify; for a request signature, a member that is not the request's, an `iat` outside the window) | `Corrupt` |

`InvalidInput` is data the store cannot hold (an integer above `i64::MAX`, a
bad table prefix); `Corrupt` is data read back that
breaks an invariant (an unknown status, a negative version, a vanished journal
entry). `Backend` keeps the driver's error as its `source` (a `BoxError`, so no
driver type is in this crate) and its message does not repeat it. An adapter
builds it with `StoreError::unavailable(e)` (`Transient`),
`StoreError::internal(e)` (`Internal`) or `StoreError::corrupt_source(e)`
(`Corrupt`). Only `Conflict` and a transient `Backend` are retryable.

## Features and environment

No Cargo features. Nothing reads a turn key variable yet (slice T2). The `testing` helpers read `ADAM_TEST_REQUIRE_DB`
(`1` or `true`): a database-gated test whose variable is unset then fails
instead of skipping.

## Tests

`MemoryStore` (which implements the push methods too) is run through the conformance suite by
`crates/adam-store-testkit/tests/memory.rs`. Behavioural tests of the types
live next to the code; those in `src/store/mod.rs` (`class_table`,
`retryable_is_derived_from_the_class`,
`backend_display_does_not_repeat_its_source`) pin the class of every
`StoreError` variant with an exhaustive `match`. `the_defaults_keep_no_bodies_and_serve_the_whole_state`
pins the three transcript defaults on a store that overrides none of them, and
`a_head_round_trips_and_a_headless_commit_makes_it_stale` and
`the_body_gc_keeps_the_current_generation_and_collects_an_old_one` pin `MemoryStore`'s head and its
GC with an explicit cutoff. `src/secrets.rs` checks that `ProcessEnv` gives what the process has,
that `NoSecrets` refuses a name the process has, that a held source's `Debug` shows its kind and not
its values, and that no error says a value. `src/jcs.rs` pins RFC 8785's ordering and numbers.
`tests/turn_signature.rs` holds ADR 0068's decisive tests of slice T1: a golden vector (RFC 8032's
own, RFC 8037's thumbprint, and a turn whose payload and signature were made outside the crate with
Python and OpenSSL), each payload field changed, a `kid` outside the set, both keys of a rotation, a
public-only key, a revoked key, no `Debug` or error holding a key, the size bound, the form, a
certificate chain in the header, and the conformance suite run on the ephemeral, PEM and
certificate keys; `src/turn/ed25519.rs` reads PKCS#8 v1 and both forms of v2.
`tests/request_signature.rs` reproduces the shared prompt-signature contract's vector V7 byte for byte
(`tests/fixtures/request-signature/`, test material only, its README says where it comes from): V7a by a key
named by a certificate chain (`x5c`, `testkit::Certified`), V7b by a `kid`; both verify, and a changed body,
method, path, `prompt_jti` or run, a changed byte of the signature and an `iat` outside the window fail; a request
signature is neither a history signature nor a detached one; no prompt signs `null`, and each signature has its
own nonce. The round trip of a
signed message through every store is `a_message_canonicalizes_as_it_was_written` and
`body_a_row_canonicalizes_as_it_was_written` in `adam-store-testkit`. Database gating is described in the
[testing guide](../../docs/guides/testing.md).

## See also

[`adam-store-testkit`](../adam-store-testkit/README.md),
[`adam-runtime`](../adam-runtime/README.md) (the consumer of `Store`),
[`adam-error`](../adam-error/README.md).
