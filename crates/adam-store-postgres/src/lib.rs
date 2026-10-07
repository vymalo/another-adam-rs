//! PostgreSQL implementation of [`adam_core::Store`].
//!
//! # Schema
//!
//! Three tables (names prefixed, `adam_` by default):
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
//!
//! # Concurrency
//!
//! * Commits are a single `UPDATE .. WHERE version = $expected`.
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
//!
//! Everything is single-statement; no multi-statement transactions are held
//! open while agent code runs.

use std::sync::Arc;
use std::time::Duration;

use adam_core::store::{add_ttl, now, sched_at, truncate_ms};
use adam_core::{
    ClaimScope, JournalEntry, Lease, NewPushConfig, NewRun, PushProgress, PushRecord, PushState,
    RunId, RunRecord, RunStatus, RunUpdate, Store, StoreError, StoreResult,
};
use adam_error::ErrorClass;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::postgres::{PgPool, PgPoolOptions, PgRow};
use sqlx::types::Json;
use sqlx::{AssertSqlSafe, Executor, Row};
use uuid::Uuid;

/// Current schema version written to the `<prefix>meta` table.
///
/// * 1: the first schema.
/// * 2: `runs.owner`, the worker a pinned claim ties a run to (see [`ClaimScope`]).
/// * 3: the `push` table (A2A push-notification configurations and their delivery progress).
pub const SCHEMA_VERSION: i32 = 3;

/// Purge deletes in batches of this many runs (plus their journals).
const PURGE_BATCH: i64 = 500;

const PUSH_COLUMNS: &str = "run_id, id, agent, owner, config, cursor, state, attempts, last_error, next_attempt_at, version, created_at, updated_at";

const RUN_COLUMNS: &str = "id, agent, conversation_id, parent_id, status, state, wake_at, version, created_at, updated_at";

/// A [`Store`] backed by PostgreSQL (12+).
#[derive(Clone, Debug)]
pub struct PgStore {
    pool: PgPool,
    prefix: String,
    sql: Sql,
}

impl PgStore {
    /// Connect with a default pool. Call [`Store::migrate`] before first use.
    pub async fn connect(url: &str) -> StoreResult<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(16)
            .connect(url)
            .await
            .map_err(classify)?;
        Ok(Self::from_pool(pool))
    }

    /// Use an existing pool, e.g. the one your app already has.
    pub fn from_pool(pool: PgPool) -> Self {
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

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    fn build(pool: PgPool, prefix: String) -> Self {
        let sql = Sql::new(&prefix);
        Self { pool, prefix, sql }
    }

    fn map_write_err(
        &self,
        err: sqlx::Error,
        run: RunId,
        agent: &str,
        conversation: Option<&str>,
    ) -> StoreError {
        if let Some(db) = err.as_database_error() {
            let constraint = db.constraint().unwrap_or_default();
            if db.code().as_deref() == Some("23505") {
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
            if db.code().as_deref() == Some("23503") {
                return StoreError::NotFound(run);
            }
            if db.code().as_deref() == Some("22P05") {
                return StoreError::InvalidInput(
                    "PostgreSQL JSONB cannot store the NUL character (\\u0000) in strings or keys"
                        .into(),
                );
            }
        }
        classify(err)
    }
}

/// Decide what a driver error means to the caller and box it as the source.
///
/// * Connection loss, pool exhaustion, restarts, deadlocks and serialization failures may
///   succeed later: `Transient` (a retry by the runtime).
/// * A row that cannot be decoded is stored data that breaks an invariant: `Corrupt`. It
///   must not be retried forever (a poisoned row would be re-leased for ever).
/// * A rejected statement or a bad connection string is a bug or a mistake that repeating
///   cannot cure: `Internal` and `Invalid`.
///
/// SQLSTATE classes are from the PostgreSQL manual, Appendix A (unverified from memory:
/// `08` connection exception, `40001` serialization failure, `40P01` deadlock, `53`
/// insufficient resources, `57P01`..`57P03` shutdown and start-up).
pub(crate) fn classify(err: sqlx::Error) -> StoreError {
    use sqlx::Error as E;
    let class = match &err {
        E::Io(_)
        | E::Tls(_)
        | E::PoolTimedOut
        | E::PoolClosed
        | E::WorkerCrashed
        | E::Protocol(_) => ErrorClass::Transient,
        E::Database(db) => match db.code().as_deref() {
            Some(code)
                if code.starts_with("08")
                    || code.starts_with("53")
                    || matches!(code, "40001" | "40P01" | "57P01" | "57P02" | "57P03") =>
            {
                ErrorClass::Transient
            }
            _ => ErrorClass::Internal,
        },
        E::Decode(_) | E::ColumnDecode { .. } | E::ColumnNotFound(_) => ErrorClass::Corrupt,
        E::Configuration(_) => ErrorClass::Invalid,
        _ => ErrorClass::Internal,
    };
    StoreError::Backend {
        class,
        source: Box::new(err),
    }
}

/// SQL text with table names baked in, built once per store.
#[derive(Clone, Debug)]
struct Sql {
    runs_pkey: String,
    open_conversation_index: String,
    migrate: Vec<Arc<str>>,
    insert_run: Arc<str>,
    load_run: Arc<str>,
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
    push_put: Arc<str>,
    push_list: Arc<str>,
    push_delete: Arc<str>,
    push_claim: Arc<str>,
    push_commit: Arc<str>,
    push_version: Arc<str>,
}

impl Sql {
    fn new(p: &str) -> Self {
        let runs = format!("{p}runs");
        let journal = format!("{p}journal");
        let meta = format!("{p}meta");
        let push = format!("{p}push");
        let runs_pkey = format!("{p}runs_pkey");
        let open_conversation_index = format!("{p}runs_open_conversation");
        let statuses = "'runnable', 'parked', 'done', 'failed'";
        let migrate: Vec<Arc<str>> = [
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
            // Record the version, and raise it from an older one. Never lower it: a process
            // running an older release must not undo a newer one's migration.
            format!(
                "INSERT INTO {meta} (key, value) VALUES ('schema_version', '{SCHEMA_VERSION}')
                 ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value
                  WHERE {meta}.value::INT < EXCLUDED.value::INT"
            ),
        ]
        .into_iter()
        .map(Arc::from)
        .collect();
        Self {
            migrate,
            insert_run: Arc::from(format!(
                "INSERT INTO {runs} (id, agent, conversation_id, parent_id, status, state, wake_at,
                                     sched_at, version, created_at, updated_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 1, $9, $9)
                 RETURNING {RUN_COLUMNS}"
            )),
            load_run: Arc::from(format!("SELECT {RUN_COLUMNS} FROM {runs} WHERE id = $1")),
            load_version: Arc::from(format!("SELECT version FROM {runs} WHERE id = $1")),
            commit_run: Arc::from(format!(
                "UPDATE {runs}
                    SET status = $3, state = $4, wake_at = $5, sched_at = $6,
                        version = version + 1, updated_at = $7
                  WHERE id = $1 AND version = $2
                 RETURNING {RUN_COLUMNS}"
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

/// Every statement is built once in [`Sql::new`] from constant text plus the
/// table prefix, which [`PgStore::with_table_prefix`] restricts to
/// `[a-z0-9_]`. Values always go through bind parameters.
fn safe(sql: &Arc<str>) -> AssertSqlSafe<Arc<str>> {
    AssertSqlSafe(Arc::clone(sql))
}

fn to_i64(n: u64, what: &str) -> StoreResult<i64> {
    i64::try_from(n).map_err(|_| StoreError::InvalidInput(format!("{what} {n} exceeds i64::MAX")))
}

fn run_from_row(row: &PgRow) -> StoreResult<RunRecord> {
    let status: String = row.try_get("status").map_err(classify)?;
    let version: i64 = row.try_get("version").map_err(classify)?;
    let Json(state): Json<Value> = row.try_get("state").map_err(classify)?;
    Ok(RunRecord {
        id: RunId(row.try_get::<Uuid, _>("id").map_err(classify)?),
        agent: row.try_get("agent").map_err(classify)?,
        conversation_id: row.try_get("conversation_id").map_err(classify)?,
        parent_id: row
            .try_get::<Option<Uuid>, _>("parent_id")
            .map_err(classify)?
            .map(RunId),
        status: RunStatus::parse(&status)
            .ok_or_else(|| StoreError::Corrupt(format!("unknown run status {status:?}")))?,
        state,
        wake_at: row.try_get("wake_at").map_err(classify)?,
        version: u64::try_from(version)
            .map_err(|_| StoreError::Corrupt(format!("negative version {version}")))?,
        created_at: row.try_get("created_at").map_err(classify)?,
        updated_at: row.try_get("updated_at").map_err(classify)?,
    })
}

fn push_from_row(row: &PgRow) -> StoreResult<PushRecord> {
    let state: String = row.try_get("state").map_err(classify)?;
    let version: i64 = row.try_get("version").map_err(classify)?;
    let attempts: i32 = row.try_get("attempts").map_err(classify)?;
    let Json(config): Json<Value> = row.try_get("config").map_err(classify)?;
    let Json(cursor): Json<Value> = row.try_get("cursor").map_err(classify)?;
    Ok(PushRecord {
        run: RunId(row.try_get::<Uuid, _>("run_id").map_err(classify)?),
        id: row.try_get("id").map_err(classify)?,
        agent: row.try_get("agent").map_err(classify)?,
        owner: row.try_get("owner").map_err(classify)?,
        config,
        cursor,
        state: PushState::parse(&state)
            .ok_or_else(|| StoreError::Corrupt(format!("unknown push state {state:?}")))?,
        attempts: u32::try_from(attempts)
            .map_err(|_| StoreError::Corrupt(format!("negative attempts {attempts}")))?,
        last_error: row.try_get("last_error").map_err(classify)?,
        next_attempt_at: row.try_get("next_attempt_at").map_err(classify)?,
        version: u64::try_from(version)
            .map_err(|_| StoreError::Corrupt(format!("negative version {version}")))?,
        created_at: row.try_get("created_at").map_err(classify)?,
        updated_at: row.try_get("updated_at").map_err(classify)?,
    })
}

fn entry_from_row(row: &PgRow) -> StoreResult<JournalEntry> {
    let seq: i64 = row.try_get("seq").map_err(classify)?;
    let Json(payload): Json<Value> = row.try_get("payload").map_err(classify)?;
    Ok(JournalEntry {
        seq: u64::try_from(seq).map_err(|_| StoreError::Corrupt(format!("negative seq {seq}")))?,
        name: row.try_get("name").map_err(classify)?,
        ok: row.try_get("ok").map_err(classify)?,
        payload,
        recorded_at: row.try_get("recorded_at").map_err(classify)?,
    })
}

#[async_trait]
impl Store for PgStore {
    async fn migrate(&self) -> StoreResult<()> {
        let mut tx = self.pool.begin().await.map_err(classify)?;
        // Serialize concurrent migrations; CREATE .. IF NOT EXISTS alone can
        // still race on the catalog.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
            .bind(format!("adam-rs:migrate:{}", self.prefix))
            .execute(&mut *tx)
            .await
            .map_err(classify)?;
        for stmt in &self.sql.migrate {
            tx.execute(safe(stmt)).await.map_err(classify)?;
        }
        tx.commit().await.map_err(classify)
    }

    async fn create_run(&self, new: NewRun) -> StoreResult<RunRecord> {
        let t = now();
        let wake_at = new.wake_at.map(truncate_ms);
        let row = sqlx::query(safe(&self.sql.insert_run))
            .bind(new.id.0)
            .bind(&new.agent)
            .bind(&new.conversation_id)
            .bind(new.parent_id.map(|p| p.0))
            .bind(new.status.as_str())
            .bind(Json(&new.state))
            .bind(wake_at)
            .bind(sched_at(new.status, wake_at, t))
            .bind(t)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| {
                self.map_write_err(e, new.id, &new.agent, new.conversation_id.as_deref())
            })?;
        run_from_row(&row)
    }

    async fn load_run(&self, id: RunId) -> StoreResult<Option<RunRecord>> {
        let row = sqlx::query(safe(&self.sql.load_run))
            .bind(id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(classify)?;
        row.as_ref().map(run_from_row).transpose()
    }

    async fn commit_run(
        &self,
        id: RunId,
        expected: u64,
        update: RunUpdate,
    ) -> StoreResult<RunRecord> {
        let t = now();
        let wake_at = update.wake_at.map(truncate_ms);
        let result = sqlx::query(safe(&self.sql.commit_run))
            .bind(id.0)
            .bind(to_i64(expected, "version")?)
            .bind(update.status.as_str())
            .bind(Json(&update.state))
            .bind(wake_at)
            .bind(sched_at(update.status, wake_at, t))
            .bind(t)
            .fetch_optional(&self.pool)
            .await;
        let row = match result {
            Ok(row) => row,
            Err(err)
                if err.as_database_error().and_then(|e| e.code()).as_deref() == Some("23505") =>
            {
                // The only unique index an UPDATE can trip is the open-conversation
                // one; look the run up to report which conversation is busy.
                let run = self.load_run(id).await?.ok_or(StoreError::NotFound(id))?;
                return Err(self.map_write_err(
                    err,
                    id,
                    &run.agent,
                    run.conversation_id.as_deref(),
                ));
            }
            Err(err) => return Err(self.map_write_err(err, id, "", None)),
        };
        if let Some(row) = row {
            return run_from_row(&row);
        }
        let actual: Option<i64> = sqlx::query_scalar(safe(&self.sql.load_version))
            .bind(id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(classify)?;
        match actual {
            None => Err(StoreError::NotFound(id)),
            Some(actual) => Err(StoreError::Conflict {
                run: id,
                expected,
                actual: actual as u64,
            }),
        }
    }

    async fn open_run_for_conversation(
        &self,
        agent: &str,
        conversation_id: &str,
    ) -> StoreResult<Option<RunRecord>> {
        let row = sqlx::query(safe(&self.sql.open_run))
            .bind(agent)
            .bind(conversation_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(classify)?;
        row.as_ref().map(run_from_row).transpose()
    }

    async fn journal_get(&self, run: RunId, seq: u64) -> StoreResult<Option<JournalEntry>> {
        let Ok(seq) = i64::try_from(seq) else {
            return Ok(None);
        };
        let row = sqlx::query(safe(&self.sql.journal_get))
            .bind(run.0)
            .bind(seq)
            .fetch_optional(&self.pool)
            .await
            .map_err(classify)?;
        row.as_ref().map(entry_from_row).transpose()
    }

    async fn journal_put(&self, run: RunId, entry: JournalEntry) -> StoreResult<JournalEntry> {
        let seq = to_i64(entry.seq, "journal seq")?;
        let inserted = sqlx::query(safe(&self.sql.journal_insert))
            .bind(run.0)
            .bind(seq)
            .bind(&entry.name)
            .bind(entry.ok)
            .bind(Json(&entry.payload))
            .bind(truncate_ms(entry.recorded_at))
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| self.map_write_err(e, run, "", None))?;
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
        let rows = sqlx::query(safe(&self.sql.journal_list))
            .bind(run.0)
            .fetch_all(&self.pool)
            .await
            .map_err(classify)?;
        rows.iter().map(entry_from_row).collect()
    }

    async fn claim_due(
        &self,
        agents: &[String],
        worker: &str,
        scope: ClaimScope,
        busy: &[RunId],
        now: DateTime<Utc>,
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
        let rows = sqlx::query(safe(statement))
            .bind(agents)
            .bind(now)
            .bind(i64::try_from(limit).unwrap_or(i64::MAX))
            .bind(worker)
            .bind(until)
            .bind(busy)
            .fetch_all(&self.pool)
            .await
            .map_err(classify)?;
        // RETURNING order is unspecified; restore the claim order.
        let mut claimed = rows
            .iter()
            .map(|row| {
                let sched: DateTime<Utc> = row.try_get("sched_at").map_err(classify)?;
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
        now: DateTime<Utc>,
        ttl: Duration,
    ) -> StoreResult<bool> {
        let now = truncate_ms(now);
        let done = sqlx::query(safe(&self.sql.renew_lease))
            .bind(id.0)
            .bind(worker)
            .bind(now)
            .bind(add_ttl(now, ttl))
            .execute(&self.pool)
            .await
            .map_err(classify)?;
        Ok(done.rows_affected() == 1)
    }

    async fn release_lease(&self, id: RunId, worker: &str) -> StoreResult<()> {
        sqlx::query(safe(&self.sql.release_lease))
            .bind(id.0)
            .bind(worker)
            .execute(&self.pool)
            .await
            .map_err(classify)?;
        Ok(())
    }

    async fn lease_until(&self, id: RunId) -> StoreResult<Option<DateTime<Utc>>> {
        let row = sqlx::query(safe(&self.sql.lease_until))
            .bind(id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(classify)?;
        let until = row
            .map(|r| r.try_get::<Option<DateTime<Utc>>, _>("lease_until"))
            .transpose()
            .map_err(classify)?;
        Ok(until.flatten())
    }

    async fn push_put(&self, new: NewPushConfig) -> StoreResult<PushRecord> {
        let row = sqlx::query(safe(&self.sql.push_put))
            .bind(new.run.0)
            .bind(&new.id)
            .bind(&new.agent)
            .bind(&new.owner)
            .bind(Json(&new.config))
            .bind(Json(&new.cursor))
            .bind(now())
            .fetch_one(&self.pool)
            .await
            .map_err(|e| self.map_write_err(e, new.run, &new.agent, None))?;
        push_from_row(&row)
    }

    async fn push_list(&self, run: RunId) -> StoreResult<Vec<PushRecord>> {
        let rows = sqlx::query(safe(&self.sql.push_list))
            .bind(run.0)
            .fetch_all(&self.pool)
            .await
            .map_err(classify)?;
        rows.iter().map(push_from_row).collect()
    }

    async fn push_delete(&self, run: RunId, id: &str) -> StoreResult<bool> {
        let done = sqlx::query(safe(&self.sql.push_delete))
            .bind(run.0)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(classify)?;
        Ok(done.rows_affected() == 1)
    }

    async fn push_claim_due(
        &self,
        agents: &[String],
        worker: &str,
        now: DateTime<Utc>,
        ttl: Duration,
        limit: usize,
    ) -> StoreResult<Vec<PushRecord>> {
        if agents.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let now = truncate_ms(now);
        let rows = sqlx::query(safe(&self.sql.push_claim))
            .bind(agents)
            .bind(now)
            .bind(i64::try_from(limit).unwrap_or(i64::MAX))
            .bind(worker)
            .bind(add_ttl(now, ttl))
            .fetch_all(&self.pool)
            .await
            .map_err(classify)?;
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
        let row = sqlx::query(safe(&self.sql.push_commit))
            .bind(run.0)
            .bind(id)
            .bind(to_i64(expected, "version")?)
            .bind(progress.state.as_str())
            .bind(Json(&progress.cursor))
            .bind(attempts)
            .bind(&progress.last_error)
            .bind(truncate_ms(progress.next_attempt_at))
            .bind(now())
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| self.map_write_err(e, run, "", None))?;
        if let Some(row) = row {
            return push_from_row(&row);
        }
        let actual: Option<i64> = sqlx::query_scalar(safe(&self.sql.push_version))
            .bind(run.0)
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(classify)?;
        match actual {
            None => Err(StoreError::NotFound(run)),
            Some(actual) => Err(StoreError::Conflict {
                run,
                expected,
                actual: actual as u64,
            }),
        }
    }

    async fn purge_finished(&self, agent: &str, before: DateTime<Utc>) -> StoreResult<u64> {
        let mut total = 0;
        loop {
            let done = sqlx::query(safe(&self.sql.purge))
                .bind(agent)
                .bind(before)
                .bind(PURGE_BATCH)
                .execute(&self.pool)
                .await
                .map_err(classify)?;
            total += done.rows_affected();
            if done.rows_affected() < PURGE_BATCH as u64 {
                return Ok(total);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use adam_error::Classify;
    use sqlx::error::BoxDynError;

    fn boxed(msg: &'static str) -> BoxDynError {
        msg.into()
    }

    fn class_of(err: sqlx::Error) -> ErrorClass {
        classify(err).class()
    }

    #[test]
    fn driver_errors_are_classified_by_what_they_mean() {
        let io = || std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        // Exhaustive over the variants we decide on; the rest fall through to Internal.
        let table: Vec<(sqlx::Error, ErrorClass)> = vec![
            (sqlx::Error::Io(io()), ErrorClass::Transient),
            (sqlx::Error::Tls(boxed("handshake")), ErrorClass::Transient),
            (sqlx::Error::PoolTimedOut, ErrorClass::Transient),
            (sqlx::Error::PoolClosed, ErrorClass::Transient),
            (sqlx::Error::WorkerCrashed, ErrorClass::Transient),
            (
                sqlx::Error::Protocol("bad frame".into()),
                ErrorClass::Transient,
            ),
            (sqlx::Error::Decode(boxed("bad utf8")), ErrorClass::Corrupt),
            (
                sqlx::Error::ColumnDecode {
                    index: "version".into(),
                    source: boxed("mismatched types"),
                },
                ErrorClass::Corrupt,
            ),
            (sqlx::Error::ColumnNotFound("x".into()), ErrorClass::Corrupt),
            (
                sqlx::Error::Configuration(boxed("bad url")),
                ErrorClass::Invalid,
            ),
            (sqlx::Error::RowNotFound, ErrorClass::Internal),
            (
                sqlx::Error::Encode(boxed("bad value")),
                ErrorClass::Internal,
            ),
        ];
        for (err, want) in table {
            let shown = err.to_string();
            assert_eq!(class_of(err), want, "{shown}");
        }
    }

    #[test]
    fn a_corrupt_row_is_not_retryable() {
        let e = classify(sqlx::Error::Decode(boxed("bad")));
        assert!(!e.is_retryable());
        assert!(e.class().should_alert());
    }

    /// Provoke a server-side error with a chosen SQLSTATE.
    async fn raise(pool: &PgPool, sqlstate: &str) -> sqlx::Error {
        let stmt =
            format!("DO $$ BEGIN RAISE EXCEPTION 'boom' USING ERRCODE = '{sqlstate}'; END $$");
        match sqlx::query(AssertSqlSafe(stmt)).execute(pool).await {
            Ok(_) => panic!("statement should have failed"),
            Err(e) => e,
        }
    }

    #[tokio::test]
    async fn sqlstates_are_classified_against_a_real_server() {
        let Some(url) = adam_core::testing::test_env("ADAM_TEST_POSTGRES_URL") else {
            return;
        };
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();
        for (state, want) in [
            ("40001", ErrorClass::Transient), // serialization failure
            ("40P01", ErrorClass::Transient), // deadlock
            ("08006", ErrorClass::Transient), // connection failure
            ("53300", ErrorClass::Transient), // too many connections
            ("57P01", ErrorClass::Transient), // admin shutdown
            ("42601", ErrorClass::Internal),  // syntax error
            ("23514", ErrorClass::Internal),  // check violation
        ] {
            assert_eq!(
                class_of(raise(&pool, state).await),
                want,
                "SQLSTATE {state}"
            );
        }
        // A real syntax error on a raw query, not a raised one.
        let err = sqlx::query("SELEC 1").execute(&pool).await.unwrap_err();
        assert_eq!(class_of(err), ErrorClass::Internal);
    }
}
