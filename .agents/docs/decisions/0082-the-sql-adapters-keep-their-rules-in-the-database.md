# 0082: the evidence, the survey and the slice proofs

For people: [0082. The SQL adapters keep their rules in the database](../../../docs/decisions/0082-the-sql-adapters-keep-their-rules-in-the-database.md),
which states the decision. This page holds the survey it was written from (every rule the Rust side enforces, with
its `path:line`), the third-party facts with their sources, and what each slice must prove.

Surveyed at `origin/main` `45abb141` on 2026-11-27. Line numbers are at that commit; re-read them before a slice.

## How to check a slice against this page

1. Every row of the PostgreSQL survey is marked with its form, by decision 1's rule: **function** (the rule moves
   into the schema; after the slice the row's `path:line` no longer holds it, the Rust side binds, calls and decodes),
   **column** (the `served` column, decision 12), **inline** (the statement stays in Rust, parameterized and prepared
   once; a value Rust computes for it today moves into the statement where the row says so), **no** (stays in Rust)
   or **already SQL**. A slice never turns an inline row into a function: that changes decision 1 first. The SQLite
   rows keep **yes**, **partly** and **no** (SQLite has no functions, decision 8).
2. Run the conformance suites against PostgreSQL 18, the floor and, once P0 lands, the only server CI runs
   (`ADAM_TEST_POSTGRES_URL=… ADAM_TEST_REQUIRE_DB=1 cargo nextest run -p adam-store-postgres -p adam-runtime`), and
   `cargo test -p adam-store-sqlite` (it never skips).
3. **Break each moved rule once on purpose** in the function body (the mutation listed per slice below), run the suite,
   see the named case fail, and put the mutation and the failing case in the pull request.
4. The fresh-database test (P0) lists every function; a slice that adds one adds it to the list, or the test fails.
5. The fixture `crates/adam-store-postgres/tests/fixtures/schema-v6.sql` becomes `schema-v7.sql`; the unit test
   `the_statements_equal_the_version_6_fixture` (`crates/adam-store-postgres/src/lib.rs:2369`) follows it, regenerated
   with `ADAM_WRITE_FIXTURE=1`.

## The survey: `adam-store-postgres`

`M` is the `Store` method a rule serves. "Already SQL" rows stay where they are and are listed so the survey is
complete.

| # | Rule | Where | M | Form, and why |
|---|---|---|---|---|
| 1 | The served document: the head when `head_version = version` or the state is the JSON `null` tombstone, else the state | `src/lib.rs:138` (`served_columns`) | `load_run`, `load_head`, `claim_due`, `open_run_for_conversation`, `list_runs` | **column**: `served`, a virtual generated column of `{p}runs` (decision 12); every read selects it inline, the claim function returns it. The rule every read shares, written once beside the data |
| 2 | When a run is due: runnable → `wake_at` or `updated_at`, parked → `wake_at`, terminal → none | `crates/adam-core/src/store/mod.rs:1052`, used at `src/lib.rs:1334` and `:1425` | `create_run`, `commit_run` | **function**: `{p}sched_at(status, wake_at, updated_at)`, a `RETURN` body, called by the inline create and inside `{p}commit_run`: a rule two statements would repeat. The core helper stays for the memory and MongoDB stores |
| 3 | Millisecond truncation and the clock | `crates/adam-core/src/store/mod.rs:1076`, `:1090`, `:1199`; every write | all writes, claims, leases | **no**: decision 2; times are arguments |
| 4 | A NUL becomes U+FFFD | `crates/adam-core/src/store/mod.rs:1128`; every write | all writes | **no**: a `JSONB` parameter refuses a NUL at bind (SQLSTATE `22P05`, mapped at `src/lib.rs:450`), before any body runs; ADR 0051 |
| 5 | Create: insert at version 1, `AlreadyExists` / `ConversationBusy` by the violated constraint | `src/lib.rs:1330`, `:799`, `:426`–`:447` | `create_run` | **inline**: one `INSERT` by the caller's id, `sched_at` from row 2's function; the unique indexes stay the rule; the 23505-by-constraint mapping stays in Rust, in the SQLSTATE table |
| 6 | Headless compare-and-swap, then a second statement on another connection to tell `Conflict` (with the winning version) from `NotFound` | `src/lib.rs:1441`, `:820`, `:1996` (`commit_conflict`) | `commit_run` | **function**: `{p}commit_run`, PL/pgSQL, returns an outcome; a read after another writer's commit: the second statement must stay a second statement inside the function (a fresh snapshot, see *Snapshots* below) |
| 7 | Commit with a head, a body or refs: lock the row `FOR UPDATE`, check the version, write, CAS, one transaction | `src/lib.rs:2040`–`:2155`, `:836` | `commit_run` | **function**: the same; several statements that must agree |
| 8 | Body plan: index equals position, an append cannot start past the rows, the generation (given, current, or new), the head's `body` patched to `{generation, len}` | `src/lib.rs:1109`–`:1191` | `commit_run` | **function**: part of `{p}commit_run`, PL/pgSQL, `jsonb_set`, `gen_random_uuid()` (not `uuidv7()`, decision 12); a refusal by `RAISE` (`AD001`) |
| 9 | Body write: an identical foreign row is adopted, one older than 5 min is collected and the insert retried once, a live different row is a `Conflict`, a second occupied slot is `Corrupt` | `src/lib.rs:2164`–`:2270`; grace at `:94` | `commit_run` | **function**: part of `{p}commit_run`, PL/pgSQL; control flow over several statements; `AD004` (Conflict, raised after writes so they roll back) and `AD003` |
| 10 | Blob references deduplicated, inserted once per digest | `src/lib.rs:2084`, `:967` | `commit_run` | **function**: `SELECT DISTINCT unnest(..)` inside `{p}commit_run`, one of the commit's statements |
| 11 | Superseded generations collected after the CAS, one hour of grace, best effort | `src/lib.rs:2147`, `:98`, `:1903` | `commit_run`, `gc_body` | **function**: `{p}gc_body`, `BEGIN ATOMIC`, rows picked by a rule (every generation but the named one, past the grace); still called after the commit (inside it, a failure would fail the commit) |
| 12 | Journal append: insert or nothing, then read the winner on another connection; a different name is `NonDeterminism`; a vanished winner is `Corrupt` | `src/lib.rs:1498`–`:1538`, `:872` | `journal_put` | **function**: `{p}journal_put`, PL/pgSQL; a read after another writer's commit and a `RAISE`: `AD002` with the recorded name in `DETAIL`; a missing run stays 23503 → `NotFound`. `ON CONFLICT DO UPDATE .. RETURNING old.*` (18) would read the winner in the insert, at the cost of an update of the winner's row on every replay: not adopted |
| 13 | Claim: due, not busy, lease free, `FOR UPDATE SKIP LOCKED`, lease set; pinned: `owner IS NULL OR owner = $worker`, `owner = COALESCE(owner, $worker)` | `src/lib.rs:992`–`:1027`, `:1550`; order restored in Rust at `:1585` | `claim_due` | **function**: `{p}claim_due(.., pinned boolean, ..)`, `BEGIN ATOMIC`; rows picked by a rule (due, not busy, lease free, the pinned owner); the claim order moves into the function (`WITH claimed AS (UPDATE .. RETURNING ..) SELECT .. ORDER BY sched_at, id`) |
| 14 | Renew only by the owner and only before the lease ends | `src/lib.rs:883`, `:1596` | `renew_lease` | **inline**: one `UPDATE` by the caller's id; the rule is its `WHERE` |
| 15 | Release only by the owner; the owner pin is untouched | `src/lib.rs:887`, `:1616` | `release_lease` | **inline**: one `UPDATE` by the caller's id; the rule is its `WHERE` |
| 16 | `release_worker_leases` is **not implemented** here: the trait's default does nothing | `crates/adam-core/src/store/mod.rs:841`; `tests/conformance.rs` runs no `worker_leases_conformance!` | `release_worker_leases` | **not in scope**: implementing it changes behaviour (a question for the owner) |
| 17 | `lease_until`, `activity`, `run_header`, `load_version`, journal and body reads | `src/lib.rs:892`, `:1629`, `:1645`, `:1392`, `:1480`, `:1540`, `:1881` | the reads | **inline**: plain reads by id or by run: nothing to keep atomic and no rule another statement shares |
| 18 | Listing and counting: keyset page on `(updated_at, id)`, a prefix as a range plus `starts_with`, the bound `prefix || U+10FFFF` | `src/lib.rs:1032`–`:1077`, `:1664`, `:1693` | `list_runs`, `count_runs` | **inline**: one read whose filter is the caller's; it selects `served`. The prefix bound may become an expression of the statement (P1 decides); the `starts_with` keeps the range correct either way |
| 19 | Push: put replaces and bumps the version; claim like runs; commit CAS, then a second statement for the version | `src/lib.rs:913`–`:955`, `:1714`–`:1851` | `push_*` | **partly function**: `{p}push_claim_due` (`BEGIN ATOMIC`, rows picked by a rule) and `{p}push_commit` (PL/pgSQL: an outcome, a fresh snapshot); put (one upsert by the caller's ids, its version bump in its `SET`), list and delete stay **inline** |
| 20 | Purge in batches of 500, `SKIP LOCKED`, each batch its own statement, then the blob sweep | `src/lib.rs:900`, `:974`, `:101`, `:1853`, `:1976` | `purge_finished`, `purge_blobs` | **function**: `{p}purge_batch` and `{p}sweep_blobs_batch`, `BEGIN ATOMIC`, each returning its count: rows picked by a rule (a finished run past the cutoff; a blob no run references). The loop stays in Rust (decision 6) |
| 21 | A blob's size: computed in Rust, checked by `CHECK (size = octet_length(bytes))` | `src/lib.rs:1928`, `:956`, `:640` | `put_blob` | **inline**: one `INSERT .. ON CONFLICT DO NOTHING` by the digest; the size becomes `octet_length($2)` in the statement; the `CHECK` stays |
| 22 | Migration: session and transaction advisory locks, the version gate (no DDL on a current schema), `CONCURRENTLY` outside a transaction, the version never lowered | `src/lib.rs:261`–`:424`, `:769`, `:1301` | `migrate` | **no**: orchestration of DDL; the functions and the `served` column join as a version 7 step; the server check refuses a server below 18 |
| 23 | Table prefix `[a-z0-9_]`, at most 40 | `src/lib.rs:231` | configuration | **no** |
| 24 | Pool size, statement timeout (a function call is one statement for `statement_timeout`) | `src/lib.rs:470`, `:502` | configuration | **no** |
| 25 | Child runs: `parent_id` has an index (`src/lib.rs:735`) and **no foreign key**; a child's worker reads the parent with `run_header` and cancels when it is `Failed` | `crates/adam-runtime/src/worker.rs:339`, `:354` | `run_header`, then a cancel | **no**: the cancel rewrites the runtime's state document. Without a foreign key a parent can be purged before its children, and the worker reads a missing parent as unknown and cancels nothing (`worker.rs:334`–`:338`); adding one is not this ADR's |
| 26 | Cancellation: load, rewrite the state (error, inbox kept for a session close, missing files pruned), commit, retry on conflict, then `Agent::on_cancelled` | `crates/adam-runtime/src/runtime.rs:773`, `:854` | `load_run`, `commit_run` | **no**: the runtime's document and hook; its commit is row 6 |
| 27 | One open run per conversation | `src/lib.rs:726` (partial unique index) | `create_run`, `commit_run` | already SQL |
| 28 | Status, push state, `version > 0`, `seq >= 0`, `idx >= 0`, digest `^[0-9a-f]{64}$`, cascades | the `migrate` statements, `src/lib.rs:614`–`:757` | all | already SQL |

## The survey: `adam-notify-postgres` and `adam-postgres`

| # | Rule | Where | M | Moves? |
|---|---|---|---|---|
| N1 | One `pg_notify` per batch of up to 32 items, in row order | `crates/adam-notify-postgres/src/lib.rs:524`–`:559`, `:214` | `EventSink::emit`, `Notifier::publish` | already SQL |
| N2 | Payload cap 7 999 bytes; a status's detail cut at a character boundary, a step's input and output dropped, other oversize events dropped | `crates/adam-notify-postgres/src/wire.rs:16`, `:78`–`:164` | `emit`, `publish` | **no**: the cut is of the event's structure; `NOTIFY`'s own 8 000-byte limit is already the server's |
| N3 | Origin filter, wire version, dispatch | `crates/adam-notify-postgres/src/lib.rs:653` | `subscribe` | **no**: the listener's process |
| N4 | Channel prefix rule | `crates/adam-notify-postgres/src/lib.rs:171` | configuration | **no** |
| N5 | `runnable` and `finished` published by the runtime after its commit | `crates/adam-runtime/src/runtime.rs:754`, `:765` | `publish` | **question for the owner** (decision 7): a trigger on `{p}runs` could send them at commit |
| A1 | SQLSTATE → class: `08*`, `53*`, `40001`, `40P01`, `57014`, `57P01`–`57P03` transient, the rest internal | `crates/adam-postgres/src/lib.rs:100`–`:122` | every call | **no**, and it gains the `AD` rows: the one place of decision 4 |
| A2 | `DATABASE_URL`, TLS, pool timeouts | `crates/adam-postgres/src/config.rs` | connection | **no** |

## The survey: `adam-store-sqlite`

| # | Rule | Where | M | Moves? |
|---|---|---|---|---|
| S1 | The served document (`CASE`) | `crates/adam-store-sqlite/src/lib.rs:82` | the reads, `claim_due` | **yes**: a view `{p}runs_served` |
| S2 | `sched_at` computed in Rust | `src/lib.rs:529`, `:641` | `create_run`, `commit_run` | **yes**: a trigger `AFTER INSERT` and `AFTER UPDATE OF status, wake_at, updated_at`, from `wake_at` and `updated_at` (the caller's time: no database clock) |
| S3 | A pre-read for `AlreadyExists` before the insert | `src/lib.rs:533`–`:545` | `create_run` | **partly**: the primary key already refuses it; the pre-read goes, the violation is mapped |
| S4 | Version CAS and its outcomes | `src/lib.rs:672`, `:760`–`:775` | `commit_run` | already SQL (the `WHERE`); the outcome stays Rust control flow |
| S5 | Journal non-determinism | `src/lib.rs:848`–`:912` | `journal_put` | **partly**: a `BEFORE INSERT` trigger raises `adam:nondeterminism` on another name; reading the winner stays |
| S6 | Claim in three statements (select, update, select) | `src/lib.rs:931`–`:1049` | `claim_due` | **yes**: one `UPDATE .. WHERE id IN (SELECT .. LIMIT ?) RETURNING` (SQLite 3.35 or later); the order is restored in Rust (`RETURNING` order is unspecified) |
| S7 | Renew, release, release a worker's leases | `src/lib.rs:1051`–`:1125` | the lease methods | already one statement each |
| S8 | `migrate` clears every lease | `src/lib.rs:510`–`:519`, resting on `lock_file` at `:265` | `migrate` | **no**: the reason is the file lock, which SQL cannot see |
| S9 | A newer schema is refused | `src/lib.rs:489` | `migrate` | **no**: before any DDL |
| S10 | Body plan and write (adopt, collect, retry, conflict) | `src/lib.rs:1991`, `:2050` | `commit_run` | **no**: control flow; SQLite has no procedures |
| S11 | Superseded generation collected after the commit | `src/lib.rs:786`–`:801` | `commit_run` | **partly**: a trigger `AFTER UPDATE OF head` deletes other generations written before `NEW.updated_at` minus one hour; `gc_body` stays a statement |
| S12 | Purge in batches of 500 **inside one `IMMEDIATE` transaction** | `src/lib.rs:1471`–`:1503` | `purge_finished` | **yes**: one `DELETE`; the batches bound nothing today |
| S13 | Digest checked by length only (PostgreSQL checks hex) | `src/lib.rs:424` | `put_blob` | **yes**: a `BEFORE INSERT` trigger raising on `digest GLOB '*[^0-9a-f]*'` |
| S14 | Error mapping | `src/lib.rs:1913`–`:1989` | every call | **no**, and it gains the `adam:<code>` messages of a `RAISE(ABORT, ..)`, in one table |

## Evidence

| Claim | Status | Source |
|---|---|---|
| `BEGIN ATOMIC` / `RETURN` bodies are `LANGUAGE sql` only, parsed at definition, track dependencies, no polymorphic arguments | *verified 2026-11-27* | https://www.postgresql.org/docs/current/sql-createfunction.html |
| The form exists in 14 and not in 13 | *verified 2026-11-27* | https://www.postgresql.org/docs/14/sql-createfunction.html, https://www.postgresql.org/docs/13/sql-createfunction.html |
| The feature matrix lists "BEGIN ATOMIC function bodies" | *verified 2026-11-27* | https://www.postgresql.org/about/featurematrix/detail/begin-atomic-function-bodies/ (the page names no version) |
| 15 to 19 supported; 18 until 2030-11-14, 19 (released 2026-11-19) until 2031-11-19; 14 ended 2026-11-12, 13 on 2025-11-13, 12 on 2024-11-21 | *verified 2026-11-27* | https://www.postgresql.org/support/versioning/ |
| A custom SQLSTATE is five digits or upper-case letters, not `00000`; avoid codes ending in `000` | *verified 2026-11-27* | https://www.postgresql.org/docs/current/plpgsql-errors-and-messages.html |
| `COMMIT` in a procedure only from a top-level `CALL` (or nested `CALL`s, no intervening `SELECT`) | *verified 2026-11-27* | https://www.postgresql.org/docs/current/plpgsql-transactions.html |
| `COMMIT` in a procedure called through the extended query protocol, as `tokio-postgres` prepares it | *unverified* | the reason decision 6 keeps the sweep loop in Rust |
| `batch_execute` sends the whole string as one simple-protocol `Query` message; nothing splits it on `;` | *verified 2026-11-27* | `tokio-postgres` 0.7.18 (the `Cargo.lock` version), `src/simple_query.rs:46`–`:62` |
| `migrate` sends each statement of its list with `batch_execute` | *verified 2026-11-27* | `src/lib.rs:316`, `:402`, `:421` |
| `gen_random_uuid()` is a core function (version 4, random) in the 13 and the 18 documentation; 18 adds the alias `uuidv4()` | *verified 2026-11-27* | https://www.postgresql.org/docs/13/functions-uuid.html, https://www.postgresql.org/docs/18/functions-uuid.html |
| `uuidv7([shift interval])` (18) makes a time-ordered id whose timestamp is computed when it is made (millisecond UNIX time, a sub-millisecond part, random bits); the caller can only shift it | *verified 2026-11-27* | https://www.postgresql.org/docs/18/functions-uuid.html, https://www.postgresql.org/docs/18/release-18.html |
| `RETURNING` takes `old` and `new` in `INSERT`, `UPDATE`, `DELETE` and `MERGE` (18); for `INSERT .. ON CONFLICT DO UPDATE` the old values are non-`NULL` for conflicting rows | *verified 2026-11-27* | https://www.postgresql.org/docs/18/release-18.html, https://www.postgresql.org/docs/18/dml-returning.html |
| `RETURNING` returns no row the `WHERE` did not match | *unverified* (the page implies it: "Returning Data from Modified Rows"; it does not say it) | https://www.postgresql.org/docs/18/dml-returning.html |
| Generated columns are virtual by default (18): computed when read; the expression only immutable, the current row, no other generated column; a virtual one uses only built-in functions and types, operators and casts included; no value can be written to one in `INSERT` or `UPDATE` | *verified 2026-11-27* | https://www.postgresql.org/docs/18/ddl-generated-columns.html, https://www.postgresql.org/docs/18/release-18.html |
| "Adding a virtual generated column never requires a rewrite" | *verified 2026-11-27* | https://www.postgresql.org/docs/18/sql-altertable.html (Notes) |
| `jsonb_typeof` and the `jsonb` and integer `=` operators of the served expression are immutable | *unverified*: the server refuses a generated column whose expression is not, so P0's migration of a fresh database is the proof | — |
| A virtual generated column can be read in `UPDATE .. RETURNING` (the claim) | *unverified*: P2's claim cases are the proof | — |
| PostgreSQL 18.0 was released 2025-09-25 | *verified 2026-11-27* | https://www.postgresql.org/docs/18/release-18.html |
| The bundled SQLite is 3.54.1 (`RETURNING`, triggers with `RAISE`, `GLOB`) | *verified 2026-11-27* | `Cargo.lock`: `libsqlite3-sys` 0.39.1, its `sqlite3/sqlite3.h` |
| The supported range today: PostgreSQL 12+, CI 12 and 17, compose 16 | *verified 2026-11-27* | `crates/adam-store-postgres/README.md:3`, `.github/workflows/ci.yml:319`–`:320`, `compose.yaml:29`, `docs/reference/store-adapters.md:111` |
| Other servers below 18 that P0 moves: the second Postgres job of `ci.yml` and the coder workflow's service run 17 | *verified 2026-11-27* | `.github/workflows/ci.yml:384`, `.github/workflows/coder.yml:275` |
| The chart's CloudNativePG `Cluster` names no `imageName`, so the operator's default major version applies | *verified 2026-11-27* (no `imageName` in `deploy/coder/templates/cnpg-cluster.yaml`); which major that is: *unverified* | — |

### Snapshots: why some functions are PL/pgSQL

Under `READ COMMITTED` one SQL statement sees one snapshot. A headless commit written as one statement
(`WITH upd AS (UPDATE .. RETURNING ..) SELECT ..`) would, after losing to a concurrent commit it waited for, read the
**pre-statement** version and report `actual = expected`. Today the second read runs on another connection with a
fresh snapshot (`src/lib.rs:1996`). In a `VOLATILE` PL/pgSQL function each statement takes a new snapshot, which keeps
`actual` exact. The same holds for the journal (the winner of `ON CONFLICT DO NOTHING` is read after it commits,
`src/lib.rs:1525`) and the push commit (`src/lib.rs:1833`). A slice that writes one of these as one statement must
show the conflict case reporting the winning version. PostgreSQL 18's `RETURNING old` does not remove the second
statement: a compare-and-swap that lost matched no row, so its `RETURNING` has nothing to return.

### JSON equality in the body write

Row 9 compares a stored message with the one this attempt would write. Rust compares `serde_json::Value`s
(`src/lib.rs:2234`); in SQL it is `jsonb` equality, which treats `1` and `1.0` as equal where `Value` does not. The
stored row was decoded from `jsonb` either way, so the difference can only adopt a row that differs in a number's
spelling; P5 states which it keeps and adds a case for it.

## Slice proofs

| Slice | Functions | Decisive cases (the testkit's names) | Broken on purpose |
|---|---|---|---|
| P0 | none yet; the version 7 step, the `AD` table in `adam-postgres`, the server check | a new `every_function_is_created_and_answers` (fresh database, `migrate`, call each listed function, compare `pg_proc` for the prefix with the list); a server below 18 refused before any DDL (`server_version_num` below 180000); the CI matrix's two rows `{ postgres: "18" }`, `ci.yml:384`, `coder.yml:275` and `compose.yaml:29` on 18; the chart's `cnpg-cluster.yaml` pins `imageName` at 18, or the pull request shows the operator's default is 18 or later (a pin changes the chart's render, which another repository reads: say so) | list a function the schema lacks, and one it has but the list does not; let a 17 server through the check |
| P1 | the `served` column; the inline reads, `list_runs`, `count_runs` select it | `body_every_read_serves_the_current_head`, `body_a_stale_head_serves_the_state`, `body_a_tombstoned_run_with_a_stale_head_serves_the_head`, `body_a_head_that_is_json_null_is_not_a_head`, `list_runs_*`, `count_runs_ignores_the_page`, `run_header_matches_the_record`; `tests/head_view.rs` counts still one lease and one head | serve the state when the head is current (in the column's expression); drop the `starts_with` (in the inline statement) |
| P2 | `claim_due` (renewal, release, `lease_until` and `activity` stay inline) | `claim_is_exclusive_under_concurrency`, `claim_skips_busy_runs`, `pinned_claims_are_exclusive_and_stable_under_concurrency`, `pinned_claim_never_gives_a_run_to_another_worker`, `any_claim_ignores_and_never_sets_the_owner`, `lease_expiry_allows_takeover`, `renew_and_release_lease`; `activity_conformance!` | remove `SKIP LOCKED`; drop the owner filter; let a renewal extend an ended lease |
| P3 | `sched_at`, `commit_run` (headless); the inline create calls `sched_at` | `concurrent_commits_single_winner`, `commit_stale_version_conflicts`, `commit_missing_run`, `claim_respects_due_rules`, `conversation_race_single_winner`, `create_duplicate_id`, `two_creates_with_different_ids_in_one_conversation_leave_one_open_run`; `headless_measurement!` | compute `sched_at` from `updated_at` for a parked run; report `actual = expected` |
| P4 | `journal_put` | `journal_detects_nondeterminism`, `journal_first_writer_wins`, `journal_concurrent_writers_agree`, `journal_requires_run` | compare nothing on conflict |
| P5 | `commit_run` with parts, `gc_body` | `body_conformance!`, `blob_a_run_reference_keeps_the_blob_alive`, `tests/body_writers.rs` (adopted, collected after the grace, `Conflict`); `transcript_measurement!` | adopt a different row; collect a row inside the grace |
| P6 | `push_claim_due`, `push_commit` (put, list and delete stay inline) | `push_*` cases of `store_conformance!` | skip the version bump on a put |
| P7 | `purge_batch`, `sweep_blobs_batch` | `purge_finished_runs`, `blob_purge_respects_before`, `blob_purge_finished_removes_the_refs_and_the_blob`, `blob_a_shared_digest_survives_until_the_last_run_is_purged`, `body_purge_removes_the_rows` | purge a parked run; sweep a referenced blob |
| S1 | the view, the `sched_at` and digest triggers; SQLite schema 2 | `store_conformance!` (`claim_respects_due_rules`, the body read cases), `tests/schema.rs` (a version 1 file upgrades, a newer one is refused) | a trigger that ignores `wake_at` |
| S2 | the journal and body-collection triggers, the one-statement claim and purge | `journal_detects_nondeterminism`, `claim_is_exclusive_under_concurrency`, `purge_finished_runs`, `body_a_compaction_starts_a_new_generation_and_the_gc_collects_the_old`, `worker_leases_conformance!` | the journal trigger raising on the same name |

P2 and P5 also measure a claim and a commit against the statements of `45abb141` (cost of a call: *unverified* until
then) and put the numbers in the pull request.
