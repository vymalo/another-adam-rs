//! SQLite implementation of [`adam_core::Store`].
//!
//! # One file, one process
//!
//! The store is a single SQLite file in WAL mode. It is meant for a test harness, a desktop or a
//! client application that has no database server, not for a deployment that splits a control
//! plane from its workers: a file holds **one process** (ADR 0053, decisions 2 and 3). [`open`]
//! takes an exclusive advisory lock on `<path>.lock` and refuses a second process with
//! [`StoreError::InvalidInput`]; the operating system releases the lock when the process dies, so
//! a restart can open the file at once. [`Store::migrate`] clears every lease it finds, because
//! the lock proves no other process is alive: the runs of a killed process are claimable at once.
//!
//! [`open`]: SqliteStore::open
//!
//! # Schema
//!
//! The tables are the PostgreSQL ones with SQLite types, prefixed `adam_` by default:
//! `<prefix>runs` (`state` and `head` as JSON text, timestamps as milliseconds since the epoch),
//! `<prefix>journal`, `<prefix>push`, `<prefix>blob`, `<prefix>run_blob`, `<prefix>body` and
//! `<prefix>meta` (the schema version). One open run per conversation is a partial unique index,
//! and a listing pages on `(updated_at, id)` with a keyset.
//!
//! # Concurrency
//!
//! Every call runs on a blocking thread ([`tokio::task::spawn_blocking`]) and takes the one
//! connection behind a mutex, so the async runtime never waits on the file and a process has one
//! writer. Each change is one `BEGIN IMMEDIATE` transaction; a commit that writes a head, body
//! rows or blob references does the version check, the rows and the compare-and-swap in that one
//! transaction. SQLite has no `FOR UPDATE SKIP LOCKED`, which a single process does not need.
//!
//! # Version
//!
//! [`SCHEMA_VERSION`] is written to `<prefix>meta`. A file stamped with a newer version is refused
//! with a clear error, so an old binary cannot silently read a newer schema.

#![forbid(unsafe_code)]

use std::fmt;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use adam_core::store::{
    add_ttl, head_body, now, sched_at, set_head_body, storable, storable_owned, truncate_ms,
};
use adam_core::time::Timestamp;
use adam_core::{
    BlobId, BodyRow, BodyWrite, ClaimScope, ConversationScope, JournalEntry, Lease, NewPushConfig,
    NewRun, PushProgress, PushRecord, PushState, RunHeader, RunId, RunQuery, RunRecord, RunStatus,
    RunUpdate, Store, StoreError, StoreResult,
};
use adam_error::ErrorClass;
use async_trait::async_trait;
use jiff::Timestamp as JiffTimestamp;
use rusqlite::types::Value;
use rusqlite::{Connection, ErrorCode, Row, TransactionBehavior, params_from_iter};
use serde_json::Value as Json;
use uuid::Uuid;

/// Current schema version written to the `<prefix>meta` table.
///
/// * 1: the first schema, the same tables as PostgreSQL's schema version 6 (runs with their head
///   and version, the journal, push configurations, blobs and their references, and the body).
pub const SCHEMA_VERSION: i32 = 1;

/// How old a foreign attempt's body row must be before a commit may collect it: the insert→CAS gap
/// is milliseconds, and the runtime's default lease is 30 s, so five minutes separates a dead
/// attempt from a live one.
const BODY_ORPHAN_GRACE: jiff::SignedDuration = jiff::SignedDuration::from_mins(5);

/// How old a superseded body generation must be before a commit collects it.
const BODY_GC_GRACE: jiff::SignedDuration = jiff::SignedDuration::from_hours(1);

/// Purge deletes in batches of this many runs.
const PURGE_BATCH: i64 = 500;

const RUN_COLUMNS: &str = "id, agent, conversation_id, parent_id, status, state, wake_at, version, created_at, updated_at";
const RUN_METADATA_COLUMNS: &str =
    "id, agent, conversation_id, parent_id, status, wake_at, version, created_at, updated_at";
const RUN_HEADER_COLUMNS: &str = "id, agent, conversation_id, parent_id, status, wake_at, version, created_at, updated_at, lease_until";
const PUSH_COLUMNS: &str = "run_id, id, agent, owner, config, cursor, state, attempts, last_error, next_attempt_at, version, created_at, updated_at";

/// A [`Store`] backed by one SQLite file.
#[derive(Clone)]
pub struct SqliteStore {
    inner: Arc<Inner>,
    sql: Arc<Sql>,
}

struct Inner {
    conn: Mutex<Connection>,
    path: PathBuf,
    /// The exclusive advisory lock on `<path>.lock`, held for the life of the store. The operating
    /// system releases it when the process exits, so a killed process never leaves it held.
    _lock: Option<File>,
}

impl fmt::Debug for SqliteStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SqliteStore")
            .field("path", &self.inner.path)
            .field("prefix", &self.sql.prefix)
            .finish_non_exhaustive()
    }
}

impl SqliteStore {
    /// Open the SQLite file at `path` (or `:memory:`), take the process lock and set the pragmas.
    ///
    /// A second process on the same file is refused with [`StoreError::InvalidInput`]. Call
    /// [`Store::migrate`] before first use.
    ///
    /// # Errors
    ///
    /// The path cannot be opened, another process holds its lock, or SQLite refuses a pragma.
    pub fn open(path: impl AsRef<Path>) -> StoreResult<Self> {
        let path = path.as_ref();
        let memory = path == Path::new(":memory:");
        let lock = if memory { None } else { Some(lock_file(path)?) };
        let conn = if memory {
            Connection::open_in_memory()
        } else {
            Connection::open(path)
        }
        .map_err(|e| {
            StoreError::InvalidInput(format!(
                "opening the SQLite database {}: {e}",
                path.display()
            ))
        })?;
        // A foreign key is what deletes a run's journal, push configurations, body rows and blob
        // references with it, and what refuses a journal entry or push configuration of no run.
        conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")
            .map_err(|e| {
                StoreError::InvalidInput(format!("setting the pragmas of {}: {e}", path.display()))
            })?;
        if !memory {
            conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
                .map_err(|e| {
                    StoreError::InvalidInput(format!("putting {} in WAL mode: {e}", path.display()))
                })?;
        }
        Ok(Self {
            inner: Arc::new(Inner {
                conn: Mutex::new(conn),
                path: path.to_path_buf(),
                _lock: lock,
            }),
            sql: Arc::new(Sql::new("adam_")),
        })
    }

    /// Use a different table-name prefix (default `adam_`), e.g. to host two isolated stores in one
    /// file. Only `[a-z0-9_]` is allowed, starting with a letter or underscore, at most 40
    /// characters.
    ///
    /// # Errors
    ///
    /// The prefix is not a valid identifier.
    pub fn with_table_prefix(self, prefix: &str) -> StoreResult<Self> {
        if !valid_prefix(prefix) {
            return Err(StoreError::InvalidInput(format!(
                "invalid table prefix {prefix:?}"
            )));
        }
        Ok(Self {
            inner: self.inner,
            sql: Arc::new(Sql::new(prefix)),
        })
    }

    /// The file this store opened (`:memory:` for an in-memory one).
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Open a connection now, to learn whether the file answers.
    ///
    /// # Errors
    ///
    /// The connection is gone or SQLite refuses the probe.
    pub async fn ping(&self) -> StoreResult<()> {
        self.blocking(|conn, _| {
            query_optional(conn, "SELECT 1", &[], |row| {
                row.get::<_, i64>(0).map_err(classify)
            })?
            .map(|_| ())
            .ok_or_else(|| StoreError::internal(std::io::Error::other("SQLite answered no row")))
        })
        .await
    }

    /// Run `f` on the blocking pool, with the one connection locked.
    async fn blocking<T, F>(&self, f: F) -> StoreResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection, &Sql) -> StoreResult<T> + Send + 'static,
    {
        let inner = self.inner.clone();
        let sql = self.sql.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = inner
                .conn
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            f(&mut conn, &sql)
        })
        .await
        .map_err(StoreError::internal)?
    }
}

fn valid_prefix(prefix: &str) -> bool {
    !prefix.is_empty()
        && prefix.len() <= 40
        && prefix
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !prefix.starts_with(|c: char| c.is_ascii_digit())
}

/// The lock path of a database file: `<path>.lock`.
fn lock_path(path: &Path) -> PathBuf {
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    PathBuf::from(lock)
}

/// Take an exclusive advisory lock on `<path>.lock`, or say who holds it.
fn lock_file(path: &Path) -> StoreResult<File> {
    let lock = lock_path(path);
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock)
        .map_err(|e| {
            StoreError::InvalidInput(format!("opening the lock file {}: {e}", lock.display()))
        })?;
    match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(file),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            Err(StoreError::InvalidInput(format!(
                "the SQLite database {} is already open by another process: one process per file \
                 (its lock {} is held)",
                path.display(),
                lock.display()
            )))
        }
        Err(e) => Err(StoreError::InvalidInput(format!(
            "locking {}: {e}",
            lock.display()
        ))),
    }
}

/// The SQL text of one store, with the table names baked in.
struct Sql {
    prefix: String,
    meta: String,
    runs: String,
    journal: String,
    push: String,
    blob: String,
    run_blob: String,
    body: String,
    open_conversation_index: String,
}

impl Sql {
    fn new(prefix: &str) -> Self {
        Self {
            prefix: prefix.to_owned(),
            meta: format!("{prefix}meta"),
            runs: format!("{prefix}runs"),
            journal: format!("{prefix}journal"),
            push: format!("{prefix}push"),
            blob: format!("{prefix}blob"),
            run_blob: format!("{prefix}run_blob"),
            body: format!("{prefix}body"),
            open_conversation_index: format!("{prefix}runs_open_conversation"),
        }
    }

    /// The whole schema, idempotent. `foreign_keys` is on, so the references are enforced.
    fn migrate_statements(&self) -> Vec<String> {
        let Self {
            meta,
            runs,
            journal,
            push,
            blob,
            run_blob,
            body,
            open_conversation_index,
            ..
        } = self;
        let statuses = "'runnable', 'parked', 'done', 'failed'";
        vec![
            format!(
                "CREATE TABLE IF NOT EXISTS {meta} (key TEXT PRIMARY KEY, value TEXT NOT NULL)"
            ),
            format!(
                "CREATE TABLE IF NOT EXISTS {runs} (
                    id              TEXT    NOT NULL,
                    agent           TEXT    NOT NULL,
                    conversation_id TEXT,
                    parent_id       TEXT,
                    status          TEXT    NOT NULL CHECK (status IN ({statuses})),
                    state           TEXT    NOT NULL,
                    wake_at         INTEGER,
                    sched_at        INTEGER,
                    version         INTEGER NOT NULL CHECK (version > 0),
                    lease_owner     TEXT,
                    lease_until     INTEGER,
                    owner           TEXT,
                    head            TEXT,
                    head_version    INTEGER,
                    created_at      INTEGER NOT NULL,
                    updated_at      INTEGER NOT NULL,
                    PRIMARY KEY (id)
                )"
            ),
            format!(
                "CREATE UNIQUE INDEX IF NOT EXISTS {open_conversation_index}
                 ON {runs} (agent, conversation_id)
                 WHERE conversation_id IS NOT NULL AND status IN ('runnable', 'parked')"
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS {runs}_due ON {runs} (agent, sched_at, id)
                 WHERE sched_at IS NOT NULL"
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS {runs}_finished ON {runs} (agent, updated_at)
                 WHERE status IN ('done', 'failed')"
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS {runs}_parent ON {runs} (parent_id)
                 WHERE parent_id IS NOT NULL"
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS {runs}_list
                 ON {runs} (agent, conversation_id, updated_at DESC, id DESC)
                 WHERE conversation_id IS NOT NULL"
            ),
            format!(
                "CREATE TABLE IF NOT EXISTS {journal} (
                    run_id      TEXT    NOT NULL REFERENCES {runs} (id) ON DELETE CASCADE,
                    seq         INTEGER NOT NULL CHECK (seq >= 0),
                    name        TEXT    NOT NULL,
                    ok          INTEGER NOT NULL CHECK (ok IN (0, 1)),
                    payload     TEXT    NOT NULL,
                    recorded_at INTEGER NOT NULL,
                    PRIMARY KEY (run_id, seq)
                )"
            ),
            format!(
                "CREATE TABLE IF NOT EXISTS {push} (
                    run_id          TEXT    NOT NULL REFERENCES {runs} (id) ON DELETE CASCADE,
                    id              TEXT    NOT NULL,
                    agent           TEXT    NOT NULL,
                    owner           TEXT    NOT NULL,
                    config          TEXT    NOT NULL,
                    cursor          TEXT    NOT NULL,
                    state           TEXT    NOT NULL CHECK (state IN ('active', 'done', 'gave_up')),
                    attempts        INTEGER NOT NULL CHECK (attempts >= 0),
                    last_error      TEXT,
                    next_attempt_at INTEGER NOT NULL,
                    version         INTEGER NOT NULL CHECK (version > 0),
                    lease_owner     TEXT,
                    lease_until     INTEGER,
                    created_at      INTEGER NOT NULL,
                    updated_at      INTEGER NOT NULL,
                    PRIMARY KEY (run_id, id)
                )"
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS {push}_due
                 ON {push} (agent, next_attempt_at, run_id, id) WHERE state = 'active'"
            ),
            format!(
                "CREATE TABLE IF NOT EXISTS {blob} (
                    digest      TEXT    NOT NULL,
                    bytes       BLOB    NOT NULL,
                    size        INTEGER NOT NULL CHECK (size = length(bytes)),
                    created_at  INTEGER NOT NULL,
                    PRIMARY KEY (digest),
                    CHECK (length(digest) = 64)
                )"
            ),
            format!("CREATE INDEX IF NOT EXISTS {blob}_gc ON {blob} (created_at)"),
            format!(
                "CREATE TABLE IF NOT EXISTS {run_blob} (
                    run_id  TEXT NOT NULL REFERENCES {runs} (id) ON DELETE CASCADE,
                    digest  TEXT NOT NULL,
                    PRIMARY KEY (run_id, digest)
                )"
            ),
            format!("CREATE INDEX IF NOT EXISTS {run_blob}_digest ON {run_blob} (digest)"),
            format!(
                "CREATE TABLE IF NOT EXISTS {body} (
                    run_id      TEXT    NOT NULL REFERENCES {runs} (id) ON DELETE CASCADE,
                    generation  TEXT    NOT NULL,
                    idx         INTEGER NOT NULL CHECK (idx >= 0),
                    message     TEXT    NOT NULL,
                    writer      TEXT    NOT NULL,
                    written_at  INTEGER NOT NULL,
                    PRIMARY KEY (run_id, generation, idx)
                )"
            ),
            format!("CREATE INDEX IF NOT EXISTS {body}_written ON {body} (written_at)"),
        ]
    }
}

// ---------------------------------------------------------------------------------------------
// The Store implementation
// ---------------------------------------------------------------------------------------------

#[async_trait]
impl Store for SqliteStore {
    async fn migrate(&self) -> StoreResult<()> {
        let path = self.inner.path.clone();
        self.blocking(move |conn, sql| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(classify)?;
            execute(
                &tx,
                &format!(
                    "CREATE TABLE IF NOT EXISTS {} (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
                    sql.meta
                ),
                &[],
            )?;
            let stored: Option<String> = query_optional(
                &tx,
                &format!(
                    "SELECT value FROM {} WHERE key = 'schema_version'",
                    sql.meta
                ),
                &[],
                |row| row.get(0).map_err(classify),
            )?;
            let stored = stored
                .map(|value| {
                    value.parse::<i32>().map_err(|_| {
                        StoreError::Corrupt(format!("schema_version {value:?} is not an integer"))
                    })
                })
                .transpose()?;
            if let Some(version) = stored
                && version > SCHEMA_VERSION
            {
                return Err(StoreError::InvalidInput(format!(
                    "the SQLite database {} has schema version {version}, newer than this \
                     binary's {SCHEMA_VERSION}: upgrade the binary, or start the release that \
                     migrated the file",
                    path.display()
                )));
            }
            for statement in sql.migrate_statements() {
                tx.execute_batch(&statement).map_err(classify)?;
            }
            execute(
                &tx,
                &format!(
                    "INSERT INTO {} (key, value) VALUES ('schema_version', ?)
                     ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                    sql.meta
                ),
                &[Value::Text(SCHEMA_VERSION.to_string())],
            )?;
            // The file lock proves no other process is alive: every lease it finds belongs to a
            // predecessor that died, so its runs are claimable at once.
            execute(
                &tx,
                &format!(
                    "UPDATE {} SET lease_owner = NULL, lease_until = NULL
                      WHERE lease_until IS NOT NULL",
                    sql.runs
                ),
                &[],
            )?;
            tx.commit().map_err(classify)
        })
        .await
    }

    async fn create_run(&self, new: NewRun) -> StoreResult<RunRecord> {
        let t = now();
        let wake_at = new.wake_at.map(truncate_ms);
        let sched = sched_at(new.status, wake_at, t);
        let mut new = new;
        let state = json_text_owned(std::mem::take(&mut new.state))?;
        self.blocking(move |conn, sql| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(classify)?;
            let exists: bool = query_optional(
                &tx,
                &format!("SELECT 1 FROM {} WHERE id = ?", sql.runs),
                &[Value::Text(new.id.to_string())],
                |row| row.get::<_, i64>(0).map_err(classify),
            )?
            .is_some();
            if exists {
                return Err(StoreError::AlreadyExists(new.id));
            }
            let result = execute_raw(
                &tx,
                &format!(
                    "INSERT INTO {} (id, agent, conversation_id, parent_id, status, state, wake_at,
                                     sched_at, version, created_at, updated_at)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?)",
                    sql.runs
                ),
                &[
                    Value::Text(new.id.to_string()),
                    Value::Text(new.agent.clone()),
                    opt_text(new.conversation_id.as_deref()),
                    opt_text(new.parent_id.map(|id| id.to_string()).as_deref()),
                    Value::Text(new.status.as_str().to_owned()),
                    Value::Text(state),
                    opt_integer(wake_at.map(to_ms)),
                    opt_integer(sched.map(to_ms)),
                    Value::Integer(to_ms(t)),
                    Value::Integer(to_ms(t)),
                ],
            );
            if let Err(e) = result {
                return Err(map_create_error(e, &new));
            }
            let record = query_optional(
                &tx,
                &format!(
                    "SELECT {RUN_METADATA_COLUMNS} FROM {} WHERE id = ?",
                    sql.runs
                ),
                &[Value::Text(new.id.to_string())],
                metadata_from_row,
            )?
            .ok_or_else(|| {
                StoreError::Corrupt(format!("run {} vanished after its insert", new.id))
            })?;
            tx.commit().map_err(classify)?;
            Ok(record)
        })
        .await
    }

    async fn load_run(&self, id: RunId) -> StoreResult<Option<RunRecord>> {
        self.blocking(move |conn, sql| {
            query_optional(
                conn,
                &format!("SELECT {RUN_COLUMNS} FROM {} WHERE id = ?", sql.runs),
                &[Value::Text(id.to_string())],
                run_from_row,
            )
        })
        .await
    }

    async fn load_head(&self, id: RunId) -> StoreResult<Option<RunRecord>> {
        self.blocking(move |conn, sql| {
            query_optional(
                conn,
                &format!(
                    "SELECT id, agent, conversation_id, parent_id, status,
                            CASE WHEN head_version = version AND head IS NOT NULL
                                 THEN head ELSE state END AS state,
                            wake_at, version, created_at, updated_at
                       FROM {} WHERE id = ?",
                    sql.runs
                ),
                &[Value::Text(id.to_string())],
                run_from_row,
            )
        })
        .await
    }

    async fn run_header(&self, id: RunId) -> StoreResult<Option<RunHeader>> {
        self.blocking(move |conn, sql| {
            query_optional(
                conn,
                &format!("SELECT {RUN_HEADER_COLUMNS} FROM {} WHERE id = ?", sql.runs),
                &[Value::Text(id.to_string())],
                run_header_from_row,
            )
        })
        .await
    }

    async fn commit_run(
        &self,
        id: RunId,
        expected: u64,
        update: RunUpdate,
    ) -> StoreResult<RunRecord> {
        let t = now();
        let expected_i64 = to_i64(expected, "version")?;
        let RunUpdate {
            status,
            state,
            head,
            body,
            blobs,
            wake_at,
        } = update;
        let wake_at = wake_at.map(truncate_ms);
        let sched = sched_at(status, wake_at, t);
        let state = json_text_owned(state)?;
        let status = status.as_str().to_owned();
        self.blocking(move |conn, sql| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(classify)?;
            let context = query_optional(
                &tx,
                &format!(
                    "SELECT version, agent, conversation_id, head, head_version
                       FROM {} WHERE id = ?",
                    sql.runs
                ),
                &[Value::Text(id.to_string())],
                |row| {
                    Ok((
                        row.get::<_, i64>("version").map_err(classify)?,
                        row.get::<_, String>("agent").map_err(classify)?,
                        row.get::<_, Option<String>>("conversation_id")
                            .map_err(classify)?,
                        row.get::<_, Option<String>>("head").map_err(classify)?,
                        row.get::<_, Option<i64>>("head_version")
                            .map_err(classify)?,
                    ))
                },
            )?;
            let Some((version, agent, conversation_id, current_head, head_version)) = context
            else {
                return Err(StoreError::NotFound(id));
            };
            if version != expected_i64 {
                return Err(StoreError::Conflict {
                    run: id,
                    expected,
                    actual: u64::try_from(version).unwrap_or(0),
                });
            }
            let current_head = current_head.as_deref().map(json_value).transpose()?;
            let mut head = head;
            let plan = body
                .map(|body| {
                    plan_body(
                        &mut head,
                        current_head.as_ref(),
                        head_version,
                        expected_i64,
                        body,
                        t,
                    )
                })
                .transpose()?;
            if let Some(plan) = &plan {
                write_body(&tx, sql, id, plan, expected)?;
            }
            if !blobs.is_empty() {
                let mut seen = std::collections::HashSet::new();
                for digest in blobs.iter().filter(|digest| seen.insert(digest.as_str())) {
                    execute(
                        &tx,
                        &format!(
                            "INSERT OR IGNORE INTO {} (run_id, digest) VALUES (?, ?)",
                            sql.run_blob
                        ),
                        &[
                            Value::Text(id.to_string()),
                            Value::Text(digest.as_str().to_owned()),
                        ],
                    )?;
                }
            }
            let head_text = head.as_ref().map(json_text).transpose()?;
            let updated = execute_raw(
                &tx,
                &match head_text {
                    Some(_) => format!(
                        "UPDATE {} SET status = ?, state = ?, wake_at = ?, sched_at = ?,
                                      version = version + 1, updated_at = ?, head = ?,
                                      head_version = ?
                          WHERE id = ? AND version = ?",
                        sql.runs
                    ),
                    None => format!(
                        "UPDATE {} SET status = ?, state = ?, wake_at = ?, sched_at = ?,
                                      version = version + 1, updated_at = ?
                          WHERE id = ? AND version = ?",
                        sql.runs
                    ),
                },
                &match &head_text {
                    Some(head) => vec![
                        Value::Text(status.clone()),
                        Value::Text(state.clone()),
                        opt_integer(wake_at.map(to_ms)),
                        opt_integer(sched.map(to_ms)),
                        Value::Integer(to_ms(t)),
                        Value::Text(head.clone()),
                        Value::Integer(expected_i64 + 1),
                        Value::Text(id.to_string()),
                        Value::Integer(expected_i64),
                    ],
                    None => vec![
                        Value::Text(status.clone()),
                        Value::Text(state.clone()),
                        opt_integer(wake_at.map(to_ms)),
                        opt_integer(sched.map(to_ms)),
                        Value::Integer(to_ms(t)),
                        Value::Text(id.to_string()),
                        Value::Integer(expected_i64),
                    ],
                },
            );
            match updated {
                Ok(0) => {
                    return Err(StoreError::Conflict {
                        run: id,
                        expected,
                        actual: u64::try_from(version).unwrap_or(0),
                    });
                }
                Ok(_) => {}
                Err(e) if unique_violation(&e) => {
                    return Err(match conversation_id {
                        Some(conversation_id) => StoreError::ConversationBusy {
                            agent,
                            conversation_id,
                        },
                        None => classify(e),
                    });
                }
                Err(e) => return Err(classify(e)),
            }
            let record = query_optional(
                &tx,
                &format!(
                    "SELECT {RUN_METADATA_COLUMNS} FROM {} WHERE id = ?",
                    sql.runs
                ),
                &[Value::Text(id.to_string())],
                metadata_from_row,
            )?
            .ok_or_else(|| StoreError::Corrupt(format!("run {id} vanished after its commit")))?;
            tx.commit().map_err(classify)?;
            // A superseded generation is collected after the CAS, an hour of grace; the generation
            // the commit names is never collected here.
            if let Some(plan) = plan {
                let cutoff = to_ms(t - BODY_GC_GRACE);
                let _ = execute(
                    conn,
                    &format!(
                        "DELETE FROM {} WHERE run_id = ? AND generation <> ? AND written_at < ?",
                        sql.body
                    ),
                    &[
                        Value::Text(id.to_string()),
                        Value::Text(plan.generation.to_string()),
                        Value::Integer(cutoff),
                    ],
                );
            }
            Ok(record)
        })
        .await
    }

    async fn open_run_for_conversation(
        &self,
        agent: &str,
        conversation_id: &str,
    ) -> StoreResult<Option<RunRecord>> {
        let agent = agent.to_owned();
        let conversation_id = conversation_id.to_owned();
        self.blocking(move |conn, sql| {
            query_optional(
                conn,
                &format!(
                    "SELECT {RUN_COLUMNS} FROM {}
                      WHERE agent = ? AND conversation_id = ?
                        AND status IN ('runnable', 'parked')",
                    sql.runs
                ),
                &[Value::Text(agent), Value::Text(conversation_id)],
                run_from_row,
            )
        })
        .await
    }

    async fn journal_get(&self, run: RunId, seq: u64) -> StoreResult<Option<JournalEntry>> {
        let Ok(seq) = i64::try_from(seq) else {
            return Ok(None);
        };
        self.blocking(move |conn, sql| {
            query_optional(
                conn,
                &format!(
                    "SELECT seq, name, ok, payload, recorded_at FROM {}
                      WHERE run_id = ? AND seq = ?",
                    sql.journal
                ),
                &[Value::Text(run.to_string()), Value::Integer(seq)],
                entry_from_row,
            )
        })
        .await
    }

    async fn journal_put(&self, run: RunId, entry: JournalEntry) -> StoreResult<JournalEntry> {
        let seq = to_i64(entry.seq, "journal seq")?;
        let recorded_at = truncate_ms(entry.recorded_at);
        let payload = json_text_owned(entry.payload)?;
        let name = entry.name.clone();
        self.blocking(move |conn, sql| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(classify)?;
            let inserted = execute_raw(
                &tx,
                &format!(
                    "INSERT INTO {} (run_id, seq, name, ok, payload, recorded_at)
                     VALUES (?, ?, ?, ?, ?, ?)",
                    sql.journal
                ),
                &[
                    Value::Text(run.to_string()),
                    Value::Integer(seq),
                    Value::Text(name.clone()),
                    Value::Integer(i64::from(entry.ok)),
                    Value::Text(payload.clone()),
                    Value::Integer(to_ms(recorded_at)),
                ],
            );
            match inserted {
                Ok(_) => {
                    let stored = JournalEntry {
                        seq: entry.seq,
                        name,
                        ok: entry.ok,
                        payload: json_value(&payload)?,
                        recorded_at,
                    };
                    tx.commit().map_err(classify)?;
                    return Ok(stored);
                }
                Err(e) if unique_violation(&e) => {}
                Err(e) if foreign_key_violation(&e) => return Err(StoreError::NotFound(run)),
                Err(e) => return Err(classify(e)),
            }
            let existing = query_optional(
                &tx,
                &format!(
                    "SELECT seq, name, ok, payload, recorded_at FROM {}
                      WHERE run_id = ? AND seq = ?",
                    sql.journal
                ),
                &[Value::Text(run.to_string()), Value::Integer(seq)],
                entry_from_row,
            )?
            .ok_or_else(|| StoreError::Corrupt(format!("journal entry {run}/{seq} vanished")))?;
            if existing.name != entry.name {
                return Err(StoreError::NonDeterminism {
                    run,
                    seq: entry.seq,
                    recorded: existing.name,
                    requested: entry.name,
                });
            }
            tx.commit().map_err(classify)?;
            Ok(existing)
        })
        .await
    }

    async fn journal_list(&self, run: RunId) -> StoreResult<Vec<JournalEntry>> {
        self.blocking(move |conn, sql| {
            query_all(
                conn,
                &format!(
                    "SELECT seq, name, ok, payload, recorded_at FROM {}
                      WHERE run_id = ? ORDER BY seq",
                    sql.journal
                ),
                &[Value::Text(run.to_string())],
                entry_from_row,
            )
        })
        .await
    }

    #[allow(clippy::too_many_arguments)]
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
        let agents = agents.to_vec();
        let worker = worker.to_owned();
        let busy: Vec<String> = busy.iter().map(RunId::to_string).collect();
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.blocking(move |conn, sql| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(classify)?;
            let mut params = Vec::new();
            let mut placeholders = String::new();
            for (index, agent) in agents.iter().enumerate() {
                if index > 0 {
                    placeholders.push_str(", ");
                }
                placeholders.push('?');
                params.push(Value::Text(agent.clone()));
            }
            params.push(Value::Integer(to_ms(now)));
            params.push(Value::Integer(to_ms(now)));
            if scope == ClaimScope::Pinned {
                params.push(Value::Text(worker.clone()));
            }
            let busy_clause = if busy.is_empty() {
                String::new()
            } else {
                let mut clause = String::from(" AND id NOT IN (");
                for (index, id) in busy.iter().enumerate() {
                    if index > 0 {
                        clause.push_str(", ");
                    }
                    clause.push('?');
                    params.push(Value::Text(id.clone()));
                }
                clause.push(')');
                clause
            };
            let owner_clause = if scope == ClaimScope::Pinned {
                " AND (owner IS NULL OR owner = ?)"
            } else {
                ""
            };
            let ids = query_all(
                &tx,
                &format!(
                    "SELECT id FROM {} WHERE agent IN ({placeholders})
                       AND sched_at IS NOT NULL AND sched_at <= ?
                       AND (lease_until IS NULL OR lease_until <= ?){owner_clause}{busy_clause}
                     ORDER BY sched_at, id LIMIT ?",
                    sql.runs
                ),
                &{
                    let mut all = params.clone();
                    all.push(Value::Integer(limit));
                    all
                },
                |row| row.get::<_, String>(0).map_err(classify),
            )?;
            if ids.is_empty() {
                tx.commit().map_err(classify)?;
                return Ok(Vec::new());
            }
            let mut update = format!("UPDATE {} SET lease_owner = ?, lease_until = ?", sql.runs);
            if scope == ClaimScope::Pinned {
                update.push_str(", owner = COALESCE(owner, ?)");
            }
            update.push_str(" WHERE id IN (");
            let mut update_params = vec![Value::Text(worker.clone()), Value::Integer(to_ms(until))];
            if scope == ClaimScope::Pinned {
                update_params.push(Value::Text(worker.clone()));
            }
            for (index, id) in ids.iter().enumerate() {
                if index > 0 {
                    update.push_str(", ");
                }
                update.push('?');
                update_params.push(Value::Text(id.clone()));
            }
            update.push(')');
            execute(&tx, &update, &update_params)?;
            let mut select = format!(
                "SELECT {RUN_COLUMNS}, sched_at FROM {} WHERE id IN (",
                sql.runs
            );
            let mut select_params = Vec::new();
            for (index, id) in ids.iter().enumerate() {
                if index > 0 {
                    select.push_str(", ");
                }
                select.push('?');
                select_params.push(Value::Text(id.clone()));
            }
            select.push_str(") ORDER BY sched_at, id");
            let leases = query_all(&tx, &select, &select_params, |row| {
                Ok(Lease {
                    run: run_from_row(row)?,
                    worker: worker.clone(),
                    until,
                })
            })?;
            tx.commit().map_err(classify)?;
            Ok(leases)
        })
        .await
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
        let worker = worker.to_owned();
        self.blocking(move |conn, sql| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(classify)?;
            let changed = execute(
                &tx,
                &format!(
                    "UPDATE {} SET lease_until = ?
                      WHERE id = ? AND lease_owner = ? AND lease_until > ?",
                    sql.runs
                ),
                &[
                    Value::Integer(to_ms(until)),
                    Value::Text(id.to_string()),
                    Value::Text(worker),
                    Value::Integer(to_ms(now)),
                ],
            )?;
            tx.commit().map_err(classify)?;
            Ok(changed == 1)
        })
        .await
    }

    async fn release_lease(&self, id: RunId, worker: &str) -> StoreResult<()> {
        let worker = worker.to_owned();
        self.blocking(move |conn, sql| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(classify)?;
            execute(
                &tx,
                &format!(
                    "UPDATE {} SET lease_owner = NULL, lease_until = NULL
                      WHERE id = ? AND lease_owner = ?",
                    sql.runs
                ),
                &[Value::Text(id.to_string()), Value::Text(worker)],
            )?;
            tx.commit().map_err(classify)
        })
        .await
    }

    // TODO(#362, S-4G7Y): `Store::release_worker_leases(worker)` is defaulted and not on `main`
    // yet (checked at 843ded1). The separate `worker_leases_conformance!` covers Memory and
    // SQLite; PostgreSQL and MongoDB keep the no-op default. When the trait gains it, implement
    // `async fn release_worker_leases(&self, worker: &str) -> StoreResult<u64>` as one
    // `BEGIN IMMEDIATE` `UPDATE {runs} SET lease_owner = NULL, lease_until = NULL
    // WHERE lease_owner = ?`, returning the rows changed, and add
    // `adam_store_testkit::worker_leases_conformance!(make_store);` to `tests/conformance.rs`.

    async fn lease_until(&self, id: RunId) -> StoreResult<Option<Timestamp>> {
        self.blocking(move |conn, sql| {
            query_optional(
                conn,
                &format!("SELECT lease_until FROM {} WHERE id = ?", sql.runs),
                &[Value::Text(id.to_string())],
                |row| {
                    row.get::<_, Option<i64>>(0)
                        .map_err(classify)?
                        .map(from_ms)
                        .transpose()
                },
            )
            .map(Option::flatten)
        })
        .await
    }

    async fn list_runs(&self, query: &RunQuery) -> StoreResult<Vec<RunRecord>> {
        let query = query.clone();
        self.blocking(move |conn, sql| {
            let mut params = Vec::new();
            let mut clauses = vec![String::from("agent = ?")];
            params.push(Value::Text(query.agent.clone()));
            push_scope(&query.scope, &mut clauses, &mut params);
            if !push_filters(&query, &mut clauses, &mut params, true)? {
                return Ok(Vec::new());
            }
            let text = format!(
                "SELECT {RUN_COLUMNS} FROM {} WHERE {} ORDER BY updated_at DESC, id DESC LIMIT ?",
                sql.runs,
                clauses.join(" AND ")
            );
            let limit = i64::try_from(query.limit).unwrap_or(i64::MAX);
            params.push(Value::Integer(limit));
            query_all(conn, &text, &params, run_from_row)
        })
        .await
    }

    async fn count_runs(&self, query: &RunQuery) -> StoreResult<u64> {
        let query = query.clone();
        self.blocking(move |conn, sql| {
            let mut params = Vec::new();
            let mut clauses = vec![String::from("agent = ?")];
            params.push(Value::Text(query.agent.clone()));
            push_scope(&query.scope, &mut clauses, &mut params);
            if !push_filters(&query, &mut clauses, &mut params, false)? {
                return Ok(0);
            }
            let count: i64 = query_optional(
                conn,
                &format!(
                    "SELECT count(*) FROM {} WHERE {}",
                    sql.runs,
                    clauses.join(" AND ")
                ),
                &params,
                |row| row.get(0).map_err(classify),
            )?
            .ok_or_else(|| StoreError::Corrupt("count returned no row".into()))?;
            u64::try_from(count).map_err(|_| StoreError::Corrupt(format!("negative count {count}")))
        })
        .await
    }

    async fn push_put(&self, new: NewPushConfig) -> StoreResult<PushRecord> {
        let t = now();
        let config = json_text_owned(new.config)?;
        let cursor = json_text_owned(new.cursor)?;
        let run = new.run;
        self.blocking(move |conn, sql| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(classify)?;
            let exists: bool = query_optional(
                &tx,
                &format!("SELECT 1 FROM {} WHERE id = ?", sql.runs),
                &[Value::Text(run.to_string())],
                |row| row.get::<_, i64>(0).map_err(classify),
            )?
            .is_some();
            if !exists {
                return Err(StoreError::NotFound(run));
            }
            let record = query_optional(
                &tx,
                &format!(
                    "INSERT INTO {} (run_id, id, agent, owner, config, cursor, state, attempts,
                                     last_error, next_attempt_at, version, created_at, updated_at)
                     VALUES (?, ?, ?, ?, ?, ?, 'active', 0, NULL, ?, 1, ?, ?)
                     ON CONFLICT (run_id, id) DO UPDATE
                        SET agent = excluded.agent, owner = excluded.owner,
                            config = excluded.config, cursor = excluded.cursor,
                            state = 'active', attempts = 0, last_error = NULL,
                            next_attempt_at = excluded.next_attempt_at,
                            lease_owner = NULL, lease_until = NULL,
                            version = {push}.version + 1, updated_at = excluded.updated_at
                     RETURNING {PUSH_COLUMNS}",
                    sql.push,
                    push = sql.push
                ),
                &[
                    Value::Text(run.to_string()),
                    Value::Text(new.id.clone()),
                    Value::Text(new.agent.clone()),
                    Value::Text(new.owner.clone()),
                    Value::Text(config),
                    Value::Text(cursor),
                    Value::Integer(to_ms(t)),
                    Value::Integer(to_ms(t)),
                    Value::Integer(to_ms(t)),
                ],
                push_from_row,
            )?
            .ok_or_else(|| StoreError::Corrupt(format!("push config of run {run} vanished")))?;
            tx.commit().map_err(classify)?;
            Ok(record)
        })
        .await
    }

    async fn push_list(&self, run: RunId) -> StoreResult<Vec<PushRecord>> {
        self.blocking(move |conn, sql| {
            query_all(
                conn,
                &format!(
                    "SELECT {PUSH_COLUMNS} FROM {} WHERE run_id = ? ORDER BY id",
                    sql.push
                ),
                &[Value::Text(run.to_string())],
                push_from_row,
            )
        })
        .await
    }

    async fn push_delete(&self, run: RunId, id: &str) -> StoreResult<bool> {
        let id = id.to_owned();
        self.blocking(move |conn, sql| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(classify)?;
            let changed = execute(
                &tx,
                &format!("DELETE FROM {} WHERE run_id = ? AND id = ?", sql.push),
                &[Value::Text(run.to_string()), Value::Text(id)],
            )?;
            tx.commit().map_err(classify)?;
            Ok(changed == 1)
        })
        .await
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
        let agents = agents.to_vec();
        let worker = worker.to_owned();
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.blocking(move |conn, sql| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(classify)?;
            let mut params = Vec::new();
            let mut placeholders = String::new();
            for (index, agent) in agents.iter().enumerate() {
                if index > 0 {
                    placeholders.push_str(", ");
                }
                placeholders.push('?');
                params.push(Value::Text(agent.clone()));
            }
            params.push(Value::Integer(to_ms(now)));
            params.push(Value::Integer(to_ms(now)));
            params.push(Value::Integer(limit));
            let due = query_all(
                &tx,
                &format!(
                    "SELECT run_id, id FROM {}
                      WHERE agent IN ({placeholders}) AND state = 'active'
                        AND next_attempt_at <= ?
                        AND (lease_until IS NULL OR lease_until <= ?)
                      ORDER BY next_attempt_at, run_id, id LIMIT ?",
                    sql.push
                ),
                &params,
                |row| {
                    Ok((
                        row.get::<_, String>("run_id").map_err(classify)?,
                        row.get::<_, String>("id").map_err(classify)?,
                    ))
                },
            )?;
            let mut records = Vec::with_capacity(due.len());
            for (run_id, id) in due {
                execute(
                    &tx,
                    &format!(
                        "UPDATE {} SET lease_owner = ?, lease_until = ?
                          WHERE run_id = ? AND id = ?",
                        sql.push
                    ),
                    &[
                        Value::Text(worker.clone()),
                        Value::Integer(to_ms(until)),
                        Value::Text(run_id.clone()),
                        Value::Text(id.clone()),
                    ],
                )?;
                let record = query_optional(
                    &tx,
                    &format!(
                        "SELECT {PUSH_COLUMNS} FROM {} WHERE run_id = ? AND id = ?",
                        sql.push
                    ),
                    &[Value::Text(run_id), Value::Text(id)],
                    push_from_row,
                )?
                .ok_or_else(|| {
                    StoreError::Corrupt("a claimed push configuration vanished".into())
                })?;
                records.push(record);
            }
            tx.commit().map_err(classify)?;
            records.sort_by(|a, b| {
                (a.next_attempt_at, a.run, &a.id).cmp(&(b.next_attempt_at, b.run, &b.id))
            });
            Ok(records)
        })
        .await
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
        let cursor = json_text_owned(progress.cursor)?;
        let expected_i64 = to_i64(expected, "version")?;
        let state = progress.state.as_str().to_owned();
        let next_attempt_at = truncate_ms(progress.next_attempt_at);
        let updated = now();
        let id = id.to_owned();
        self.blocking(move |conn, sql| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(classify)?;
            let record = query_optional(
                &tx,
                &format!(
                    "UPDATE {} SET state = ?, cursor = ?, attempts = ?, last_error = ?,
                                  next_attempt_at = ?, version = version + 1, updated_at = ?,
                                  lease_owner = NULL, lease_until = NULL
                      WHERE run_id = ? AND id = ? AND version = ?
                     RETURNING {PUSH_COLUMNS}",
                    sql.push
                ),
                &[
                    Value::Text(state),
                    Value::Text(cursor),
                    Value::Integer(i64::from(attempts)),
                    opt_text(progress.last_error.as_deref()),
                    Value::Integer(to_ms(next_attempt_at)),
                    Value::Integer(to_ms(updated)),
                    Value::Text(run.to_string()),
                    Value::Text(id.clone()),
                    Value::Integer(expected_i64),
                ],
                push_from_row,
            )?;
            if let Some(record) = record {
                tx.commit().map_err(classify)?;
                return Ok(record);
            }
            let actual: Option<i64> = query_optional(
                &tx,
                &format!(
                    "SELECT version FROM {} WHERE run_id = ? AND id = ?",
                    sql.push
                ),
                &[Value::Text(run.to_string()), Value::Text(id)],
                |row| row.get(0).map_err(classify),
            )?;
            match actual {
                None => Err(StoreError::NotFound(run)),
                Some(actual) => Err(StoreError::Conflict {
                    run,
                    expected,
                    actual: u64::try_from(actual).unwrap_or(0),
                }),
            }
        })
        .await
    }

    async fn purge_finished(&self, agent: &str, before: Timestamp) -> StoreResult<u64> {
        let agent = agent.to_owned();
        let before = truncate_ms(before);
        self.blocking(move |conn, sql| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(classify)?;
            let mut total = 0_u64;
            loop {
                let done = execute(
                    &tx,
                    &format!(
                        "DELETE FROM {runs} WHERE id IN (
                            SELECT id FROM {runs}
                             WHERE agent = ? AND status IN ('done', 'failed') AND updated_at < ?
                             LIMIT ?)",
                        runs = sql.runs
                    ),
                    &[
                        Value::Text(agent.clone()),
                        Value::Integer(to_ms(before)),
                        Value::Integer(PURGE_BATCH),
                    ],
                )?;
                total += done as u64;
                if (done as i64) < PURGE_BATCH {
                    break;
                }
            }
            sweep_blobs(&tx, sql, before)?;
            tx.commit().map_err(classify)?;
            Ok(total)
        })
        .await
    }

    fn keeps_bodies(&self) -> bool {
        true
    }

    async fn body_list(&self, run: RunId, generation: Uuid) -> StoreResult<Vec<BodyRow>> {
        self.blocking(move |conn, sql| {
            query_all(
                conn,
                &format!(
                    "SELECT idx, message FROM {}
                      WHERE run_id = ? AND generation = ? ORDER BY idx",
                    sql.body
                ),
                &[
                    Value::Text(run.to_string()),
                    Value::Text(generation.to_string()),
                ],
                |row| {
                    let idx: i64 = row.get("idx").map_err(classify)?;
                    let idx = u32::try_from(idx)
                        .map_err(|_| StoreError::Corrupt(format!("negative body index {idx}")))?;
                    Ok(BodyRow {
                        idx,
                        message: json_value(&row.get::<_, String>("message").map_err(classify)?)?,
                    })
                },
            )
        })
        .await
    }

    async fn gc_body(
        &self,
        run: RunId,
        keep_generation: Uuid,
        before: Timestamp,
    ) -> StoreResult<u64> {
        let before = truncate_ms(before);
        self.blocking(move |conn, sql| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(classify)?;
            let changed = execute(
                &tx,
                &format!(
                    "DELETE FROM {} WHERE run_id = ? AND generation <> ? AND written_at < ?",
                    sql.body
                ),
                &[
                    Value::Text(run.to_string()),
                    Value::Text(keep_generation.to_string()),
                    Value::Integer(to_ms(before)),
                ],
            )?;
            tx.commit().map_err(classify)?;
            Ok(changed as u64)
        })
        .await
    }

    fn keeps_blobs(&self) -> bool {
        true
    }

    async fn put_blob(&self, id: &BlobId, bytes: &[u8]) -> StoreResult<()> {
        let digest = id.as_str().to_owned();
        let bytes = bytes.to_vec();
        let created = now();
        self.blocking(move |conn, sql| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(classify)?;
            let size = to_i64(bytes.len() as u64, "blob size")?;
            execute(
                &tx,
                &format!(
                    "INSERT OR IGNORE INTO {} (digest, bytes, size, created_at)
                     VALUES (?, ?, ?, ?)",
                    sql.blob
                ),
                &[
                    Value::Text(digest),
                    Value::Blob(bytes),
                    Value::Integer(size),
                    Value::Integer(to_ms(created)),
                ],
            )?;
            tx.commit().map_err(classify)
        })
        .await
    }

    async fn blob_stored(&self, id: &BlobId) -> StoreResult<bool> {
        let digest = id.as_str().to_owned();
        self.blocking(move |conn, sql| {
            Ok(query_optional(
                conn,
                &format!("SELECT 1 FROM {} WHERE digest = ?", sql.blob),
                &[Value::Text(digest)],
                |row| row.get::<_, i64>(0).map_err(classify),
            )?
            .is_some())
        })
        .await
    }

    async fn get_blob(&self, id: &BlobId) -> StoreResult<Option<Vec<u8>>> {
        let digest = id.as_str().to_owned();
        self.blocking(move |conn, sql| {
            query_optional(
                conn,
                &format!("SELECT bytes FROM {} WHERE digest = ?", sql.blob),
                &[Value::Text(digest)],
                |row| row.get::<_, Vec<u8>>(0).map_err(classify),
            )
        })
        .await
    }

    async fn purge_blobs(&self, before: Timestamp) -> StoreResult<u64> {
        let before = truncate_ms(before);
        self.blocking(move |conn, sql| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(classify)?;
            let changed = sweep_blobs(&tx, sql, before)?;
            tx.commit().map_err(classify)?;
            Ok(changed)
        })
        .await
    }
}

// ---------------------------------------------------------------------------------------------
// Rows, JSON, time and errors
// ---------------------------------------------------------------------------------------------

fn push_scope(scope: &ConversationScope, clauses: &mut Vec<String>, params: &mut Vec<Value>) {
    match scope {
        ConversationScope::Prefix(prefix) => {
            clauses.push(
                "conversation_id >= ? AND conversation_id < ? AND substr(conversation_id, 1, ?) = ?"
                    .into(),
            );
            params.push(Value::Text(prefix.clone()));
            params.push(Value::Text(prefix_end(prefix)));
            params.push(Value::Integer(prefix.chars().count() as i64));
            params.push(Value::Text(prefix.clone()));
        }
        ConversationScope::Exact(exact) => {
            clauses.push("conversation_id = ?".into());
            params.push(Value::Text(exact.clone()));
        }
    }
}

/// The status and time filters, and (when `with_after`) the keyset position. `false` means no row
/// can match (an empty status list).
fn push_filters(
    query: &RunQuery,
    clauses: &mut Vec<String>,
    params: &mut Vec<Value>,
    with_after: bool,
) -> StoreResult<bool> {
    if let Some(statuses) = &query.statuses {
        if statuses.is_empty() {
            return Ok(false);
        }
        let mut clause = String::from("status IN (");
        for (index, status) in statuses.iter().enumerate() {
            if index > 0 {
                clause.push_str(", ");
            }
            clause.push('?');
            params.push(Value::Text(status.as_str().to_owned()));
        }
        clause.push(')');
        clauses.push(clause);
    }
    if let Some(since) = query.updated_since {
        clauses.push("updated_at >= ?".into());
        params.push(Value::Integer(to_ms(since)));
    }
    if let Some((at, id)) = query.after
        && with_after
    {
        clauses.push("(updated_at < ? OR (updated_at = ? AND id < ?))".into());
        params.push(Value::Integer(to_ms(at)));
        params.push(Value::Integer(to_ms(at)));
        params.push(Value::Text(id.to_string()));
    }
    Ok(true)
}

/// The first text above every text that starts with `prefix`, in byte order.
fn prefix_end(prefix: &str) -> String {
    let mut end = prefix.to_owned();
    end.push(char::MAX);
    end
}

fn run_from_row(row: &Row) -> StoreResult<RunRecord> {
    let mut run = metadata_from_row(row)?;
    run.state = json_value(&row.get::<_, String>("state").map_err(classify)?)?;
    Ok(run)
}

fn metadata_from_row(row: &Row) -> StoreResult<RunRecord> {
    Ok(RunRecord {
        id: RunId(uuid_col(row, "id")?),
        agent: row.get("agent").map_err(classify)?,
        conversation_id: row.get("conversation_id").map_err(classify)?,
        parent_id: opt_uuid_col(row, "parent_id")?.map(RunId),
        status: run_status(row)?,
        state: Json::Null,
        wake_at: opt_time_col(row, "wake_at")?,
        version: version_col(row, "version")?,
        created_at: time_col(row, "created_at")?,
        updated_at: time_col(row, "updated_at")?,
    })
}

fn run_header_from_row(row: &Row) -> StoreResult<RunHeader> {
    Ok(RunHeader {
        id: RunId(uuid_col(row, "id")?),
        agent: row.get("agent").map_err(classify)?,
        conversation_id: row.get("conversation_id").map_err(classify)?,
        parent_id: opt_uuid_col(row, "parent_id")?.map(RunId),
        status: run_status(row)?,
        wake_at: opt_time_col(row, "wake_at")?,
        version: version_col(row, "version")?,
        created_at: time_col(row, "created_at")?,
        updated_at: time_col(row, "updated_at")?,
        lease_until: opt_time_col(row, "lease_until")?,
    })
}

fn push_from_row(row: &Row) -> StoreResult<PushRecord> {
    let state: String = row.get("state").map_err(classify)?;
    let attempts: i64 = row.get("attempts").map_err(classify)?;
    Ok(PushRecord {
        run: RunId(uuid_col(row, "run_id")?),
        id: row.get("id").map_err(classify)?,
        agent: row.get("agent").map_err(classify)?,
        owner: row.get("owner").map_err(classify)?,
        config: json_value(&row.get::<_, String>("config").map_err(classify)?)?,
        cursor: json_value(&row.get::<_, String>("cursor").map_err(classify)?)?,
        state: PushState::parse(&state)
            .ok_or_else(|| StoreError::Corrupt(format!("unknown push state {state:?}")))?,
        attempts: u32::try_from(attempts)
            .map_err(|_| StoreError::Corrupt(format!("negative attempts {attempts}")))?,
        last_error: row.get("last_error").map_err(classify)?,
        next_attempt_at: time_col(row, "next_attempt_at")?,
        version: version_col(row, "version")?,
        created_at: time_col(row, "created_at")?,
        updated_at: time_col(row, "updated_at")?,
    })
}

fn entry_from_row(row: &Row) -> StoreResult<JournalEntry> {
    let seq: i64 = row.get("seq").map_err(classify)?;
    Ok(JournalEntry {
        seq: u64::try_from(seq).map_err(|_| StoreError::Corrupt(format!("negative seq {seq}")))?,
        name: row.get("name").map_err(classify)?,
        ok: row.get::<_, i64>("ok").map_err(classify)? != 0,
        payload: json_value(&row.get::<_, String>("payload").map_err(classify)?)?,
        recorded_at: time_col(row, "recorded_at")?,
    })
}

fn run_status(row: &Row) -> StoreResult<RunStatus> {
    let status: String = row.get("status").map_err(classify)?;
    RunStatus::parse(&status)
        .ok_or_else(|| StoreError::Corrupt(format!("unknown run status {status:?}")))
}

fn version_col(row: &Row, column: &str) -> StoreResult<u64> {
    let value: i64 = row.get(column).map_err(classify)?;
    u64::try_from(value).map_err(|_| StoreError::Corrupt(format!("negative {column} {value}")))
}

fn time_col(row: &Row, column: &str) -> StoreResult<Timestamp> {
    let value: i64 = row.get(column).map_err(classify)?;
    from_ms(value)
}

fn opt_time_col(row: &Row, column: &str) -> StoreResult<Option<Timestamp>> {
    row.get::<_, Option<i64>>(column)
        .map_err(classify)?
        .map(from_ms)
        .transpose()
}

fn uuid_col(row: &Row, column: &str) -> StoreResult<Uuid> {
    let text: String = row.get(column).map_err(classify)?;
    Uuid::parse_str(&text)
        .map_err(|e| StoreError::Corrupt(format!("{column} {text:?} is not a UUID: {e}")))
}

fn opt_uuid_col(row: &Row, column: &str) -> StoreResult<Option<Uuid>> {
    row.get::<_, Option<String>>(column)
        .map_err(classify)?
        .map(|text| {
            Uuid::parse_str(&text)
                .map_err(|e| StoreError::Corrupt(format!("{column} {text:?} is not a UUID: {e}")))
        })
        .transpose()
}

/// A timestamp as the column holds it: whole milliseconds since the epoch, floored as
/// [`truncate_ms`] floors.
fn to_ms(t: Timestamp) -> i64 {
    truncate_ms(t).as_millisecond()
}

/// A millisecond count back to a [`Timestamp`], through seconds and nanoseconds: jiff's
/// `from_millisecond` refuses the last 999 ms of its own range, which [`to_ms`] may produce.
fn from_ms(ms: i64) -> StoreResult<Timestamp> {
    let second = ms.div_euclid(1_000);
    let nanosecond = i32::try_from(ms.rem_euclid(1_000) * 1_000_000)
        .map_err(|_| StoreError::Corrupt(format!("millisecond {ms} is out of range")))?;
    JiffTimestamp::new(second, nanosecond)
        .map(Timestamp)
        .map_err(|e| StoreError::Corrupt(format!("millisecond {ms} is out of range: {e}")))
}

fn json_text(value: &Json) -> StoreResult<String> {
    serde_json::to_string(storable(value).as_ref()).map_err(StoreError::internal)
}

fn json_text_owned(value: Json) -> StoreResult<String> {
    serde_json::to_string(&storable_owned(value)).map_err(StoreError::internal)
}

fn json_value(text: &str) -> StoreResult<Json> {
    serde_json::from_str(text).map_err(StoreError::corrupt_source)
}

fn opt_text(value: Option<&str>) -> Value {
    value.map_or(Value::Null, |value| Value::Text(value.to_owned()))
}

fn opt_integer(value: Option<i64>) -> Value {
    value.map_or(Value::Null, Value::Integer)
}

fn to_i64(n: u64, what: &str) -> StoreResult<i64> {
    i64::try_from(n).map_err(|_| StoreError::InvalidInput(format!("{what} {n} exceeds i64::MAX")))
}

fn query_optional<T>(
    conn: &Connection,
    text: &str,
    params: &[Value],
    read: impl FnOnce(&Row<'_>) -> StoreResult<T>,
) -> StoreResult<Option<T>> {
    let mut statement = conn.prepare(text).map_err(classify)?;
    let mut rows = statement
        .query(params_from_iter(params.iter()))
        .map_err(classify)?;
    match rows.next().map_err(classify)? {
        Some(row) => Ok(Some(read(row)?)),
        None => Ok(None),
    }
}

fn query_all<T>(
    conn: &Connection,
    text: &str,
    params: &[Value],
    mut read: impl FnMut(&Row<'_>) -> StoreResult<T>,
) -> StoreResult<Vec<T>> {
    let mut statement = conn.prepare(text).map_err(classify)?;
    let mut rows = statement
        .query(params_from_iter(params.iter()))
        .map_err(classify)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(classify)? {
        out.push(read(row)?);
    }
    Ok(out)
}

fn execute(conn: &Connection, text: &str, params: &[Value]) -> StoreResult<usize> {
    execute_raw(conn, text, params).map_err(classify)
}

/// [`execute`] with the driver error unmapped, for a statement whose constraint decides the
/// answer (`AlreadyExists`, `ConversationBusy`, first-writer-wins).
fn execute_raw(conn: &Connection, text: &str, params: &[Value]) -> Result<usize, rusqlite::Error> {
    conn.execute(text, params_from_iter(params.iter()))
}

fn sweep_blobs(conn: &Connection, sql: &Sql, before: Timestamp) -> StoreResult<u64> {
    let changed = execute(
        conn,
        &format!(
            "DELETE FROM {} WHERE created_at < ?
               AND digest NOT IN (SELECT digest FROM {})",
            sql.blob, sql.run_blob
        ),
        &[Value::Integer(to_ms(before))],
    )?;
    Ok(changed as u64)
}

fn unique_violation(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(e, _)
            if e.code == ErrorCode::ConstraintViolation
                && matches!(
                    e.extended_code,
                    rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
                        | rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY
                )
    )
}

fn foreign_key_violation(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(e, _)
            if e.code == ErrorCode::ConstraintViolation
                && e.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY
    )
}

fn map_create_error(err: rusqlite::Error, new: &NewRun) -> StoreError {
    if unique_violation(&err) {
        return match (&new.conversation_id, new.status.is_open()) {
            (Some(conversation_id), true) => StoreError::ConversationBusy {
                agent: new.agent.clone(),
                conversation_id: conversation_id.clone(),
            },
            _ => StoreError::AlreadyExists(new.id),
        };
    }
    if foreign_key_violation(&err) {
        return StoreError::NotFound(new.id);
    }
    classify(err)
}

fn classify(err: rusqlite::Error) -> StoreError {
    let class = match &err {
        rusqlite::Error::SqliteFailure(e, _) => match e.code {
            ErrorCode::DatabaseBusy
            | ErrorCode::DatabaseLocked
            | ErrorCode::OperationInterrupted
            | ErrorCode::SystemIoFailure
            | ErrorCode::DiskFull
            | ErrorCode::SchemaChanged => ErrorClass::Transient,
            ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase | ErrorCode::TypeMismatch => {
                ErrorClass::Corrupt
            }
            ErrorCode::ReadOnly | ErrorCode::CannotOpen | ErrorCode::PermissionDenied => {
                ErrorClass::Invalid
            }
            _ => ErrorClass::Internal,
        },
        rusqlite::Error::FromSqlConversionFailure(..)
        | rusqlite::Error::InvalidColumnType(..)
        | rusqlite::Error::InvalidColumnName(..)
        | rusqlite::Error::InvalidColumnIndex(..)
        | rusqlite::Error::Utf8Error(..) => ErrorClass::Corrupt,
        _ => ErrorClass::Internal,
    };
    StoreError::Backend {
        class,
        source: Box::new(err),
    }
}

/// The rows one commit writes and where they go, resolved inside its transaction.
struct BodyPlan {
    generation: Uuid,
    start: usize,
    writer: Uuid,
    rows: Vec<BodyRow>,
    written_at: Timestamp,
}

/// Resolve the generation and the rows a commit writes, and patch the head it carries.
fn plan_body(
    head: &mut Option<Json>,
    current_head: Option<&Json>,
    head_version: Option<i64>,
    expected: i64,
    body: BodyWrite,
    written_at: Timestamp,
) -> StoreResult<BodyPlan> {
    let BodyWrite {
        generation,
        rows,
        writer,
    } = body;
    for (position, row) in rows.iter().enumerate() {
        let position = u32::try_from(position)
            .map_err(|_| StoreError::InvalidInput("a transcript is longer than u32".into()))?;
        if row.idx != position {
            return Err(StoreError::InvalidInput(format!(
                "body row {position} has index {}",
                row.idx
            )));
        }
    }
    let current = if head_version == Some(expected) {
        current_head.and_then(head_body)
    } else {
        None
    };
    let target = generation
        .or(current.map(|(generation, _)| generation))
        .unwrap_or_else(Uuid::new_v4);
    let start = match (generation, current) {
        (Some(_), _) | (None, None) => 0,
        (None, Some((_, len))) => usize::try_from(len)
            .map_err(|_| StoreError::InvalidInput("body length exceeds usize".into()))?,
    };
    if start > rows.len() {
        return Err(StoreError::InvalidInput(format!(
            "the body append starts at {start}, past the {} rows given",
            rows.len()
        )));
    }
    let len = u32::try_from(rows.len())
        .map_err(|_| StoreError::InvalidInput("a transcript is longer than u32".into()))?;
    if let Some(head) = head.as_mut() {
        set_head_body(head, target, len)?;
    }
    Ok(BodyPlan {
        generation: target,
        start,
        writer,
        rows,
        written_at,
    })
}

/// Write the rows of one body plan inside the commit's transaction, resolving a slot another
/// writer left: an identical message is adopted, one older than [`BODY_ORPHAN_GRACE`] is
/// collected (once), anything else is a `Conflict`.
fn write_body(
    conn: &Connection,
    sql: &Sql,
    id: RunId,
    plan: &BodyPlan,
    expected: u64,
) -> StoreResult<()> {
    let rows = &plan.rows[plan.start..];
    if rows.is_empty() {
        return Ok(());
    }
    let written_at = to_ms(plan.written_at);
    let from = i64::try_from(plan.start)
        .map_err(|_| StoreError::InvalidInput("body start exceeds i64".into()))?;
    let to = i64::try_from(plan.rows.len())
        .map_err(|_| StoreError::InvalidInput("a transcript is longer than i64".into()))?;
    for pass in 0..2 {
        for row in rows {
            execute(
                conn,
                &format!(
                    "INSERT OR IGNORE INTO {} (run_id, generation, idx, message, writer, written_at)
                     VALUES (?, ?, ?, ?, ?, ?)",
                    sql.body
                ),
                &[
                    Value::Text(id.to_string()),
                    Value::Text(plan.generation.to_string()),
                    Value::Integer(i64::from(row.idx)),
                    Value::Text(json_text(&row.message)?),
                    Value::Text(plan.writer.to_string()),
                    Value::Integer(written_at),
                ],
            )?;
        }
        let conflicts = query_all(
            conn,
            &format!(
                "SELECT idx, message, written_at FROM {}
                  WHERE run_id = ? AND generation = ? AND idx >= ? AND idx < ?
                    AND writer <> ?",
                sql.body
            ),
            &[
                Value::Text(id.to_string()),
                Value::Text(plan.generation.to_string()),
                Value::Integer(from),
                Value::Integer(to),
                Value::Text(plan.writer.to_string()),
            ],
            |row| {
                Ok((
                    row.get::<_, i64>("idx").map_err(classify)?,
                    row.get::<_, String>("message").map_err(classify)?,
                    row.get::<_, i64>("written_at").map_err(classify)?,
                ))
            },
        )?;
        if conflicts.is_empty() {
            return Ok(());
        }
        let cutoff = plan.written_at - BODY_ORPHAN_GRACE;
        let mut orphans = Vec::new();
        for (index, message, at) in conflicts {
            let position = usize::try_from(index)
                .map_err(|_| StoreError::Corrupt(format!("negative body index {index}")))?;
            let expected_row = plan.rows.get(position).ok_or_else(|| {
                StoreError::Corrupt(format!("body row {index} is outside the transcript"))
            })?;
            let message = json_value(&message)?;
            if &message == storable(&expected_row.message).as_ref() {
                continue;
            }
            if from_ms(at)? < cutoff {
                orphans.push(index);
            } else {
                return Err(StoreError::Conflict {
                    run: id,
                    expected,
                    actual: expected,
                });
            }
        }
        if orphans.is_empty() {
            return Ok(());
        }
        if pass == 1 {
            return Err(StoreError::Corrupt(format!(
                "body generation {} of run {id}: a collected slot is still occupied",
                plan.generation
            )));
        }
        for index in orphans {
            execute(
                conn,
                &format!(
                    "DELETE FROM {} WHERE run_id = ? AND generation = ? AND idx = ?
                      AND writer <> ?",
                    sql.body
                ),
                &[
                    Value::Text(id.to_string()),
                    Value::Text(plan.generation.to_string()),
                    Value::Integer(index),
                    Value::Text(plan.writer.to_string()),
                ],
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The column is whole milliseconds and floors, as [`truncate_ms`] floors: a value before 1970
    /// with a fractional millisecond becomes the millisecond below, never toward zero. (The
    /// conformance suite cannot see this through the public API: `NewRun::wake_at`,
    /// `RunUpdate::wake_at` and `JournalEntry::ok` already truncate before the store is called.)
    #[test]
    fn a_pre_1970_sub_millisecond_is_floored() {
        let before: Timestamp = "1969-12-31T23:59:59.999500Z".parse().expect("a timestamp");
        let floored: Timestamp = "1969-12-31T23:59:59.999Z".parse().expect("a timestamp");
        assert_eq!(to_ms(before), -1, "floored, never toward zero");
        assert_eq!(from_ms(-1).expect("a millisecond"), floored);
    }
}
