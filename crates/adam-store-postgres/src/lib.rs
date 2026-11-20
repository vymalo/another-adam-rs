//! PostgreSQL implementation of [`adam_core::Store`].
//!
//! # Schema
//!
//! Five tables (names prefixed, `adam_` by default):
//!
//! * `adam_runs`: one row per run. `state` is `JSONB`, so runs are queryable
//!   from SQL for dashboards and debugging. `sched_at` is the precomputed "due
//!   at" time; claiming is a range scan on a partial index over it. `owner`
//!   (schema version 2) is the worker a pinned claim tied the run to; it is
//!   never cleared, and `NULL` for a run no pinned claim has taken.
//! * `adam_journal`: one row per recorded step, primary key `(run_id, seq)`,
//!   deleted with its run through `ON DELETE CASCADE`.
//! * `adam_push` (schema version 3): one row per A2A push-notification configuration, primary
//!   key `(run_id, id)`, deleted with its run through `ON DELETE CASCADE`. `config` holds the
//!   webhook **with its credentials, as the client gave them**; `cursor` is how far delivery
//!   got. A partial index over `(agent, next_attempt_at)` for active configs is the claim scan.
//! * `adam_blob` and `adam_run_blob` (schema version 5, part 1): file bytes out of the run.
//!   `adam_blob` is content-addressed (`digest` is the primary key, `size` is checked against the
//!   bytes) and shared by every run that names a digest; `adam_run_blob` is the append-only set of
//!   digests one run references, primary key `(run_id, digest)`, deleted with its run.
//!
//! # Concurrency
//!
//! * Commits are a single `UPDATE .. WHERE version = $expected`; one that carries blob references
//!   writes them in the same transaction, so a commit that loses the version check writes none.
//! * Claiming uses `FOR UPDATE SKIP LOCKED`, so concurrent workers split the due
//!   runs between them instead of queueing on the same rows. A pinned claim adds
//!   `owner IS NULL OR owner = $worker` and sets `owner` in the same statement.
//! * "One open run per conversation" is a partial unique index, so it holds
//!   even across processes.
//! * Journal writes are `INSERT .. ON CONFLICT DO NOTHING`, then a read of the
//!   winner.
//! * Push configurations are claimed like runs (`FOR UPDATE SKIP LOCKED`, one statement) and
//!   written with `UPDATE .. WHERE version = $expected`; putting an id again is one
//!   `INSERT .. ON CONFLICT DO UPDATE` that bumps the version.
//! * A blob is written once (`INSERT .. ON CONFLICT DO NOTHING`): the bytes behind a digest never
//!   change, so a retried put is one no-op.
//!
//! No transaction is held open while agent code runs.
//!
//! # Driver
//!
//! The statements, the tables and the wire bytes are the ones `sqlx` wrote; only the driver
//! changed ([ADR 0053](../../docs/decisions/0053-sqlite-a-store-service-a-database-per-agent-and-a-static-coder.md)).
//! Statements are prepared once per connection (`prepare_cached`), time columns bind
//! `adam_core::time::Timestamp` directly through `postgres-types`' `with-jiff-0_2`, and the
//! migration and the notification listener take a connection out of the pool (`Object::take`)
//! so a session lock or a `LISTEN` never returns to it.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use adam_core::store::{add_ttl, now, sched_at, storable, truncate_ms};
use adam_core::time::Timestamp;
use adam_core::{
    BlobId, ClaimScope, ConversationScope, JournalEntry, Lease, NewPushConfig, NewRun,
    PushProgress, PushRecord, PushState, RunHeader, RunId, RunQuery, RunRecord, RunStatus,
    RunUpdate, Store, StoreError, StoreResult,
};
use adam_error::ErrorClass;
use adam_postgres::{PgConfig, classify, classify_pool};
use async_trait::async_trait;
use deadpool_postgres::{ClientWrapper, Object, Pool};
use serde_json::Value;
use tokio_postgres::Row;
use tokio_postgres::types::{FromSql, ToSql};
use uuid::Uuid;

/// Current schema version written to the `<prefix>meta` table.
///
/// * 1: the first schema.
/// * 2: `runs.owner`, the worker a pinned claim ties a run to (see [`ClaimScope`]).
/// * 3: the `push` table (A2A push-notification configurations and their delivery progress) and
///   the index `ListTasks` reads runs by (`runs_list`).
/// * 4: the listing index becomes `runs_list_status` with `INCLUDE (status)`, so the listing's
///   status filter is answered from the index instead of the heap. An upgrade builds it beside the
///   version 3 `runs_list` (`CREATE INDEX CONCURRENTLY`) and drops that one only once the new one
///   is valid, so `runs` stays readable and writable for the whole build.
/// * 5, part 1: file bytes out of the run. `adam_blob` holds content-addressed bytes and
///   `adam_run_blob` the digests each run references (`ON DELETE CASCADE`); the upgrade is
///   metadata-only (two new tables and their indexes, no rewrite of `runs`).
pub const SCHEMA_VERSION: i32 = 5;

/// Purge deletes in batches of this many runs (plus their journals).
const PURGE_BATCH: i64 = 500;

/// The pool size [`PgStore::connect`] uses when `ADAM_PG_POOL_SIZE` is unset: 16 connections per
/// process.
pub const DEFAULT_POOL_SIZE: u32 = 16;

/// The smallest pool `ADAM_PG_POOL_SIZE` or [`PgStore::connect_with_pool_size`] accepts.
///
/// Two, not one: a one-connection pool serializes every store call behind whichever query is
/// running, and a migration that holds the connection (the session advisory lock) would leave a
/// healthy process unable to answer. The `adam-notify-postgres` listener has its own connection
/// and does not take one from the pool, but the floor is kept so a deployment that sets one does
/// not silently lose that headroom.
///
/// Two is the floor, not a size to run: [`DEFAULT_POOL_SIZE`] (16) is the intended size.
pub const MIN_POOL_SIZE: u32 = 2;

/// The largest pool `ADAM_PG_POOL_SIZE` or [`PgStore::connect_with_pool_size`] accepts.
pub const MAX_POOL_SIZE: u32 = 65_535;

const PUSH_COLUMNS: &str = "run_id, id, agent, owner, config, cursor, state, attempts, last_error, next_attempt_at, version, created_at, updated_at";

const RUN_COLUMNS: &str = "id, agent, conversation_id, parent_id, status, state, wake_at, version, created_at, updated_at";

/// What `create_run` and `commit_run` return: [`RUN_COLUMNS`] without `state`. The caller wrote
/// the state, and a `RETURNING` that echoed it would parse the largest column back on every
/// transition.
const RUN_METADATA_COLUMNS: &str =
    "id, agent, conversation_id, parent_id, status, wake_at, version, created_at, updated_at";

/// What [`Store::run_header`] reads: the record's metadata and the lease, never `state`.
const RUN_HEADER_COLUMNS: &str = "id, agent, conversation_id, parent_id, status, wake_at, version, created_at, updated_at, lease_until";

/// A [`Store`] backed by PostgreSQL (12+).
#[derive(Clone, Debug)]
pub struct PgStore {
    pool: Pool,
    sql: Sql,
}

impl PgStore {
    /// Connect with a pool of [`ADAM_PG_POOL_SIZE`](pool_size_from_env) connections (16 by
    /// default). Call [`Store::migrate`] before first use.
    pub async fn connect(url: &str) -> StoreResult<Self> {
        Self::connect_with_pool_size(url, pool_size_from_env()?).await
    }

    /// Connect with a pool of exactly `max_connections` ([`MIN_POOL_SIZE`] to
    /// [`MAX_POOL_SIZE`]), whatever the environment says. Call [`Store::migrate`] before first
    /// use.
    pub async fn connect_with_pool_size(url: &str, max_connections: u32) -> StoreResult<Self> {
        let store = Self::connect_lazy_with_pool_size(url, max_connections)?;
        store.ping().await?;
        Ok(store)
    }

    /// Like [`connect`](Self::connect), with the same [`ADAM_PG_POOL_SIZE`](pool_size_from_env)
    /// pool, but opens no connection yet: the first use does. A process that wants to check its
    /// own configuration before it depends on the database builds on this, then calls
    /// [`ping`](Self::ping). Fails on a URL the driver cannot parse or a pool size out of range.
    pub fn connect_lazy(url: &str) -> StoreResult<Self> {
        Self::connect_lazy_with_pool_size(url, pool_size_from_env()?)
    }

    /// [`connect_lazy`](Self::connect_lazy) with an explicit pool size.
    fn connect_lazy_with_pool_size(url: &str, max_connections: u32) -> StoreResult<Self> {
        let config = PgConfig::parse(url)?;
        Self::from_config(config, max_connections)
    }

    /// Use an already parsed [`PgConfig`], e.g. the one `adam-service` parses once for the store
    /// and the notification listener. Opens no connection.
    pub fn from_config(config: PgConfig, max_connections: u32) -> StoreResult<Self> {
        if !(MIN_POOL_SIZE..=MAX_POOL_SIZE).contains(&max_connections) {
            return Err(StoreError::InvalidInput(format!(
                "pool size {max_connections} is out of range ({MIN_POOL_SIZE} to {MAX_POOL_SIZE}): \
                 a one-connection pool cannot serve the notify listener and a query at the same time"
            )));
        }
        let pool = config.pool(max_connections as usize)?;
        Ok(Self::from_pool(pool))
    }

    /// Open a connection now, to learn whether the database answers.
    pub async fn ping(&self) -> StoreResult<()> {
        let client = self.pool.get().await.map_err(classify_pool)?;
        let stmt = client.prepare_cached("SELECT 1").await.map_err(classify)?;
        client.query_one(&stmt, &[]).await.map_err(classify)?;
        Ok(())
    }

    /// Use an existing pool, e.g. the one your app already has.
    ///
    /// Sizing is the caller's: a pool of one connection serializes every store call (see
    /// [`MIN_POOL_SIZE`]).
    pub fn from_pool(pool: Pool) -> Self {
        Self::build(pool, "adam_".to_owned())
    }

    /// Use a different table-name prefix (default `adam_`), e.g. to host
    /// several isolated environments in one database. Only `[a-z0-9_]` is
    /// allowed, starting with a letter or underscore.
    pub fn with_table_prefix(self, prefix: &str) -> StoreResult<Self> {
        let valid = !prefix.is_empty()
            && prefix.len() <= 40
            && prefix
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            && !prefix.starts_with(|c: char| c.is_ascii_digit());
        if !valid {
            return Err(StoreError::InvalidInput(format!(
                "invalid table prefix {prefix:?}"
            )));
        }
        Ok(Self::build(self.pool, prefix.to_owned()))
    }

    /// The pool this store uses, e.g. to build the notification publisher on it.
    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    fn build(pool: Pool, prefix: String) -> Self {
        let sql = Sql::new(&prefix);
        Self { pool, sql }
    }

    /// The migration proper, on the connection that holds the session advisory lock.
    ///
    /// Steady-state starts take no table lock (even a no-op `ALTER TABLE` or `CREATE INDEX IF NOT
    /// EXISTS` does), which is why a start cannot deadlock with live traffic: a current schema
    /// returns before any DDL.
    async fn migrate_locked(&self, client: &mut ClientWrapper) -> StoreResult<()> {
        let stmt = client
            .prepare_cached("SELECT to_regclass($1) IS NOT NULL")
            .await
            .map_err(classify)?;
        let meta_exists: bool = client
            .query_one(&stmt, &[&self.sql.meta_table])
            .await
            .map_err(classify)
            .and_then(|row| decode_at(&row, 0))?;
        if meta_exists {
            let stmt = client
                .prepare_cached(&self.sql.schema_version)
                .await
                .map_err(classify)?;
            let stored: Option<String> = client
                .query_opt(&stmt, &[])
                .await
                .map_err(classify)?
                .map(|row| decode_at(&row, 0))
                .transpose()?;
            let stored = stored.and_then(|v| v.parse::<i32>().ok());
            if stored >= Some(SCHEMA_VERSION) {
                return Ok(());
            }
            // The listing index is replaced outside the transaction: `CONCURRENTLY` cannot run in
            // one, and the non-concurrent pair would hold `ACCESS EXCLUSIVE` on `runs` for the
            // whole build. A version 4 database already has it.
            if stored.is_none_or(|v| v < 4) {
                self.upgrade_listing_index(client).await?;
            }
            // Schema version 5, part 1: the blob tables. Their own step, so a version 4 start does
            // not re-check the listing index and a version 5 start does neither.
            if stored == Some(4) {
                self.upgrade_blobs(client).await?;
            }
        }
        let tx = client.transaction().await.map_err(classify)?;
        // Serialize concurrent migrations; CREATE .. IF NOT EXISTS alone can still race on the
        // catalog. Re-entrant for this session, which already holds the session lock; a migrator of
        // an older release takes only this lock, so it is excluded too.
        let stmt = tx
            .prepare_cached("SELECT pg_advisory_xact_lock(hashtext($1))")
            .await
            .map_err(classify)?;
        tx.execute(&stmt, &[&self.sql.migrate_lock])
            .await
            .map_err(classify)?;
        for stmt in &self.sql.migrate {
            tx.batch_execute(stmt).await.map_err(classify)?;
        }
        tx.commit().await.map_err(classify)
    }

    /// Schema version 3 -> 4, without blocking traffic on `runs`.
    ///
    /// The version 3 listing index is replaced by [`Sql::listing_index`]: the new one is built with
    /// `CREATE INDEX CONCURRENTLY` and the old one dropped the same way. Both take only
    /// `SHARE UPDATE EXCLUSIVE`, which reads and writes do not conflict with; the non-concurrent
    /// pair took `ACCESS EXCLUSIVE` for the drop and `SHARE` for the build, inside the migration
    /// transaction, so every read and write queued behind the whole build. Neither concurrent
    /// statement can run in a transaction block, which is why this runs before one is opened.
    ///
    /// Idempotent: `IF NOT EXISTS` and the validity check make a crash between the statements
    /// re-run cleanly, and a build that failed leaves an INVALID index, which is dropped and
    /// rebuilt rather than kept. The old index is dropped only after the new one is valid, so a
    /// listing always has one; the new index has the same key columns, and an older release does
    /// not name an index, so its queries keep using it across a rolling upgrade.
    async fn upgrade_listing_index(&self, client: &mut ClientWrapper) -> StoreResult<()> {
        let stmt = client
            .prepare_cached("SELECT to_regclass($1) IS NOT NULL")
            .await
            .map_err(classify)?;
        let runs: bool = client
            .query_one(&stmt, &[&self.sql.runs_table])
            .await
            .map_err(classify)
            .and_then(|row| decode_at(&row, 0))?;
        if !runs {
            // A fresh database: the transaction below creates the table and the index.
            return Ok(());
        }
        let stmt = client
            .prepare_cached(&self.sql.listing_index_state)
            .await
            .map_err(classify)?;
        let valid: Option<bool> = client
            .query_opt(&stmt, &[&self.sql.listing_index])
            .await
            .map_err(classify)?
            .map(|row| decode_at(&row, 0))
            .transpose()?;
        if valid == Some(false) {
            // A concurrent build that failed leaves an INVALID index, and `IF NOT EXISTS` would
            // keep it for ever: drop it and build again.
            client
                .batch_execute(&self.sql.drop_listing_index)
                .await
                .map_err(classify)?;
        }
        if valid != Some(true) {
            client
                .batch_execute(&self.sql.create_listing_index)
                .await
                .map_err(classify)?;
        }
        client
            .batch_execute(&self.sql.drop_v3_listing_index)
            .await
            .map_err(classify)?;
        Ok(())
    }

    /// Schema version 4 -> 5, part 1: the blob tables.
    ///
    /// Metadata-only: two new tables, their indexes and a foreign key from a table that is empty on
    /// a version 4 database. The only lock it takes on `runs` is the `SHARE ROW EXCLUSIVE` a new
    /// foreign key holds for the length of this transaction; no row is rewritten. Its own step,
    /// gated on `stored == Some(4)`, so a version 4 start neither re-checks the version 4 listing
    /// index nor pays for its `CONCURRENTLY` inspection.
    ///
    /// Idempotent: `IF NOT EXISTS` everywhere, so a crash between statements re-runs cleanly.
    async fn upgrade_blobs(&self, client: &mut ClientWrapper) -> StoreResult<()> {
        let tx = client.transaction().await.map_err(classify)?;
        // The same advisory lock the migration transaction takes: re-entrant for this session,
        // which holds the session lock already, and it still excludes a migrator of another
        // release.
        let stmt = tx
            .prepare_cached("SELECT pg_advisory_xact_lock(hashtext($1))")
            .await
            .map_err(classify)?;
        tx.execute(&stmt, &[&self.sql.migrate_lock])
            .await
            .map_err(classify)?;
        for stmt in &self.sql.migrate_blobs {
            tx.batch_execute(stmt).await.map_err(classify)?;
        }
        tx.commit().await.map_err(classify)
    }

    fn map_write_err(
        &self,
        err: tokio_postgres::Error,
        run: RunId,
        agent: &str,
        conversation: Option<&str>,
    ) -> StoreError {
        if let Some(db) = err.as_db_error() {
            let code = db.code().code();
            let constraint = db.constraint().unwrap_or_default();
            if code == "23505" {
                if constraint == self.sql.open_conversation_index {
                    return StoreError::ConversationBusy {
                        agent: agent.to_owned(),
                        conversation_id: conversation.unwrap_or_default().to_owned(),
                    };
                }
                if constraint == self.sql.runs_pkey {
                    return StoreError::AlreadyExists(run);
                }
            }
            if code == "23503" {
                return StoreError::NotFound(run);
            }
            if matches!(code, "22P05" | "22021") {
                return StoreError::InvalidInput(
                    "PostgreSQL cannot store a character of this value (a NUL in JSONB, or a \
                     character the database encoding cannot hold): a NUL is a bug, since \
                     adam_core::storable replaces it before every write"
                        .into(),
                );
            }
        }
        classify(err)
    }
}

/// `ADAM_PG_POOL_SIZE`: the connections one process's pool may open, [`MIN_POOL_SIZE`] to
/// [`MAX_POOL_SIZE`].
///
/// Unset means [`DEFAULT_POOL_SIZE`]. The pool is **per process**: a deployment of N replicas opens
/// up to N × this many connections. A value that is not an integer in range is
/// [`StoreError::InvalidInput`], not a silent fallback: a typo must not change capacity without
/// saying so.
pub fn pool_size_from_env() -> StoreResult<u32> {
    match std::env::var("ADAM_PG_POOL_SIZE") {
        Ok(raw) => pool_size_from_text(&raw),
        Err(std::env::VarError::NotPresent) => Ok(DEFAULT_POOL_SIZE),
        Err(std::env::VarError::NotUnicode(_)) => Err(StoreError::InvalidInput(
            "ADAM_PG_POOL_SIZE is not valid unicode".into(),
        )),
    }
}

/// [`pool_size_from_env`]'s rule for one value.
fn pool_size_from_text(raw: &str) -> StoreResult<u32> {
    let size: u32 = raw.trim().parse().map_err(|_| {
        StoreError::InvalidInput(format!(
            "ADAM_PG_POOL_SIZE {raw:?} is not an integer ({MIN_POOL_SIZE} to {MAX_POOL_SIZE})"
        ))
    })?;
    if !(MIN_POOL_SIZE..=MAX_POOL_SIZE).contains(&size) {
        return Err(StoreError::InvalidInput(format!(
            "ADAM_PG_POOL_SIZE {raw:?} is out of range ({MIN_POOL_SIZE} to {MAX_POOL_SIZE}): a \
             one-connection pool cannot serve the notify listener and a query at the same time"
        )));
    }
    Ok(size)
}

/// SQL text with table names baked in, built once per store.
#[derive(Clone, Debug)]
struct Sql {
    runs_pkey: String,
    open_conversation_index: String,
    migrate: Vec<Arc<str>>,
    /// Schema version 5 part 1's DDL: the blob tables, run on their own in `upgrade_blobs`.
    migrate_blobs: Vec<Arc<str>>,
    /// The meta table's name with the prefix, bound to `to_regclass` before it is read.
    meta_table: String,
    /// The runs table's name with the prefix, checked before the listing index is built.
    runs_table: String,
    /// The advisory-lock key every migrator of this prefix takes.
    migrate_lock: String,
    /// The listing index (schema version 4) and the SQL that builds, drops and inspects it.
    listing_index: String,
    listing_index_state: Arc<str>,
    create_listing_index: Arc<str>,
    drop_listing_index: Arc<str>,
    drop_v3_listing_index: Arc<str>,
    schema_version: Arc<str>,
    insert_run: Arc<str>,
    load_run: Arc<str>,
    run_header: Arc<str>,
    load_version: Arc<str>,
    commit_run: Arc<str>,
    open_run: Arc<str>,
    journal_get: Arc<str>,
    journal_insert: Arc<str>,
    journal_list: Arc<str>,
    claim_due_any: Arc<str>,
    claim_due_pinned: Arc<str>,
    renew_lease: Arc<str>,
    release_lease: Arc<str>,
    lease_until: Arc<str>,
    purge: Arc<str>,
    list_prefix: Arc<str>,
    list_exact: Arc<str>,
    count_prefix: Arc<str>,
    count_exact: Arc<str>,
    push_put: Arc<str>,
    push_list: Arc<str>,
    push_delete: Arc<str>,
    push_claim: Arc<str>,
    push_commit: Arc<str>,
    push_version: Arc<str>,
    put_blob: Arc<str>,
    get_blob: Arc<str>,
    blob_exists: Arc<str>,
    commit_blobs: Arc<str>,
    purge_blobs: Arc<str>,
}

impl Sql {
    fn new(p: &str) -> Self {
        let runs = format!("{p}runs");
        let journal = format!("{p}journal");
        let meta = format!("{p}meta");
        let push = format!("{p}push");
        let runs_pkey = format!("{p}runs_pkey");
        let open_conversation_index = format!("{p}runs_open_conversation");
        // Schema version 4's listing index, and the name schema version 3 gave it. The new name is
        // what lets the upgrade build it beside the old one and drop that one only afterwards.
        let runs_list = format!("{p}runs_list_status");
        let runs_list_v3 = format!("{p}runs_list");
        let statuses = "'runnable', 'parked', 'done', 'failed'";
        // Schema version 5, part 1: file bytes out of the run. `adam_blob` is global and
        // content-addressed; `adam_run_blob` is the append-only set of digests a run references, and
        // cascades with it. Both are also in `migrate`, for a fresh database; `upgrade_blobs` runs
        // this list on its own for a version 4 one.
        let blob = format!("{p}blob");
        let run_blob = format!("{p}run_blob");
        let migrate_blobs: Vec<Arc<str>> = [
            format!(
                "CREATE TABLE IF NOT EXISTS {blob} (
                    digest      TEXT        NOT NULL CHECK (digest ~ '^[0-9a-f]{{64}}$'),
                    bytes       BYTEA       NOT NULL,
                    size        BIGINT      NOT NULL CHECK (size = octet_length(bytes)),
                    created_at  TIMESTAMPTZ NOT NULL,
                    CONSTRAINT {p}blob_pkey PRIMARY KEY (digest)
                )"
            ),
            format!("CREATE INDEX IF NOT EXISTS {p}blob_gc ON {blob} (created_at)"),
            format!(
                "CREATE TABLE IF NOT EXISTS {run_blob} (
                    run_id  UUID NOT NULL REFERENCES {runs} (id) ON DELETE CASCADE,
                    digest  TEXT NOT NULL,
                    CONSTRAINT {p}run_blob_pkey PRIMARY KEY (run_id, digest)
                )"
            ),
            // The GC asks "does any run still reference this digest"; the primary key is
            // `(run_id, digest)`, so it needs its own index.
            format!("CREATE INDEX IF NOT EXISTS {p}run_blob_digest ON {run_blob} (digest)"),
        ]
        .into_iter()
        .map(Arc::from)
        .collect();
        let mut migrate: Vec<Arc<str>> = [
            format!(
                "CREATE TABLE IF NOT EXISTS {meta} (
                    key   TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                )"
            ),
            format!(
                "CREATE TABLE IF NOT EXISTS {runs} (
                    id              UUID        NOT NULL,
                    agent           TEXT        NOT NULL,
                    conversation_id TEXT,
                    parent_id       UUID,
                    status          TEXT        NOT NULL CHECK (status IN ({statuses})),
                    state           JSONB       NOT NULL,
                    wake_at         TIMESTAMPTZ,
                    sched_at        TIMESTAMPTZ,
                    version         BIGINT      NOT NULL CHECK (version > 0),
                    lease_owner     TEXT,
                    lease_until     TIMESTAMPTZ,
                    owner           TEXT,
                    created_at      TIMESTAMPTZ NOT NULL,
                    updated_at      TIMESTAMPTZ NOT NULL,
                    CONSTRAINT {runs_pkey} PRIMARY KEY (id)
                )"
            ),
            // Schema version 2 -> 3: A2A push-notification configurations. Created before the
            // statements below that lock `runs` exclusively, so a migration takes its locks in the
            // order a `push_put` does (the push table, then the run it references) and the two
            // cannot deadlock.
            format!(
                "CREATE TABLE IF NOT EXISTS {push} (
                    run_id          UUID        NOT NULL REFERENCES {runs} (id) ON DELETE CASCADE,
                    id              TEXT        NOT NULL,
                    agent           TEXT        NOT NULL,
                    owner           TEXT        NOT NULL,
                    config          JSONB       NOT NULL,
                    cursor          JSONB       NOT NULL,
                    state           TEXT        NOT NULL CHECK (state IN ('active', 'done', 'gave_up')),
                    attempts        INTEGER     NOT NULL CHECK (attempts >= 0),
                    last_error      TEXT,
                    next_attempt_at TIMESTAMPTZ NOT NULL,
                    version         BIGINT      NOT NULL CHECK (version > 0),
                    lease_owner     TEXT,
                    lease_until     TIMESTAMPTZ,
                    created_at      TIMESTAMPTZ NOT NULL,
                    updated_at      TIMESTAMPTZ NOT NULL,
                    PRIMARY KEY (run_id, id)
                )"
            ),
            // Claiming: due active configs per agent, earliest first.
            format!(
                "CREATE INDEX IF NOT EXISTS {p}push_due ON {push} (agent, next_attempt_at, run_id, id)
                 WHERE state = 'active'"
            ),
            // Schema version 1 -> 2: tables made by version 1 have no owner column.
            format!("ALTER TABLE {runs} ADD COLUMN IF NOT EXISTS owner TEXT"),
            // Claiming: due runs per agent, earliest first.
            format!(
                "CREATE INDEX IF NOT EXISTS {p}runs_due ON {runs} (agent, sched_at, id)
                 WHERE sched_at IS NOT NULL"
            ),
            // At most one open run per conversation, enforced by the database.
            format!(
                "CREATE UNIQUE INDEX IF NOT EXISTS {open_conversation_index}
                 ON {runs} (agent, conversation_id)
                 WHERE conversation_id IS NOT NULL AND status IN ('runnable', 'parked')"
            ),
            // Retention sweeps.
            format!(
                "CREATE INDEX IF NOT EXISTS {p}runs_finished ON {runs} (agent, updated_at)
                 WHERE status IN ('done', 'failed')"
            ),
            format!("CREATE INDEX IF NOT EXISTS {p}runs_parent ON {runs} (parent_id) WHERE parent_id IS NOT NULL"),
            // Listing an owner's runs, newest first (`ListTasks`): the conversation id is compared
            // in the "C" collation, so that a prefix is a range the index can serve. `status` is
            // included so a status filter is answered from the index. A fresh database gets it
            // here; a version 3 database gets it from `upgrade_listing_index`, which builds it
            // concurrently under this name and drops the version 3 index only afterwards.
            format!(
                "CREATE INDEX IF NOT EXISTS {runs_list}
                 ON {runs} (agent, (conversation_id COLLATE \"C\"), updated_at DESC, id DESC)
                 INCLUDE (status)
                 WHERE conversation_id IS NOT NULL"
            ),
            format!(
                "CREATE TABLE IF NOT EXISTS {journal} (
                    run_id      UUID        NOT NULL REFERENCES {runs} (id) ON DELETE CASCADE,
                    seq         BIGINT      NOT NULL CHECK (seq >= 0),
                    name        TEXT        NOT NULL,
                    ok          BOOLEAN     NOT NULL,
                    payload     JSONB       NOT NULL,
                    recorded_at TIMESTAMPTZ NOT NULL,
                    PRIMARY KEY (run_id, seq)
                )"
            ),
        ]
        .into_iter()
        .map(Arc::from)
        .collect();
        migrate.extend(migrate_blobs.iter().cloned());
        // Record the version, and raise it from an older one. Never lower it: a process
        // running an older release must not undo a newer one's migration.
        migrate.push(Arc::from(format!(
            "INSERT INTO {meta} (key, value) VALUES ('schema_version', '{SCHEMA_VERSION}')
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value
              WHERE {meta}.value::INT < EXCLUDED.value::INT"
        )));
        Self {
            migrate,
            migrate_blobs,
            meta_table: meta.clone(),
            runs_table: runs.clone(),
            migrate_lock: format!("adam-rs:migrate:{p}"),
            listing_index: runs_list.clone(),
            // `to_regclass` resolves the unqualified name through the search path, like every
            // other statement here, so two deployments that share a database with the same prefix
            // in different schemas each inspect their own index (a `pg_class.relname` match would
            // read whichever came first).
            listing_index_state: Arc::from(
                "SELECT indisvalid FROM pg_index WHERE indexrelid = to_regclass($1)",
            ),
            create_listing_index: Arc::from(format!(
                "CREATE INDEX CONCURRENTLY IF NOT EXISTS {runs_list}
                 ON {runs} (agent, (conversation_id COLLATE \"C\"), updated_at DESC, id DESC)
                 INCLUDE (status)
                 WHERE conversation_id IS NOT NULL"
            )),
            drop_listing_index: Arc::from(format!("DROP INDEX CONCURRENTLY IF EXISTS {runs_list}")),
            drop_v3_listing_index: Arc::from(format!(
                "DROP INDEX CONCURRENTLY IF EXISTS {runs_list_v3}"
            )),
            schema_version: Arc::from(format!(
                "SELECT value FROM {meta} WHERE key = 'schema_version'"
            )),
            insert_run: Arc::from(format!(
                "INSERT INTO {runs} (id, agent, conversation_id, parent_id, status, state, wake_at,
                                     sched_at, version, created_at, updated_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 1, $9, $9)
                 RETURNING {RUN_METADATA_COLUMNS}"
            )),
            load_run: Arc::from(format!("SELECT {RUN_COLUMNS} FROM {runs} WHERE id = $1")),
            run_header: Arc::from(format!(
                "SELECT {RUN_HEADER_COLUMNS} FROM {runs} WHERE id = $1"
            )),
            load_version: Arc::from(format!("SELECT version FROM {runs} WHERE id = $1")),
            commit_run: Arc::from(format!(
                "UPDATE {runs}
                    SET status = $3, state = $4, wake_at = $5, sched_at = $6,
                        version = version + 1, updated_at = $7
                  WHERE id = $1 AND version = $2
                 RETURNING {RUN_METADATA_COLUMNS}"
            )),
            open_run: Arc::from(format!(
                "SELECT {RUN_COLUMNS} FROM {runs}
                  WHERE agent = $1 AND conversation_id = $2 AND status IN ('runnable', 'parked')"
            )),
            journal_get: Arc::from(format!(
                "SELECT seq, name, ok, payload, recorded_at FROM {journal} WHERE run_id = $1 AND seq = $2"
            )),
            journal_insert: Arc::from(format!(
                "INSERT INTO {journal} (run_id, seq, name, ok, payload, recorded_at)
                 VALUES ($1, $2, $3, $4, $5, $6)
                 ON CONFLICT (run_id, seq) DO NOTHING
                 RETURNING seq, name, ok, payload, recorded_at"
            )),
            journal_list: Arc::from(format!(
                "SELECT seq, name, ok, payload, recorded_at FROM {journal} WHERE run_id = $1 ORDER BY seq"
            )),
            claim_due_any: Arc::from(claim_due_sql(&runs, ClaimScope::Any)),
            claim_due_pinned: Arc::from(claim_due_sql(&runs, ClaimScope::Pinned)),
            renew_lease: Arc::from(format!(
                "UPDATE {runs} SET lease_until = $4
                  WHERE id = $1 AND lease_owner = $2 AND lease_until > $3"
            )),
            release_lease: Arc::from(format!(
                "UPDATE {runs} SET lease_owner = NULL, lease_until = NULL
                  WHERE id = $1 AND lease_owner = $2"
            )),
            lease_until: Arc::from(format!("SELECT lease_until FROM {runs} WHERE id = $1")),
            // Batched so a large backlog never becomes one huge transaction.
            // SKIP LOCKED lets concurrent sweepers share the work.
            purge: Arc::from(format!(
                "DELETE FROM {runs}
                  WHERE id IN (
                    SELECT id FROM {runs}
                     WHERE agent = $1 AND status IN ('done', 'failed') AND updated_at < $2
                     LIMIT $3
                     FOR UPDATE SKIP LOCKED
                  )"
            )),
            list_prefix: Arc::from(list_sql(&runs, Scope::Prefix)),
            list_exact: Arc::from(list_sql(&runs, Scope::Exact)),
            count_prefix: Arc::from(count_sql(&runs, Scope::Prefix)),
            count_exact: Arc::from(count_sql(&runs, Scope::Exact)),
            push_put: Arc::from(format!(
                "INSERT INTO {push} (run_id, id, agent, owner, config, cursor, state, attempts,
                                    last_error, next_attempt_at, version, created_at, updated_at)
                 VALUES ($1, $2, $3, $4, $5, $6, 'active', 0, NULL, $7, 1, $7, $7)
                 ON CONFLICT (run_id, id) DO UPDATE
                    SET agent = EXCLUDED.agent, owner = EXCLUDED.owner, config = EXCLUDED.config,
                        cursor = EXCLUDED.cursor, state = 'active', attempts = 0, last_error = NULL,
                        next_attempt_at = EXCLUDED.next_attempt_at, lease_owner = NULL,
                        lease_until = NULL, version = {push}.version + 1,
                        updated_at = EXCLUDED.updated_at
                 RETURNING {PUSH_COLUMNS}"
            )),
            push_list: Arc::from(format!(
                "SELECT {PUSH_COLUMNS} FROM {push} WHERE run_id = $1 ORDER BY id"
            )),
            push_delete: Arc::from(format!("DELETE FROM {push} WHERE run_id = $1 AND id = $2")),
            push_claim: Arc::from(format!(
                "WITH due AS (
                    SELECT run_id, id FROM {push}
                     WHERE agent = ANY($1) AND state = 'active' AND next_attempt_at <= $2
                       AND (lease_until IS NULL OR lease_until <= $2)
                     ORDER BY next_attempt_at, run_id, id
                     LIMIT $3
                     FOR UPDATE SKIP LOCKED
                 )
                 UPDATE {push} p SET lease_owner = $4, lease_until = $5
                   FROM due
                  WHERE p.run_id = due.run_id AND p.id = due.id
                 RETURNING p.run_id, p.id, p.agent, p.owner, p.config, p.cursor, p.state,
                           p.attempts, p.last_error, p.next_attempt_at, p.version, p.created_at,
                           p.updated_at"
            )),
            push_commit: Arc::from(format!(
                "UPDATE {push}
                    SET state = $4, cursor = $5, attempts = $6, last_error = $7,
                        next_attempt_at = $8, version = version + 1, updated_at = $9,
                        lease_owner = NULL, lease_until = NULL
                  WHERE run_id = $1 AND id = $2 AND version = $3
                 RETURNING {PUSH_COLUMNS}"
            )),
            push_version: Arc::from(format!(
                "SELECT version FROM {push} WHERE run_id = $1 AND id = $2"
            )),
            put_blob: Arc::from(format!(
                "INSERT INTO {blob} (digest, bytes, size, created_at)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (digest) DO NOTHING"
            )),
            get_blob: Arc::from(format!("SELECT bytes FROM {blob} WHERE digest = $1")),
            blob_exists: Arc::from(format!(
                "SELECT EXISTS(SELECT 1 FROM {blob} WHERE digest = $1)"
            )),
            // The refs of one commit. `ON CONFLICT DO NOTHING` makes a retried commit (or two
            // artifacts with the same bytes) one no-op per digest already referenced.
            commit_blobs: Arc::from(format!(
                "INSERT INTO {run_blob} (run_id, digest)
                 SELECT $1, digest FROM unnest($2::text[]) AS digest
                 ON CONFLICT (run_id, digest) DO NOTHING"
            )),
            // Unreferenced blobs older than the cutoff, in batches so a backlog never becomes one
            // huge transaction. `NOT EXISTS` on the digest index is the whole question.
            purge_blobs: Arc::from(format!(
                "DELETE FROM {blob}
                  WHERE digest IN (
                    SELECT b.digest FROM {blob} b
                     WHERE b.created_at < $1
                       AND NOT EXISTS (SELECT 1 FROM {run_blob} r WHERE r.digest = b.digest)
                     LIMIT $2
                  )"
            )),
            runs_pkey,
            open_conversation_index,
        }
    }
}

/// The claiming statement. `$1` agents, `$2` now, `$3` limit, `$4` worker, `$5` lease end, `$6` the
/// runs the caller is stepping, which are never claimed. The pinned form filters on `owner` and
/// sets it (`COALESCE` keeps an existing owner).
fn claim_due_sql(runs: &str, scope: ClaimScope) -> String {
    let (filter, set_owner) = match scope {
        ClaimScope::Any => ("", ""),
        ClaimScope::Pinned => (
            "AND (owner IS NULL OR owner = $4)",
            ", owner = COALESCE(r.owner, $4)",
        ),
    };
    format!(
        "WITH due AS (
            SELECT id FROM {runs}
             WHERE agent = ANY($1)
               AND id <> ALL($6)
               AND sched_at <= $2
               AND (lease_until IS NULL OR lease_until <= $2)
               {filter}
             ORDER BY sched_at, id
             LIMIT $3
             FOR UPDATE SKIP LOCKED
         )
         UPDATE {runs} r
            SET lease_owner = $4, lease_until = $5{set_owner}
           FROM due
          WHERE r.id = due.id
         RETURNING r.id, r.agent, r.conversation_id, r.parent_id, r.status, r.state, r.wake_at,
                   r.version, r.created_at, r.updated_at, r.sched_at"
    )
}

#[derive(Clone, Copy)]
enum Scope {
    Prefix,
    Exact,
}

/// The listing statement. `$1` agent, `$2` statuses (or `NULL`), `$3` updated since (or `NULL`),
/// `$4` and `$5` the position to continue after (or `NULL`), `$6` the limit, `$7` the scope (the
/// prefix, or the conversation) and `$8` the first text above every text that starts with the
/// prefix (`NULL` for an exact scope). The prefix is a range over the index **and** an exact
/// `starts_with`, so the range may be wide but never wrong.
fn list_sql(runs: &str, scope: Scope) -> String {
    format!(
        "SELECT {RUN_COLUMNS} FROM {runs}
          WHERE agent = $1
            AND {}
            AND ($2::text[] IS NULL OR status = ANY($2))
            AND ($3::timestamptz IS NULL OR updated_at >= $3)
            AND ($4::timestamptz IS NULL OR (updated_at, id) < ($4, $5::uuid))
          ORDER BY updated_at DESC, id DESC
          LIMIT $6",
        scope_sql(scope, "$7", "$8")
    )
}

/// The counting statement: `$1` agent, `$2` statuses, `$3` updated since, `$4` the scope, `$5` the
/// bound of a prefix (as in [`list_sql`], without the position and the limit).
fn count_sql(runs: &str, scope: Scope) -> String {
    format!(
        "SELECT count(*) FROM {runs}
          WHERE agent = $1
            AND {}
            AND ($2::text[] IS NULL OR status = ANY($2))
            AND ($3::timestamptz IS NULL OR updated_at >= $3)",
        scope_sql(scope, "$4", "$5")
    )
}

fn scope_sql(scope: Scope, text: &str, bound: &str) -> String {
    match scope {
        Scope::Prefix => format!(
            "conversation_id COLLATE \"C\" >= {text} AND conversation_id COLLATE \"C\" < {bound}
             AND starts_with(conversation_id, {text})"
        ),
        Scope::Exact => format!("conversation_id COLLATE \"C\" = {text} AND {bound}::text IS NULL"),
    }
}

/// The first text above every text that starts with `prefix` in the "C" collation (byte order):
/// the prefix and the highest code point. Any text with the prefix is below it, and the
/// `starts_with` of the statement makes the bound's looseness harmless.
fn prefix_end(prefix: &str) -> String {
    let mut end = prefix.to_owned();
    end.push(char::MAX);
    end
}

fn to_i64(n: u64, what: &str) -> StoreResult<i64> {
    i64::try_from(n).map_err(|_| StoreError::InvalidInput(format!("{what} {n} exceeds i64::MAX")))
}

/// A [`Timestamp`] as the driver binds it: `postgres-types`' `with-jiff-0_2` maps
/// `jiff::Timestamp` to `TIMESTAMPTZ` (microseconds since 2000, the bytes chrono wrote), so the
/// store converts at the edge instead of carrying a bridge type of its own.
fn sql_time(t: Timestamp) -> jiff::Timestamp {
    t.0
}

/// One column of a row, by name. A failure here is stored data that breaks an invariant (or a
/// schema the store does not expect): `Corrupt`, never retried for ever.
fn decode<'a, T: FromSql<'a>>(row: &'a Row, column: &str) -> StoreResult<T> {
    row.try_get(column).map_err(corrupt_source)
}

/// One column of a row, by position (an expression without a name).
fn decode_at<'a, T: FromSql<'a>>(row: &'a Row, index: usize) -> StoreResult<T> {
    row.try_get(index).map_err(corrupt_source)
}

fn corrupt_source(err: tokio_postgres::Error) -> StoreError {
    StoreError::Backend {
        class: ErrorClass::Corrupt,
        source: Box::new(err),
    }
}

fn run_from_row(row: &Row) -> StoreResult<RunRecord> {
    let mut run = run_metadata_from_row(row)?;
    run.state = decode(row, "state")?;
    Ok(run)
}

/// A row of [`RUN_HEADER_COLUMNS`]: the record's metadata and its lease, never `state`.
fn run_header_from_row(row: &Row) -> StoreResult<RunHeader> {
    let status: String = decode(row, "status")?;
    let version: i64 = decode(row, "version")?;
    Ok(RunHeader {
        id: RunId(decode(row, "id")?),
        agent: decode(row, "agent")?,
        conversation_id: decode(row, "conversation_id")?,
        parent_id: decode::<Option<Uuid>>(row, "parent_id")?.map(RunId),
        status: RunStatus::parse(&status)
            .ok_or_else(|| StoreError::Corrupt(format!("unknown run status {status:?}")))?,
        wake_at: opt_time(row, "wake_at")?,
        version: u64::try_from(version)
            .map_err(|_| StoreError::Corrupt(format!("negative version {version}")))?,
        created_at: time(row, "created_at")?,
        updated_at: time(row, "updated_at")?,
        lease_until: opt_time(row, "lease_until")?,
    })
}

/// A row of [`RUN_METADATA_COLUMNS`]: the record a write returns, whose `state` is
/// [`Value::Null`].
fn run_metadata_from_row(row: &Row) -> StoreResult<RunRecord> {
    let status: String = decode(row, "status")?;
    let version: i64 = decode(row, "version")?;
    Ok(RunRecord {
        id: RunId(decode(row, "id")?),
        agent: decode(row, "agent")?,
        conversation_id: decode(row, "conversation_id")?,
        parent_id: decode::<Option<Uuid>>(row, "parent_id")?.map(RunId),
        status: RunStatus::parse(&status)
            .ok_or_else(|| StoreError::Corrupt(format!("unknown run status {status:?}")))?,
        state: Value::Null,
        wake_at: opt_time(row, "wake_at")?,
        version: u64::try_from(version)
            .map_err(|_| StoreError::Corrupt(format!("negative version {version}")))?,
        created_at: time(row, "created_at")?,
        updated_at: time(row, "updated_at")?,
    })
}

fn time(row: &Row, column: &str) -> StoreResult<Timestamp> {
    decode::<jiff::Timestamp>(row, column).map(Timestamp)
}

fn opt_time(row: &Row, column: &str) -> StoreResult<Option<Timestamp>> {
    decode::<Option<jiff::Timestamp>>(row, column).map(|at| at.map(Timestamp))
}

fn push_from_row(row: &Row) -> StoreResult<PushRecord> {
    let state: String = decode(row, "state")?;
    let version: i64 = decode(row, "version")?;
    let attempts: i32 = decode(row, "attempts")?;
    Ok(PushRecord {
        run: RunId(decode(row, "run_id")?),
        id: decode(row, "id")?,
        agent: decode(row, "agent")?,
        owner: decode(row, "owner")?,
        config: decode(row, "config")?,
        cursor: decode(row, "cursor")?,
        state: PushState::parse(&state)
            .ok_or_else(|| StoreError::Corrupt(format!("unknown push state {state:?}")))?,
        attempts: u32::try_from(attempts)
            .map_err(|_| StoreError::Corrupt(format!("negative attempts {attempts}")))?,
        last_error: decode(row, "last_error")?,
        next_attempt_at: time(row, "next_attempt_at")?,
        version: u64::try_from(version)
            .map_err(|_| StoreError::Corrupt(format!("negative version {version}")))?,
        created_at: time(row, "created_at")?,
        updated_at: time(row, "updated_at")?,
    })
}

fn entry_from_row(row: &Row) -> StoreResult<JournalEntry> {
    let seq: i64 = decode(row, "seq")?;
    Ok(JournalEntry {
        seq: u64::try_from(seq).map_err(|_| StoreError::Corrupt(format!("negative seq {seq}")))?,
        name: decode(row, "name")?,
        ok: decode(row, "ok")?,
        payload: decode(row, "payload")?,
        recorded_at: time(row, "recorded_at")?,
    })
}

#[async_trait]
impl Store for PgStore {
    async fn migrate(&self) -> StoreResult<()> {
        // One connection for the whole migration: the listing-index upgrade must run outside a
        // transaction, so the migration takes a session-level advisory lock on this connection and
        // opens its transaction on it too. The transaction's own `pg_advisory_xact_lock` is
        // re-entrant for this session and still excludes a migrator of an older release, which
        // takes only that lock.
        let object = self.pool.get().await.map_err(classify_pool)?;
        // A session lock lives as long as its connection. Take the connection out of the pool
        // rather than returning it, so an error or a panic cannot leave the lock held on a
        // connection another task would reuse; dropping it closes the connection and releases the
        // lock.
        let mut client = Object::take(object);
        let stmt = client
            .prepare_cached("SELECT pg_advisory_lock(hashtext($1))")
            .await
            .map_err(classify)?;
        client
            .execute(&stmt, &[&self.sql.migrate_lock])
            .await
            .map_err(classify)?;
        self.migrate_locked(&mut client).await
    }

    async fn create_run(&self, new: NewRun) -> StoreResult<RunRecord> {
        let t = now();
        let wake_at = new.wake_at.map(truncate_ms);
        let parent = new.parent_id.map(|p| p.0);
        let sched = sched_at(new.status, wake_at, t);
        let (t, wake_at, sched) = (sql_time(t), wake_at.map(sql_time), sched.map(sql_time));
        let status = new.status.as_str();
        let state = storable(&new.state);
        let params: &[&(dyn ToSql + Sync)] = &[
            &new.id.0,
            &new.agent,
            &new.conversation_id,
            &parent,
            &status,
            state.as_ref(),
            &wake_at,
            &sched,
            &t,
        ];
        let client = self.pool.get().await.map_err(classify_pool)?;
        let stmt = client
            .prepare_cached(&self.sql.insert_run)
            .await
            .map_err(classify)?;
        let row = client.query_one(&stmt, params).await.map_err(|e| {
            self.map_write_err(e, new.id, &new.agent, new.conversation_id.as_deref())
        })?;
        run_metadata_from_row(&row)
    }

    async fn load_run(&self, id: RunId) -> StoreResult<Option<RunRecord>> {
        let client = self.pool.get().await.map_err(classify_pool)?;
        let stmt = client
            .prepare_cached(&self.sql.load_run)
            .await
            .map_err(classify)?;
        client
            .query_opt(&stmt, &[&id.0])
            .await
            .map_err(classify)?
            .as_ref()
            .map(run_from_row)
            .transpose()
    }

    async fn run_header(&self, id: RunId) -> StoreResult<Option<RunHeader>> {
        // One statement over the primary key: the header columns and the lease, never `state`.
        let client = self.pool.get().await.map_err(classify_pool)?;
        let stmt = client
            .prepare_cached(&self.sql.run_header)
            .await
            .map_err(classify)?;
        client
            .query_opt(&stmt, &[&id.0])
            .await
            .map_err(classify)?
            .as_ref()
            .map(run_header_from_row)
            .transpose()
    }

    async fn commit_run(
        &self,
        id: RunId,
        expected: u64,
        update: RunUpdate,
    ) -> StoreResult<RunRecord> {
        let t = now();
        let expected = to_i64(expected, "version")?;
        let RunUpdate {
            status,
            state,
            head: _,
            body: _,
            blobs,
            wake_at,
        } = update;
        let wake_at = wake_at.map(truncate_ms);
        let sched = sched_at(status, wake_at, t);
        let (t, wake_at, sched) = (sql_time(t), wake_at.map(sql_time), sched.map(sql_time));
        let status = status.as_str();
        let state = storable(&state);
        let params: &[&(dyn ToSql + Sync)] = &[
            &id.0,
            &expected,
            &status,
            state.as_ref(),
            &wake_at,
            &sched,
            &t,
        ];
        // A commit with references is one transaction: a commit that loses the CAS writes no ref,
        // and a commit that wins has all of them. A run with no file bytes keeps the single
        // statement it always was (no BEGIN/COMMIT round trips on the common path).
        if blobs.is_empty() {
            // The connection is given back before a helper that needs another one runs: a pool
            // whose every connection is held by a commit waiting for a second one deadlocks.
            let result = {
                let client = self.pool.get().await.map_err(classify_pool)?;
                let stmt = client
                    .prepare_cached(&self.sql.commit_run)
                    .await
                    .map_err(classify)?;
                client.query_opt(&stmt, params).await
            };
            return match result {
                Ok(row) => self.finish_commit(row, id, expected as u64).await,
                Err(err) => Err(self.commit_error(err, id).await?),
            };
        }
        let result = {
            let mut client = self.pool.get().await.map_err(classify_pool)?;
            let tx = client.transaction().await.map_err(classify)?;
            let stmt = tx
                .prepare_cached(&self.sql.commit_run)
                .await
                .map_err(classify)?;
            match tx.query_opt(&stmt, params).await {
                Ok(None) => {
                    tx.rollback().await.map_err(classify)?;
                    Ok(None)
                }
                Ok(Some(row)) => {
                    // One statement for the set; a digest named twice in one commit is one row.
                    let mut seen = HashSet::new();
                    let digests: Vec<String> = blobs
                        .iter()
                        .filter(|digest| seen.insert(digest.as_str()))
                        .map(|digest| digest.as_str().to_owned())
                        .collect();
                    let stmt = tx
                        .prepare_cached(&self.sql.commit_blobs)
                        .await
                        .map_err(classify)?;
                    tx.execute(&stmt, &[&id.0, &digests])
                        .await
                        .map_err(classify)?;
                    tx.commit().await.map_err(classify)?;
                    Ok(Some(row))
                }
                Err(err) => {
                    let _ = tx.rollback().await;
                    Err(err)
                }
            }
        };
        match result {
            Ok(Some(row)) => run_metadata_from_row(&row),
            Ok(None) => self.commit_conflict(id, expected as u64).await,
            Err(err) => Err(self.commit_error(err, id).await?),
        }
    }

    async fn open_run_for_conversation(
        &self,
        agent: &str,
        conversation_id: &str,
    ) -> StoreResult<Option<RunRecord>> {
        let client = self.pool.get().await.map_err(classify_pool)?;
        let stmt = client
            .prepare_cached(&self.sql.open_run)
            .await
            .map_err(classify)?;
        client
            .query_opt(&stmt, &[&agent, &conversation_id])
            .await
            .map_err(classify)?
            .as_ref()
            .map(run_from_row)
            .transpose()
    }

    async fn journal_get(&self, run: RunId, seq: u64) -> StoreResult<Option<JournalEntry>> {
        let Ok(seq) = i64::try_from(seq) else {
            return Ok(None);
        };
        let client = self.pool.get().await.map_err(classify_pool)?;
        let stmt = client
            .prepare_cached(&self.sql.journal_get)
            .await
            .map_err(classify)?;
        client
            .query_opt(&stmt, &[&run.0, &seq])
            .await
            .map_err(classify)?
            .as_ref()
            .map(entry_from_row)
            .transpose()
    }

    async fn journal_put(&self, run: RunId, entry: JournalEntry) -> StoreResult<JournalEntry> {
        let seq = to_i64(entry.seq, "journal seq")?;
        let payload = storable(&entry.payload);
        let recorded_at = sql_time(truncate_ms(entry.recorded_at));
        let params: &[&(dyn ToSql + Sync)] = &[
            &run.0,
            &seq,
            &entry.name,
            &entry.ok,
            payload.as_ref(),
            &recorded_at,
        ];
        // The connection is given back before `journal_get`, which needs one of its own.
        let inserted = {
            let client = self.pool.get().await.map_err(classify_pool)?;
            let stmt = client
                .prepare_cached(&self.sql.journal_insert)
                .await
                .map_err(classify)?;
            client
                .query_opt(&stmt, params)
                .await
                .map_err(|e| self.map_write_err(e, run, "", None))
        }?;
        if let Some(row) = inserted {
            return entry_from_row(&row);
        }
        // Someone else recorded this step first: theirs is the truth.
        let existing = self.journal_get(run, entry.seq).await?.ok_or_else(|| {
            StoreError::Corrupt(format!("journal entry {run}/{} vanished", entry.seq))
        })?;
        if existing.name != entry.name {
            return Err(StoreError::NonDeterminism {
                run,
                seq: entry.seq,
                recorded: existing.name,
                requested: entry.name,
            });
        }
        Ok(existing)
    }

    async fn journal_list(&self, run: RunId) -> StoreResult<Vec<JournalEntry>> {
        let client = self.pool.get().await.map_err(classify_pool)?;
        let stmt = client
            .prepare_cached(&self.sql.journal_list)
            .await
            .map_err(classify)?;
        let rows = client.query(&stmt, &[&run.0]).await.map_err(classify)?;
        rows.iter().map(entry_from_row).collect()
    }

    async fn claim_due(
        &self,
        agents: &[String],
        worker: &str,
        scope: ClaimScope,
        busy: &[RunId],
        now: Timestamp,
        ttl: Duration,
        limit: usize,
    ) -> StoreResult<Vec<Lease>> {
        if agents.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let now = truncate_ms(now);
        let until = add_ttl(now, ttl);
        let statement = match scope {
            ClaimScope::Any => &self.sql.claim_due_any,
            ClaimScope::Pinned => &self.sql.claim_due_pinned,
        };
        let busy: Vec<Uuid> = busy.iter().map(|id| id.0).collect();
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let (now_sql, until_sql) = (sql_time(now), sql_time(until));
        let params: &[&(dyn ToSql + Sync)] =
            &[&agents, &now_sql, &limit, &worker, &until_sql, &busy];
        let client = self.pool.get().await.map_err(classify_pool)?;
        let stmt = client.prepare_cached(statement).await.map_err(classify)?;
        let rows = client.query(&stmt, params).await.map_err(classify)?;
        // RETURNING order is unspecified; restore the claim order.
        let mut claimed = rows
            .iter()
            .map(|row| {
                let sched = time(row, "sched_at")?;
                Ok((sched, run_from_row(row)?))
            })
            .collect::<StoreResult<Vec<_>>>()?;
        claimed.sort_by(|(a, x), (b, y)| a.cmp(b).then(x.id.cmp(&y.id)));
        Ok(claimed
            .into_iter()
            .map(|(_, run)| Lease {
                run,
                worker: worker.to_owned(),
                until,
            })
            .collect())
    }

    async fn renew_lease(
        &self,
        id: RunId,
        worker: &str,
        now: Timestamp,
        ttl: Duration,
    ) -> StoreResult<bool> {
        let now = truncate_ms(now);
        let until = add_ttl(now, ttl);
        let (now_sql, until_sql) = (sql_time(now), sql_time(until));
        let params: &[&(dyn ToSql + Sync)] = &[&id.0, &worker, &now_sql, &until_sql];
        let client = self.pool.get().await.map_err(classify_pool)?;
        let stmt = client
            .prepare_cached(&self.sql.renew_lease)
            .await
            .map_err(classify)?;
        let done = client.execute(&stmt, params).await.map_err(classify)?;
        Ok(done == 1)
    }

    async fn release_lease(&self, id: RunId, worker: &str) -> StoreResult<()> {
        let client = self.pool.get().await.map_err(classify_pool)?;
        let stmt = client
            .prepare_cached(&self.sql.release_lease)
            .await
            .map_err(classify)?;
        client
            .execute(&stmt, &[&id.0, &worker])
            .await
            .map_err(classify)?;
        Ok(())
    }

    async fn lease_until(&self, id: RunId) -> StoreResult<Option<Timestamp>> {
        let client = self.pool.get().await.map_err(classify_pool)?;
        let stmt = client
            .prepare_cached(&self.sql.lease_until)
            .await
            .map_err(classify)?;
        client
            .query_opt(&stmt, &[&id.0])
            .await
            .map_err(classify)?
            .as_ref()
            .map(|row| opt_time(row, "lease_until"))
            .transpose()
            .map(Option::flatten)
    }

    async fn list_runs(&self, query: &RunQuery) -> StoreResult<Vec<RunRecord>> {
        let (statement, scope, upper) = match &query.scope {
            ConversationScope::Prefix(p) => (&self.sql.list_prefix, p.clone(), Some(prefix_end(p))),
            ConversationScope::Exact(e) => (&self.sql.list_exact, e.clone(), None),
        };
        let statuses: Option<Vec<String>> = query
            .statuses
            .as_ref()
            .map(|s| s.iter().map(|s| s.as_str().to_owned()).collect());
        let updated_since = query.updated_since.map(truncate_ms).map(sql_time);
        let after = query.after.map(|(at, _)| sql_time(truncate_ms(at)));
        let after_id = query.after.map(|(_, id)| id.0);
        let limit = i64::try_from(query.limit).unwrap_or(i64::MAX);
        let params: &[&(dyn ToSql + Sync)] = &[
            &query.agent,
            &statuses,
            &updated_since,
            &after,
            &after_id,
            &limit,
            &scope,
            &upper,
        ];
        let client = self.pool.get().await.map_err(classify_pool)?;
        let stmt = client.prepare_cached(statement).await.map_err(classify)?;
        let rows = client.query(&stmt, params).await.map_err(classify)?;
        rows.iter().map(run_from_row).collect()
    }

    async fn count_runs(&self, query: &RunQuery) -> StoreResult<u64> {
        let (statement, scope, upper) = match &query.scope {
            ConversationScope::Prefix(p) => {
                (&self.sql.count_prefix, p.clone(), Some(prefix_end(p)))
            }
            ConversationScope::Exact(e) => (&self.sql.count_exact, e.clone(), None),
        };
        let statuses: Option<Vec<String>> = query
            .statuses
            .as_ref()
            .map(|s| s.iter().map(|s| s.as_str().to_owned()).collect());
        let updated_since = query.updated_since.map(truncate_ms).map(sql_time);
        let params: &[&(dyn ToSql + Sync)] =
            &[&query.agent, &statuses, &updated_since, &scope, &upper];
        let client = self.pool.get().await.map_err(classify_pool)?;
        let stmt = client.prepare_cached(statement).await.map_err(classify)?;
        let row = client.query_one(&stmt, params).await.map_err(classify)?;
        let count: i64 = decode_at(&row, 0)?;
        u64::try_from(count).map_err(|_| StoreError::Corrupt(format!("negative count {count}")))
    }

    async fn push_put(&self, new: NewPushConfig) -> StoreResult<PushRecord> {
        let config = storable(&new.config);
        let cursor = storable(&new.cursor);
        let created = sql_time(now());
        let params: &[&(dyn ToSql + Sync)] = &[
            &new.run.0,
            &new.id,
            &new.agent,
            &new.owner,
            config.as_ref(),
            cursor.as_ref(),
            &created,
        ];
        let client = self.pool.get().await.map_err(classify_pool)?;
        let stmt = client
            .prepare_cached(&self.sql.push_put)
            .await
            .map_err(classify)?;
        let row = client
            .query_one(&stmt, params)
            .await
            .map_err(|e| self.map_write_err(e, new.run, &new.agent, None))?;
        push_from_row(&row)
    }

    async fn push_list(&self, run: RunId) -> StoreResult<Vec<PushRecord>> {
        let client = self.pool.get().await.map_err(classify_pool)?;
        let stmt = client
            .prepare_cached(&self.sql.push_list)
            .await
            .map_err(classify)?;
        let rows = client.query(&stmt, &[&run.0]).await.map_err(classify)?;
        rows.iter().map(push_from_row).collect()
    }

    async fn push_delete(&self, run: RunId, id: &str) -> StoreResult<bool> {
        let client = self.pool.get().await.map_err(classify_pool)?;
        let stmt = client
            .prepare_cached(&self.sql.push_delete)
            .await
            .map_err(classify)?;
        let done = client
            .execute(&stmt, &[&run.0, &id])
            .await
            .map_err(classify)?;
        Ok(done == 1)
    }

    async fn push_claim_due(
        &self,
        agents: &[String],
        worker: &str,
        now: Timestamp,
        ttl: Duration,
        limit: usize,
    ) -> StoreResult<Vec<PushRecord>> {
        if agents.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let now = truncate_ms(now);
        let until = add_ttl(now, ttl);
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let (now_sql, until_sql) = (sql_time(now), sql_time(until));
        let params: &[&(dyn ToSql + Sync)] = &[&agents, &now_sql, &limit, &worker, &until_sql];
        let client = self.pool.get().await.map_err(classify_pool)?;
        let stmt = client
            .prepare_cached(&self.sql.push_claim)
            .await
            .map_err(classify)?;
        let rows = client.query(&stmt, params).await.map_err(classify)?;
        // RETURNING order is unspecified; restore the claim order.
        let mut claimed = rows
            .iter()
            .map(push_from_row)
            .collect::<StoreResult<Vec<_>>>()?;
        claimed.sort_by(|a, b| {
            (a.next_attempt_at, a.run, &a.id).cmp(&(b.next_attempt_at, b.run, &b.id))
        });
        Ok(claimed)
    }

    async fn push_commit(
        &self,
        run: RunId,
        id: &str,
        expected: u64,
        progress: PushProgress,
    ) -> StoreResult<PushRecord> {
        let attempts = i32::try_from(progress.attempts).map_err(|_| {
            StoreError::InvalidInput(format!("attempts {} exceeds i32::MAX", progress.attempts))
        })?;
        let cursor = storable(&progress.cursor);
        let expected = to_i64(expected, "version")?;
        let state = progress.state.as_str();
        let next_attempt_at = sql_time(truncate_ms(progress.next_attempt_at));
        let updated = sql_time(now());
        let params: &[&(dyn ToSql + Sync)] = &[
            &run.0,
            &id,
            &expected,
            &state,
            cursor.as_ref(),
            &attempts,
            &progress.last_error,
            &next_attempt_at,
            &updated,
        ];
        let client = self.pool.get().await.map_err(classify_pool)?;
        let stmt = client
            .prepare_cached(&self.sql.push_commit)
            .await
            .map_err(classify)?;
        let row = client
            .query_opt(&stmt, params)
            .await
            .map_err(|e| self.map_write_err(e, run, "", None))?;
        if let Some(row) = row {
            return push_from_row(&row);
        }
        let stmt = client
            .prepare_cached(&self.sql.push_version)
            .await
            .map_err(classify)?;
        let actual: Option<i64> = client
            .query_opt(&stmt, &[&run.0, &id])
            .await
            .map_err(classify)?
            .map(|row| decode_at(&row, 0))
            .transpose()?;
        match actual {
            None => Err(StoreError::NotFound(run)),
            Some(actual) => Err(StoreError::Conflict {
                run,
                expected: expected as u64,
                actual: actual as u64,
            }),
        }
    }

    async fn purge_finished(&self, agent: &str, before: Timestamp) -> StoreResult<u64> {
        let mut total = 0;
        let client = self.pool.get().await.map_err(classify_pool)?;
        let stmt = client
            .prepare_cached(&self.sql.purge)
            .await
            .map_err(classify)?;
        let before_sql = sql_time(before);
        loop {
            let done = client
                .execute(&stmt, &[&agent, &before_sql, &PURGE_BATCH])
                .await
                .map_err(classify)?;
            total += done;
            if done < PURGE_BATCH as u64 {
                break;
            }
        }
        // The run's refs cascaded with it; a digest no surviving run references and that was
        // written before the cutoff goes in the same sweep.
        self.sweep_blobs(&client, before).await?;
        Ok(total)
    }

    fn keeps_blobs(&self) -> bool {
        true
    }

    async fn put_blob(&self, id: &BlobId, bytes: &[u8]) -> StoreResult<()> {
        // Idempotent: `ON CONFLICT DO NOTHING` keeps the first bytes of a digest, and the CHECK on
        // `size` refuses a row whose two columns disagree.
        let size = to_i64(bytes.len() as u64, "blob size")?;
        let created = sql_time(now());
        let params: &[&(dyn ToSql + Sync)] = &[&id.as_str(), &bytes, &size, &created];
        let client = self.pool.get().await.map_err(classify_pool)?;
        let stmt = client
            .prepare_cached(&self.sql.put_blob)
            .await
            .map_err(classify)?;
        client.execute(&stmt, params).await.map_err(classify)?;
        Ok(())
    }

    async fn blob_stored(&self, id: &BlobId) -> StoreResult<bool> {
        let client = self.pool.get().await.map_err(classify_pool)?;
        let stmt = client
            .prepare_cached(&self.sql.blob_exists)
            .await
            .map_err(classify)?;
        let row = client
            .query_one(&stmt, &[&id.as_str()])
            .await
            .map_err(classify)?;
        decode(&row, "exists")
    }

    async fn get_blob(&self, id: &BlobId) -> StoreResult<Option<Vec<u8>>> {
        let client = self.pool.get().await.map_err(classify_pool)?;
        let stmt = client
            .prepare_cached(&self.sql.get_blob)
            .await
            .map_err(classify)?;
        client
            .query_opt(&stmt, &[&id.as_str()])
            .await
            .map_err(classify)?
            .as_ref()
            .map(|row| decode(row, "bytes"))
            .transpose()
    }

    async fn purge_blobs(&self, before: Timestamp) -> StoreResult<u64> {
        let client = self.pool.get().await.map_err(classify_pool)?;
        self.sweep_blobs(&client, before).await
    }
}

impl PgStore {
    /// Delete unreferenced blobs older than `before` in batches; returns how many.
    async fn sweep_blobs(&self, client: &Object, before: Timestamp) -> StoreResult<u64> {
        let stmt = client
            .prepare_cached(&self.sql.purge_blobs)
            .await
            .map_err(classify)?;
        let before = sql_time(before);
        let mut total = 0;
        loop {
            let done = client
                .execute(&stmt, &[&before, &PURGE_BATCH])
                .await
                .map_err(classify)?;
            total += done;
            if done < PURGE_BATCH as u64 {
                return Ok(total);
            }
        }
    }

    /// The row a failed commit writes no version for: the version moved, or the run is gone.
    async fn commit_conflict(&self, id: RunId, expected: u64) -> StoreResult<RunRecord> {
        let client = self.pool.get().await.map_err(classify_pool)?;
        let stmt = client
            .prepare_cached(&self.sql.load_version)
            .await
            .map_err(classify)?;
        let actual: Option<i64> = client
            .query_opt(&stmt, &[&id.0])
            .await
            .map_err(classify)?
            .map(|row| decode_at(&row, 0))
            .transpose()?;
        match actual {
            None => Err(StoreError::NotFound(id)),
            Some(actual) => Err(StoreError::Conflict {
                run: id,
                expected,
                actual: actual as u64,
            }),
        }
    }

    /// The row a winning commit returned, or the version conflict it did not.
    async fn finish_commit(
        &self,
        row: Option<Row>,
        id: RunId,
        expected: u64,
    ) -> StoreResult<RunRecord> {
        match row {
            Some(row) => run_metadata_from_row(&row),
            None => self.commit_conflict(id, expected).await,
        }
    }

    /// A failed `commit_run`: a unique violation is the open-conversation index, and the run tells
    /// which conversation is busy; anything else is the driver's error.
    async fn commit_error(&self, err: tokio_postgres::Error, id: RunId) -> StoreResult<StoreError> {
        if err.code().map(|code| code.code()) == Some("23505") {
            // The only unique index an UPDATE can trip is the open-conversation one; look the
            // run up to report which conversation is busy.
            let run = self.load_run(id).await?.ok_or(StoreError::NotFound(id))?;
            return Ok(self.map_write_err(err, id, &run.agent, run.conversation_id.as_deref()));
        }
        Ok(self.map_write_err(err, id, "", None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use adam_error::Classify;

    #[test]
    fn the_pool_size_is_validated_and_defaults_to_sixteen() {
        assert_eq!(MIN_POOL_SIZE, 2);
        assert_eq!(pool_size_from_text("2").unwrap(), MIN_POOL_SIZE);
        assert_eq!(pool_size_from_text(" 16 ").unwrap(), 16);
        assert_eq!(
            pool_size_from_text(&MAX_POOL_SIZE.to_string()).unwrap(),
            MAX_POOL_SIZE
        );
        for bad in ["0", "1", "65536", "", "sixteen", "-1", "1.5"] {
            let err = pool_size_from_text(bad).unwrap_err();
            assert!(
                matches!(err, StoreError::InvalidInput(_)),
                "{bad:?}: {err:?}"
            );
            assert_eq!(err.class(), ErrorClass::Invalid);
        }
        // A one-connection pool would be taken by the notify listener alone: the message says so.
        let one = pool_size_from_text("1").unwrap_err().to_string();
        assert!(one.contains("notify listener"), "{one}");
        // The unset path is the default; a suite that sets the variable checks its value instead.
        match std::env::var("ADAM_PG_POOL_SIZE") {
            Err(std::env::VarError::NotPresent) => {
                assert_eq!(pool_size_from_env().unwrap(), DEFAULT_POOL_SIZE);
                assert_eq!(DEFAULT_POOL_SIZE, 16);
            }
            Ok(raw) => assert_eq!(
                pool_size_from_env().unwrap(),
                pool_size_from_text(&raw).unwrap()
            ),
            Err(e) => panic!("{e}"),
        }
    }

    #[test]
    fn the_store_keeps_blobs_but_not_bodies_yet() {
        let pg = PgConfig::parse("postgres://u@h/d").expect("a URL");
        let store = PgStore::from_pool(pg.pool(1).expect("a lazy pool"));
        assert!(Store::keeps_blobs(&store));
        assert!(
            !Store::keeps_bodies(&store),
            "the PostgreSQL body half is the follow-up after #361"
        );
    }

    #[tokio::test]
    async fn a_pool_size_out_of_range_is_refused_before_connecting() {
        // No server is needed: the size is checked before a connection is attempted.
        for size in [0, 1, MAX_POOL_SIZE + 1] {
            let err = PgStore::connect_with_pool_size("postgres://example.invalid/db", size)
                .await
                .unwrap_err();
            assert!(
                matches!(err, StoreError::InvalidInput(_)),
                "{size}: {err:?}"
            );
        }
    }

    /// The live statements are the checked-in schema fixture, whitespace-normalised, so a change
    /// to `Sql::new` cannot silently stop being the schema a `sqlx`-built database already has.
    /// `ADAM_WRITE_FIXTURE=1` rewrites the fixture instead of comparing (the migration is the
    /// source of truth; the fixture is what a database built by an earlier release already has).
    #[test]
    fn the_statements_equal_the_version_5_fixture() {
        let sql = Sql::new("adam_");
        assert!(
            sql.migrate.len() > 4,
            "the fixture must hold the whole migration"
        );
        let mut generated = String::from(
            "-- The schema version 5 that adam-store-postgres emits, exactly as the sqlx build wrote it.\n\
             -- Regenerate with ADAM_WRITE_FIXTURE=1 cargo test -p adam-store-postgres --lib the_statements_equal_the_version_5_fixture\n",
        );
        generated.push_str(&sql.migrate.join(";\n\n"));
        generated.push_str(";\n");
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/schema-v5.sql");
        if std::env::var_os("ADAM_WRITE_FIXTURE").is_some() {
            std::fs::write(path, &generated).expect("write the fixture");
        }
        let fixture = include_str!("../tests/fixtures/schema-v5.sql");
        assert_eq!(
            normalise(&(sql.migrate.join("; ") + ";")),
            normalise(fixture)
        );
    }

    /// Whitespace runs collapse to one space; comment lines are dropped.
    fn normalise(sql: &str) -> String {
        let without_comments = sql
            .lines()
            .filter(|line| !line.trim_start().starts_with("--"))
            .collect::<Vec<_>>()
            .join("\n");
        without_comments
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[tokio::test]
    async fn sqlstates_are_classified_against_a_real_server() {
        let Some(url) = adam_core::testing::test_env("ADAM_TEST_POSTGRES_URL") else {
            return;
        };
        let config = PgConfig::parse(&url).expect("url");
        let pool = config.pool(1).expect("pool");
        let client = pool.get().await.expect("connect");
        for (state, want) in [
            ("40001", ErrorClass::Transient), // serialization failure
            ("40P01", ErrorClass::Transient), // deadlock
            ("08006", ErrorClass::Transient), // connection failure
            ("53300", ErrorClass::Transient), // too many connections
            ("57P01", ErrorClass::Transient), // admin shutdown
            ("42601", ErrorClass::Internal),  // syntax error
            ("23514", ErrorClass::Internal),  // check violation
        ] {
            let stmt =
                format!("DO $$ BEGIN RAISE EXCEPTION 'boom' USING ERRCODE = '{state}'; END $$");
            let err = client.batch_execute(&stmt).await.expect_err("it raises");
            assert_eq!(classify(err).class(), want, "SQLSTATE {state}");
        }
        // A real syntax error on a raw query, not a raised one.
        let err = client
            .batch_execute("SELEC 1")
            .await
            .expect_err("syntax error");
        assert_eq!(classify(err).class(), ErrorClass::Internal);
    }

    /// The version 3 -> 4 listing index is rebuilt without taking `runs` from live traffic: a
    /// concurrent build and drop take `SHARE UPDATE EXCLUSIVE`, which a writer's `ROW EXCLUSIVE`
    /// does not conflict with, so a write beside them completes. The non-concurrent pair took
    /// `ACCESS EXCLUSIVE` for the drop and `SHARE` for the build, and every write queued behind
    /// the whole build.
    #[tokio::test]
    async fn the_listing_index_upgrade_does_not_wait_for_live_traffic() {
        let Some(url) = adam_core::testing::test_env("ADAM_TEST_POSTGRES_URL") else {
            return;
        };
        let config = PgConfig::parse(&url).expect("url");
        let pool = config.pool(4).expect("pool");
        let p = format!(
            "adam_online_{}_",
            &Uuid::new_v4().simple().to_string()[..12]
        );
        let store = PgStore::from_pool(pool.clone())
            .with_table_prefix(&p)
            .unwrap();
        store.migrate().await.unwrap();
        // Put the database back at version 3: the old index, and no new one.
        for stmt in [
            format!("DROP INDEX {p}runs_list_status"),
            format!(
                "CREATE INDEX {p}runs_list ON {p}runs (agent, (conversation_id COLLATE \"C\"),
                                                        updated_at DESC, id DESC)
                 WHERE conversation_id IS NOT NULL"
            ),
            format!("UPDATE {p}meta SET value = '3' WHERE key = 'schema_version'"),
        ] {
            pool.get()
                .await
                .unwrap()
                .batch_execute(&stmt)
                .await
                .unwrap();
        }

        // A writer holds its table lock open. A concurrent build runs beside it; a non-concurrent
        // one queues for `ACCESS EXCLUSIVE`/`SHARE` and a write queues behind it.
        let mut writer = pool.get().await.unwrap();
        let tx = writer.transaction().await.unwrap();
        tx.batch_execute(&format!("LOCK TABLE {p}runs IN ROW EXCLUSIVE MODE"))
            .await
            .unwrap();

        let upgrade = {
            let store = store.clone();
            let pool = pool.clone();
            tokio::spawn(async move {
                let mut client = Object::take(pool.get().await.expect("a connection"));
                store.upgrade_listing_index(&mut client).await
            })
        };
        // Wait until the build holds its `SHARE UPDATE EXCLUSIVE` lock on `runs`: from then on the
        // probe below runs beside the DDL itself, not before or after it. (A non-concurrent build
        // asks for `SHARE`/`ACCESS EXCLUSIVE` instead, so this wait times out and the test fails.)
        let build_started = async {
            loop {
                let client = pool.get().await.unwrap();
                let held: i64 = client
                    .query_one(
                        "SELECT count(*) FROM pg_locks
                          WHERE locktype = 'relation' AND relation = to_regclass($1)
                            AND mode = 'ShareUpdateExclusiveLock' AND granted",
                        &[&format!("{p}runs")],
                    )
                    .await
                    .unwrap()
                    .get(0);
                if held > 0 {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), build_started)
            .await
            .expect("the upgrade never took its ShareUpdateExclusiveLock on runs");
        let probe = Uuid::new_v4();
        let client = pool.get().await.unwrap();
        tokio::time::timeout(
            Duration::from_secs(2),
            client.batch_execute(&format!(
                "INSERT INTO {p}runs (id, agent, status, state, version, created_at, updated_at)
                 VALUES ('{probe}', 'probe', 'runnable', '{{}}'::jsonb, 1, now(), now())"
            )),
        )
        .await
        .expect("a write waited for the listing-index upgrade")
        .expect("the write");
        assert!(
            !upgrade.is_finished(),
            "the upgrade finished before the probe, so the probe proved nothing"
        );

        // The build waits for the writer's transaction to end, not the other way round.
        tx.rollback().await.unwrap();
        upgrade.await.unwrap().expect("the listing index upgrade");
        let client = pool.get().await.unwrap();
        let written: i64 = client
            .query_one(
                &format!("SELECT count(*) FROM {p}runs WHERE id = $1"),
                &[&probe],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(written, 1, "the write landed beside the build");

        let valid: Option<bool> = client
            .query_opt(
                "SELECT indisvalid FROM pg_index WHERE indexrelid = to_regclass($1)",
                &[&format!("{p}runs_list_status")],
            )
            .await
            .unwrap()
            .map(|row| row.get(0));
        assert_eq!(valid, Some(true), "the new index is valid");
        let old: Option<String> = client
            .query_opt(
                "SELECT indexdef FROM pg_indexes
                  WHERE schemaname = current_schema() AND indexname = $1",
                &[&format!("{p}runs_list")],
            )
            .await
            .unwrap()
            .map(|row| row.get(0));
        assert_eq!(old, None, "the version 3 index was dropped");

        for table in ["push", "journal", "runs", "meta"] {
            let _ = client
                .batch_execute(&format!("DROP TABLE IF EXISTS {p}{table} CASCADE"))
                .await;
        }
    }
}
