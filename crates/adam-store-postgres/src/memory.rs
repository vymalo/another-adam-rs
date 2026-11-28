//! [`PgMemory`]: the PostgreSQL adapter of adam-memory's L2 [`Memory`] port (ADR 0081's decisions
//! 2 and 4), behind the `memory` feature.
//!
//! One [`PgMemory`] is configured with one [`MemoryScope`] and stores its lessons in
//! `<prefix>memory_lesson` (schema version 10: [`PgStore::migrate`](crate::PgStore) creates the
//! table, and so does `PgMemory::migrate`). Two adapters over one database with two scopes never
//! read, overwrite or retire each other's lessons, because every key and every query carries the
//! configured scope. Its semantics are the in-process reference's
//! ([`InProcessMemory`](adam_memory::InProcessMemory)) exactly: a draft is prepared by the port's
//! own [`prepare`], one transaction takes a `pg_advisory_xact_lock` over the scope and the
//! audience, refuses `AudienceFull` when a new or revived served lesson would exceed the bound, and
//! upserts keeping the stored creation time; `recall` serves only `active` lessons, the freshest
//! first; `retire` keeps the lesson and stops serving it.
//!
//! # The blocking bridge
//!
//! [`Memory`] is synchronous and `deadpool_postgres` is async. A [`PgMemory`] owns a small
//! multi-thread Tokio runtime and runs every call on it through one bridge ([`PgMemory::block_on`]):
//! outside a runtime it calls `Runtime::block_on` on the owned one; inside a multi-thread runtime
//! it uses `block_in_place` with the caller's `Handle`; inside a current-thread runtime it runs the
//! future on the owned runtime from a scoped thread and joins it. **The constraint**: never call
//! `Runtime::block_on` directly from an async context — the bridge exists to choose the one legal
//! way per context, and a fourth context (a future running on the adapter's own runtime calling
//! back into the adapter) is not one of them. `PgMemory` is `Send + Sync + 'static` (the port's
//! bound), and no transaction is held open while caller code runs.
//!
//! Timestamps are `TIMESTAMPTZ`, so they keep microseconds where the in-process reference keeps
//! nanoseconds: two writes less than a microsecond apart compare equal here and are ordered by
//! root cause, the port's stable tie-break, while the reference would order them by time. A recall
//! still serves the freshest first, and the port's five-millisecond case is unaffected.

use std::future::Future;
use std::panic::resume_unwind;
use std::sync::Arc;

use adam_core::time::Timestamp;
use adam_core::{StoreError, StoreResult};
use adam_memory::{
    Audience, Lesson, LessonDraft, LessonSettings, LessonState, Memory, MemoryError, MemoryResult,
    MemoryScope, prepare,
};
use adam_postgres::{PgConfig, classify, classify_pool};
use deadpool_postgres::Pool;
use serde_json::Value;
use tokio::runtime::{Builder, Handle, Runtime, RuntimeFlavor};
use tokio_postgres::Row;
use tokio_postgres::types::{FromSql, ToSql};

use crate::{DEFAULT_POOL_SIZE, check_table_prefix, memory_lesson_ddl, migrate_lock_key};

/// What a decode failure and a stored value that breaks the port's rules are reported under: the
/// table's plain name, stable across prefixes.
const CORRUPT_PATH: &str = "memory_lesson";

/// The lesson columns a read returns: everything of a [`Lesson`] but its scope, which the adapter
/// was configured with and does not store in the record.
const LESSON_COLUMNS: &str = "audience, root_cause, remedy, rationale, anchors, sources, review, state, created, updated, \
     retired_at";

/// SQL text with table names baked in, built once per [`PgMemory`] (like [`Sql`](crate::Sql) for
/// the store).
#[derive(Clone, Debug)]
struct MemSql {
    /// The advisory-lock key every migrator of this prefix takes (`PgStore::migrate`'s too).
    migrate_lock: String,
    migrate: Vec<Arc<str>>,
    lock_scope_audience: Arc<str>,
    lock_migrate: Arc<str>,
    served: Arc<str>,
    upsert: Arc<str>,
    recall: Arc<str>,
    retire: Arc<str>,
}

impl MemSql {
    fn new(p: &str) -> Self {
        let t = format!("{p}memory_lesson");
        Self {
            migrate: memory_lesson_ddl(p),
            migrate_lock: migrate_lock_key(p),
            // The write is serialised per scope and audience, like the reference's one lock per
            // store call: sixteen writers on one audience write one at a time.
            lock_scope_audience: Arc::from(
                "SELECT pg_advisory_xact_lock(hashtextextended($1 || chr(31) || $2, 0))",
            ),
            // The migration's own lock: `adam-rs:migrate:{prefix}`, the key every migrator of this
            // prefix takes (`PgStore::migrate`'s transaction takes it too).
            lock_migrate: Arc::from("SELECT pg_advisory_xact_lock(hashtext($1))"),
            // How many lessons the audience serves and whether this key is among them: the bound
            // counts what is served, and an update or a revival of a served lesson is not a new
            // one.
            served: Arc::from(format!(
                "SELECT count(*) FILTER (WHERE state = 'active') AS served,
                        max(state) FILTER (WHERE root_cause = $3) AS mine
                   FROM {t} WHERE scope = $1 AND audience = $2"
            )),
            // An upsert keeps the stored creation time, so a retried approval is one lesson, and a
            // fresh approval of a retired lesson serves it again (`retired_at` cleared).
            upsert: Arc::from(format!(
                "INSERT INTO {t} (scope, audience, root_cause, remedy, rationale, anchors, sources,
                                  review, state, created, updated, retired_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, NULL)
                 ON CONFLICT (scope, audience, root_cause) DO UPDATE
                    SET remedy = EXCLUDED.remedy, rationale = EXCLUDED.rationale,
                        anchors = EXCLUDED.anchors, sources = EXCLUDED.sources,
                        review = EXCLUDED.review, state = EXCLUDED.state,
                        updated = EXCLUDED.updated, retired_at = NULL,
                        created = {t}.created
                 RETURNING {LESSON_COLUMNS}"
            )),
            // The recall serves only active lessons, the freshest first (then by root cause so the
            // answer is stable).
            recall: Arc::from(format!(
                "SELECT {LESSON_COLUMNS} FROM {t}
                  WHERE scope = $1 AND audience = $2 AND state = 'active'
                  ORDER BY updated DESC, root_cause ASC"
            )),
            // One statement: retire unless already retired (then keep `retired_at` and `updated`,
            // so retiring twice is one retirement), and return the stored row. A row the statement
            // does not touch is either already retired (returned unchanged) or absent (refused).
            // It takes the same scope/audience lock `record` takes, in the same implicit
            // transaction, so a retire cannot interleave between a record's count and its upsert:
            // the volatile function in the CTE is always evaluated.
            retire: Arc::from(format!(
                "WITH lock AS (SELECT pg_advisory_xact_lock(hashtextextended($1 || chr(31) || $2, 0)))
                 UPDATE {t}
                    SET state = 'retired', retired_at = COALESCE(retired_at, $4),
                        updated = CASE WHEN state = 'retired' THEN updated ELSE $4 END
                   FROM lock
                  WHERE scope = $1 AND audience = $2 AND root_cause = $3
                 RETURNING {LESSON_COLUMNS}"
            )),
        }
    }
}

/// A [`Memory`] on PostgreSQL: one configured [`MemoryScope`], one table, one owned runtime.
///
/// Built with [`PgMemory::connect`] (like [`PgStore::connect`](crate::PgStore), a pool of
/// [`DEFAULT_POOL_SIZE`] connections and one opened at once) or [`PgMemory::from_pool`] (your own
/// `deadpool_postgres::Pool`). Call `migrate()` before first use unless the store schema is
/// migrated already: **[`PgStore::migrate`](crate::PgStore) creates `<prefix>memory_lesson` at
/// schema version 10**, whatever the `memory` feature says.
///
/// The synchronous [`Memory`] port over the async driver is bridged by one owned multi-thread
/// Tokio runtime. It may be called outside a runtime and from a multi-thread or a current-thread
/// one, but never from a future running on its own runtime: read the blocking bridge's notes at
/// the top of `src/memory.rs` before calling this type from async code.
#[derive(Debug)]
pub struct PgMemory {
    pool: Pool,
    runtime: Runtime,
    sql: MemSql,
    settings: LessonSettings,
    scope: MemoryScope,
}

impl PgMemory {
    /// Connect with a pool of [`DEFAULT_POOL_SIZE`] connections and open a connection now, so a
    /// database that does not answer fails here and not at the first call. Call
    /// [`migrate`](Self::migrate) before first use (unless [`PgStore::migrate`](crate::PgStore)
    /// ran on this database).
    ///
    /// # Errors
    ///
    /// [`StoreError::InvalidInput`] (class `Invalid`) on a URL or prefix the driver refuses, and
    /// [`StoreError`] with its own class on a pool that cannot be built or a database that does
    /// not answer — the same classes [`PgStore::connect`](crate::PgStore) gives them. A
    /// deployment validates its configuration at startup and exits 78 on the invalid ones (ADR
    /// 0081 decision 12); a retrying caller must not retry an `Invalid`.
    pub fn connect(url: &str, settings: LessonSettings, scope: MemoryScope) -> StoreResult<Self> {
        let config = PgConfig::parse(url)?;
        let pool = config.pool(DEFAULT_POOL_SIZE as usize)?;
        let memory = Self::from_pool(pool, settings, scope)?;
        memory.block_on(memory.ping())?;
        Ok(memory)
    }

    /// Open one connection now, to learn whether the database answers.
    async fn ping(&self) -> StoreResult<()> {
        let client = self.pool.get().await.map_err(classify_pool)?;
        let stmt = client.prepare_cached("SELECT 1").await.map_err(classify)?;
        client.query_one(&stmt, &[]).await.map_err(classify)?;
        Ok(())
    }

    /// Use an existing pool (default table prefix `adam_`), e.g. the one the run store already
    /// uses with a table prefix of its own. Sizing is the caller's. Opens no connection.
    ///
    /// # Errors
    ///
    /// [`StoreError::internal`] (class `Internal`, never retried) when the owned runtime cannot be
    /// built, which is a thread-spawn failure in the process, not the database.
    pub fn from_pool(
        pool: Pool,
        settings: LessonSettings,
        scope: MemoryScope,
    ) -> StoreResult<Self> {
        let runtime = Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("adam-memory-pg")
            .enable_all()
            .build()
            .map_err(StoreError::internal)?;
        Ok(Self {
            pool,
            runtime,
            sql: MemSql::new("adam_"),
            settings,
            scope,
        })
    }

    /// Use a different table-name prefix (default `adam_`), the same rule as
    /// [`PgStore::with_table_prefix`](crate::PgStore). Every key and query also carries the
    /// configured scope, so a prefix is isolation of the tables, not of an L2 scope.
    ///
    /// # Errors
    ///
    /// [`StoreError::InvalidInput`] (class `Invalid`) on a prefix that is not `[a-z0-9_]`, at most
    /// 40 characters and starting with a letter or underscore.
    pub fn with_table_prefix(mut self, prefix: &str) -> StoreResult<Self> {
        check_table_prefix(prefix)?;
        self.sql = MemSql::new(prefix);
        Ok(self)
    }

    /// The pool this memory uses, e.g. to share it with the run store or a test's admin calls.
    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    /// The scope it is configured with. Every key and every query carries it.
    pub fn scope(&self) -> &MemoryScope {
        &self.scope
    }

    /// Create `<prefix>memory_lesson` and its recall index (schema version 10's DDL), idempotent,
    /// under the migrate advisory-lock key `adam-rs:migrate:{prefix}` (the one
    /// [`PgStore::migrate`](crate::PgStore) serialises on), without touching the `meta` version
    /// row. **[`PgStore::migrate`](crate::PgStore) also creates the table** at schema version 10:
    /// this is only for a deployment that built without its store migration or without the
    /// `memory` feature of its binary earlier.
    ///
    /// # Errors
    ///
    /// [`StoreError`] with the driver's class, as [`PgStore::migrate`](crate::PgStore): a
    /// connection loss is `Transient`, a rejected statement `Internal`.
    pub fn migrate(&self) -> StoreResult<()> {
        let lock = self.sql.migrate_lock.clone();
        self.block_on(async {
            let mut client = self.pool.get().await.map_err(classify_pool)?;
            let tx = client.transaction().await.map_err(classify)?;
            let stmt = tx
                .prepare_cached(&self.sql.lock_migrate)
                .await
                .map_err(classify)?;
            tx.execute(&stmt, &[&lock]).await.map_err(classify)?;
            for stmt in &self.sql.migrate {
                tx.batch_execute(stmt).await.map_err(classify)?;
            }
            tx.commit().await.map_err(classify)
        })
    }

    /// Run one call's future to completion on the owned runtime — the synchronous port's bridge
    /// to the async driver (read the module docs first).
    ///
    /// Outside a runtime the owned one drives the future; inside a multi-thread runtime the
    /// caller's reactor does, with `block_in_place` freeing its worker; inside a current-thread
    /// runtime (whose context must not block) a scoped thread drives it on the owned runtime and
    /// joins it. Never call `Runtime::block_on` from async code directly: this is the one entry
    /// point that picks a legal way per context.
    fn block_on<F, R>(&self, fut: F) -> R
    where
        F: Future<Output = R> + Send,
        R: Send,
    {
        match Handle::try_current() {
            Err(_) => self.runtime.block_on(fut),
            Ok(handle) => match handle.runtime_flavor() {
                RuntimeFlavor::MultiThread => tokio::task::block_in_place(|| handle.block_on(fut)),
                // A current-thread runtime panics on `block_in_place` and on `Runtime::block_on`;
                // a scoped thread borrows `self` and joins before this returns.
                _ => {
                    let joined = std::thread::scope(|scope| {
                        scope.spawn(|| self.runtime.block_on(fut)).join()
                    });
                    match joined {
                        Ok(result) => result,
                        Err(payload) => resume_unwind(payload),
                    }
                }
            },
        }
    }

    /// `record`, inside one transaction: the lock, the count, the upsert, the stored row.
    async fn record_stored(&self, prepared: &Lesson) -> MemoryResult<Lesson> {
        let mut client = self.pool.get().await.map_err(MemoryError::backend)?;
        let tx = client.transaction().await.map_err(MemoryError::backend)?;
        let scope = self.scope.as_str().to_owned();
        let audience = prepared.audience.as_str().to_owned();
        let stmt = tx
            .prepare_cached(&self.sql.lock_scope_audience)
            .await
            .map_err(MemoryError::backend)?;
        tx.execute(&stmt, &[&scope, &audience])
            .await
            .map_err(MemoryError::backend)?;
        let stmt = tx
            .prepare_cached(&self.sql.served)
            .await
            .map_err(MemoryError::backend)?;
        let row = tx
            .query_one(&stmt, &[&scope, &audience, &prepared.root_cause])
            .await
            .map_err(MemoryError::backend)?;
        let served: i64 = decode(&row, "served")?;
        let mine: Option<String> = decode(&row, "mine")?;
        let limit = self.settings.limits.lessons_per_audience;
        let already_served = mine.as_deref() == Some("active");
        if !already_served && served >= limit as i64 {
            // The dropped transaction rolls back: a refusal stores nothing.
            return Err(MemoryError::AudienceFull { limit });
        }
        let anchors = Value::Array(
            prepared
                .anchors
                .iter()
                .map(|anchor| Value::String(anchor.clone()))
                .collect(),
        );
        let sources = Value::Array(
            prepared
                .provenance
                .sources
                .iter()
                .map(|source| Value::String(source.clone()))
                .collect(),
        );
        let state = state_text(prepared.state).to_owned();
        let (created, updated) = (prepared.created.0, prepared.updated.0);
        let params: &[&(dyn ToSql + Sync)] = &[
            &scope,
            &audience,
            &prepared.root_cause,
            &prepared.remedy,
            &prepared.rationale,
            &anchors,
            &sources,
            &prepared.provenance.review,
            &state,
            &created,
            &updated,
        ];
        let stmt = tx
            .prepare_cached(&self.sql.upsert)
            .await
            .map_err(MemoryError::backend)?;
        let row = tx
            .query_one(&stmt, params)
            .await
            .map_err(MemoryError::backend)?;
        let lesson = lesson_from_row(&row)?;
        tx.commit().await.map_err(MemoryError::backend)?;
        Ok(lesson)
    }

    async fn recall_stored(&self, audience: &str) -> MemoryResult<Vec<Lesson>> {
        let client = self.pool.get().await.map_err(MemoryError::backend)?;
        let scope = self.scope.as_str().to_owned();
        let audience = audience.to_owned();
        let stmt = client
            .prepare_cached(&self.sql.recall)
            .await
            .map_err(MemoryError::backend)?;
        let rows = client
            .query(&stmt, &[&scope, &audience])
            .await
            .map_err(MemoryError::backend)?;
        rows.iter().map(lesson_from_row).collect()
    }

    async fn retire_stored(&self, audience: &str, root_cause: &str) -> MemoryResult<Lesson> {
        let client = self.pool.get().await.map_err(MemoryError::backend)?;
        let scope = self.scope.as_str().to_owned();
        let audience = audience.to_owned();
        let root_cause = root_cause.to_owned();
        let at = Timestamp::now().0;
        let stmt = client
            .prepare_cached(&self.sql.retire)
            .await
            .map_err(MemoryError::backend)?;
        let row = client
            .query_opt(&stmt, &[&scope, &audience, &root_cause, &at])
            .await
            .map_err(MemoryError::backend)?;
        row.map_or_else(
            || Err(MemoryError::LessonNotFound(root_cause.clone())),
            |row| lesson_from_row(&row),
        )
    }
}

impl Memory for PgMemory {
    fn record(&self, draft: LessonDraft) -> MemoryResult<Lesson> {
        self.scope.validate()?;
        let prepared = prepare(&self.scope, draft, &self.settings, Timestamp::now())?;
        self.block_on(self.record_stored(&prepared))
    }

    fn recall(&self, audience: &Audience) -> MemoryResult<Vec<Lesson>> {
        self.scope.validate()?;
        audience.validate()?;
        self.block_on(self.recall_stored(audience.as_str()))
    }

    fn retire(&self, audience: &Audience, root_cause: &str) -> MemoryResult<Lesson> {
        self.scope.validate()?;
        audience.validate()?;
        self.block_on(self.retire_stored(audience.as_str(), root_cause))
    }
}

/// The state as the `state` column holds it. A closed set: a new state must fail to compile here.
fn state_text(state: LessonState) -> &'static str {
    match state {
        LessonState::Proposed => "proposed",
        LessonState::Active => "active",
        LessonState::Retired => "retired",
    }
}

/// One column of a row, by name. A failure here is stored data that breaks the port's rules or a
/// schema the adapter does not expect: [`MemoryError::Corrupt`], never retried for ever.
fn decode<'a, T: FromSql<'a>>(row: &'a Row, column: &str) -> MemoryResult<T> {
    row.try_get(column).map_err(|err| corrupt(err.to_string()))
}

fn corrupt(reason: String) -> MemoryError {
    MemoryError::Corrupt {
        path: CORRUPT_PATH.to_owned(),
        reason,
    }
}

/// A row of [`LESSON_COLUMNS`] as the [`Lesson`] it is. The port's own serde is the decoder (the
/// type is non-exhaustive: another crate cannot build one), so a stored row that cannot become a
/// `Lesson` is [`MemoryError::Corrupt`].
fn lesson_from_row(row: &Row) -> MemoryResult<Lesson> {
    let audience: String = decode(row, "audience")?;
    let root_cause: String = decode(row, "root_cause")?;
    let remedy: String = decode(row, "remedy")?;
    let rationale: String = decode(row, "rationale")?;
    let anchors: Value = decode(row, "anchors")?;
    let sources: Value = decode(row, "sources")?;
    let review: String = decode(row, "review")?;
    let state: String = decode(row, "state")?;
    let created: jiff::Timestamp = decode(row, "created")?;
    let updated: jiff::Timestamp = decode(row, "updated")?;
    let retired_at: Option<jiff::Timestamp> = decode(row, "retired_at")?;
    let doc = serde_json::json!({
        "audience": audience,
        "root_cause": root_cause,
        "remedy": remedy,
        "rationale": rationale,
        "anchors": anchors,
        "provenance": { "sources": sources, "review": review },
        "state": state,
        "created": Timestamp(created),
        "updated": Timestamp(updated),
        "retired_at": retired_at.map(Timestamp),
    });
    serde_json::from_value(doc).map_err(|err| corrupt(err.to_string()))
}
