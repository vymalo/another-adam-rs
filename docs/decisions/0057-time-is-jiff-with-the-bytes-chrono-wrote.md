# 0057. Time is jiff, with the bytes chrono wrote

Status: **Accepted** (2026-11-19). The follow-up of the RUSTSEC-2026-0543 acceptance (`deny.toml`): `chrono` is
unmaintained, and adam-rs's own code moves to `jiff` while the A2A types it builds on (`a2a-lf`, and therefore
`adam-a2a`'s card, task and page types) still use `chrono`. Supersedes no decision; [ADR 0047](0047-adam-rs-can-be-embedded-in-an-application.md)'s
note that sqlx is built with its `chrono` feature stays true (decision 4).

## Context

`RUSTSEC-2026-0543` says chrono is unmaintained: `informational = "unmaintained"`, `patched = []` (there is no
version to move to), all versions affected, advisory date 2026-09-14, and the record recommends `jiff` and `time`
(*verified 2026-11-19*, the advisory-db checkout at commit `3157b0e258782691`,
`crates/chrono/RUSTSEC-2026-0543.md`). The owner accepted it on 2026-11-17 with a migration of adam-rs's own code to
`jiff` to follow, and a review by 2027-02-17. Eleven crates depended on `chrono` directly: `adam-core` (the `Store` types),
`adam-runtime` (`Clock`, `Ctx::now`, `Envelope`, `Inbound`, `SessionSummary`), `adam-llm-agent`
(`Conversation.chain_started`, `PendingRemote.deadline`), `adam-a2a` (`TaskQuery`, `PageToken`, `PushCursor`,
`PushStore`), `adam-a2a-runtime`, `adam-assembly` (`RoundState.started`), `adam-store-postgres`, `adam-store-mongodb`,
`adam-store-testkit`, `adam-ui` (`Grant.expires_at`, the `Clock` alias) and `adam-workspace` (the GitHub App JWT and
the token expiry). No `NaiveDate` is used anywhere.

**The one real risk is the serialized bytes.** No store record is serde-serialized (the timestamps are
`TIMESTAMPTZ` and BSON dates), but these are: the run state (`Envelope`, `Inbound`, `Conversation`, `RoundState`),
the journaled `Ctx::now_journaled` bare timestamp, the push cursor (`PushCursor.failing_since`), and the A2A
`TaskStatus.timestamp` (an `a2a-lf` type). `chrono::DateTime<Utc>`'s serde writes RFC 3339 with `Z` and
`SecondsFormat::AutoSi`: no fractional digits when the nanoseconds are zero, else 3 when `nano % 1_000_000 == 0`,
else 6 when `nano % 1_000 == 0`, else 9 (*verified 2026-11-19*, `chrono-0.4.45/src/format/formatting.rs`).
`jiff::Timestamp`'s serde trims **every** trailing zero (`.120Z` becomes `.12Z`), so a plain swap changes the bytes of
every stored state and cursor that carries a fraction.

## Decision

### 1. One newtype, `adam_core::time::Timestamp`

`Timestamp(pub jiff::Timestamp)` is the instant of every `Store` value, of the runtime's clock and of every
timestamp adam-rs stores or sends itself. Its `Serialize` writes chrono's `AutoSi` rule (the module's tests pin a
whole second, `.120`, `.100`, `.123`, `.123456`, `.123456700`, `.123456789` and a pre-epoch `.500`), and its
`Deserialize` delegates to `jiff::Timestamp`, so a state written by either build decodes and re-encodes to itself.
`Display` is the same text; `Deref` gives jiff's accessors (`as_millisecond`, `as_second`, `subsec_nanosecond`).
`Add`/`Sub<SignedDuration>` saturate at `Timestamp::MAX`/`MIN` (the framework's arithmetic always saturated), and
`Sub<Timestamp>` is a `SignedDuration`, which is what a backoff or a deadline comparison needs. `span_of` turns a
`std::time::Duration` into a `SignedDuration`, saturating; it replaces every `chrono::Duration::from_std(..).unwrap_or(MAX)`.

`truncate_ms` floors, as chrono's `timestamp_millis` did, before 1970 too: jiff's `as_millisecond`
truncates toward zero, so the function corrects a negative value with a fractional millisecond by
one, and a stored millisecond is the one the framework wrote before this decision.

Durations are `jiff::SignedDuration`, never `Span`: a `Span` carries calendar units, and adding one with a day unit
to a `Timestamp` panics. Sentinels: `DateTime::<Utc>::MAX_UTC` becomes `Timestamp::MAX`; the `u64::MAX` timeout of
`Envelope::with_limit` still parks at the end of time (a test covers it).

### 2. Which types stay chrono

`TaskStatus.timestamp`, `TaskQuery.status_timestamp_after` and `PageToken.updated_at` are A2A's own types and stay
`chrono::DateTime<Utc>`; `adam-a2a` keeps its dependency for them, and `adam-a2a-runtime` converts at the boundary
(`convert::chrono_of`, `convert::timestamp_of`). adam's own push values (`PushCursor.failing_since`,
`PushStore::next_attempt_at`, `claim_due`'s `now`) are `Timestamp`: the cursor is stored JSON, so its bytes are the
contract. `adam-a2a` gains a dependency on `adam-core` for that type.

### 3. The stores keep their columns and their bytes

* **PostgreSQL.** sqlx 0.9 has no jiff support, and 0.9 is the driver the workspace pins. `adam-store-postgres`
  keeps `chrono` and sqlx's `chrono` feature, with a private `PgTime(Timestamp)` implementing `Type<Postgres>`
  (`TIMESTAMPTZ`), `Encode` and `Decode` by delegating to `chrono::DateTime<Utc>`. The column type and the encoded
  value are exactly what the store wrote before this decision. It is the only crate that keeps chrono for a driver.
* **MongoDB.** No newtype: `Bson::DateTime::from_millis(t.as_millisecond())` and `Timestamp::from_millisecond(..)`.
  The crate drops chrono.

### 4. `deny.toml`

The RUSTSEC-2026-0543 ignore stays: `a2a-lf`, `a2a-server-lf` and `a2a-pb`, sqlx's `chrono` feature and the
PostgreSQL bridge keep chrono in the graph, and removing the ignore makes `cargo deny check` red. Its reason says
the migration landed and names what remains; the review date stays 2027-02-17. A `[bans]` rule denies a direct `chrono` dependency outside the allowed wrappers, so a new
crate that reaches for chrono fails the check rather than arriving silently.

### 5. Breaking changes

| Changed | To | Who breaks |
|---|---|---|
| `Store` methods' `DateTime<Utc>` (`wake_at`, `now`, `ttl` deadlines, `lease_until`, `purge_finished`) | `adam_core::time::Timestamp` | every `Store` implementation; `adam-store-testkit` proves one |
| `Clock::now` | `Timestamp` | every embedder's clock, including test clocks |
| `Ctx::now`, `Ctx::now_journaled`, `Inbound.received_at`, `Envelope` deadlines and wakes, `SessionSummary.updated_at` | `Timestamp` | agents that read the clock or build an `Inbound`/`Transition::Park` |
| `Conversation.chain_started`, `PendingRemote.deadline`, `RoundState.started`, `Grant.expires_at`, the `Clock` aliases of `adam-ui` and `adam-workspace` | `Timestamp` (workspace: `jiff::Timestamp`) | code that sets or reads them |
| `PushCursor.failing_since`, `PushStore`'s `next_attempt_at` and `claim_due(now)` | `Timestamp` | push-store implementations and deliverers |
| `RunHeader` (`wake_at`, `created_at`, `updated_at`, `lease_until`), `claimed(.., now)` and `Runtime::now` (merged from #199) | `Timestamp` | a reader that builds a header or computes `claimed` |
| `Store::purge_blobs(before)` (ADR 0055 S1, merged as `c6594ce` while this decision was in review) | `Timestamp`; `RunUpdate` keeps its new `blobs` | a store's blob sweep takes the new type; a `RunUpdate` literal needs `blobs` (ADR 0055's row) |
| `chrono` as a direct dependency of adam's crates | `jiff` | nothing at run time; the serialized bytes do not change |

The consumer-facing note is the `adam-upgrade` skill's known-migrations table. Two decisions merged while this
one was in review added timestamps to the `Store` surface — #199's `RunHeader`/`claimed`/`Runtime::now` and ADR 0055
S1's `purge_blobs` — and the merge converted them with the rest, so the public surface has no chrono type left.

## Consequences

* The workspace's own code no longer uses chrono except at the A2A boundary and the PostgreSQL bridge, so the
  unmaintained crate is left in the graph only where an upstream type or driver requires it.
* A timestamp is one type end to end (`Store`, runtime, agent, session, push), and its arithmetic saturates rather
  than panicking, as before.
* The bytes of every stored run state, journal payload and push cursor are unchanged: a deployment can roll this
  out beside an older build without a migration, and a state written by either decodes in the other. The one
  exception is the old `DateTime::MAX_UTC` sentinel (decision 1), reachable only through an absurd
  `Duration::MAX` timeout or TTL: it is now `Timestamp::MAX`, which is a different instant but the same
  "end of time".
* `Timestamp` is a new public type of `adam-core` and appears in the `Store` trait: a new required type for
  implementers, documented in `docs/reference/store-adapters.md` and the crate READMEs.

## Alternatives rejected

* **Use `jiff::Timestamp` everywhere.** It changes the stored bytes (trailing zeros trimmed) and its `Sub` returns a
  `Span`, which is the wrong type for a backoff and panics when a day unit meets a `Timestamp`.
* **Keep chrono and wait for sqlx/bson support.** The advisory is accepted with a migration to follow; waiting keeps
  the framework's own code on the unmaintained crate for a driver's sake.
* **Serialize a millisecond integer instead of RFC 3339.** It would change the stored bytes and every consumer that
  reads the state JSON, for no benefit.
* **One `chrono`-typed bridge in `adam-core`.** It would put the unmaintained crate back in the port, which is what
  this decision removes.

## Tests

`adam-core` pins each byte shape and a round trip; `adam-a2a` compares every `AutoSi` width against a
real `chrono::DateTime<Utc>` value, reads the old digit widths, `+00:00` and lowercase `t`/`z`, and pins
`truncate_ms` against chrono's `timestamp_millis`; `adam-runtime` pins a `.120` envelope deadline and a
`now_journaled` journal payload; `adam-a2a` pins a `.120` push cursor and a `TaskStatus`'s JSON; the
store testkit pins a `.120` instant, a truncation below a millisecond, a pre-1970 floor and the top of
jiff's range on every adapter. The existing pins (`conversation.rs`'s `2026-01-02T03:04:05Z`,
`envelope.rs`, the store suites) pass unchanged. The database suites run with `ADAM_TEST_REQUIRE_DB=1`,
so a skip is a failure.
