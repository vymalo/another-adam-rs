//! MongoDB implementation of [`adam_core::Store`].
//!
//! # Collections
//!
//! Three collections (names prefixed, `adam_` by default):
//!
//! * `adam_runs`: one document per run. `_id` is the run's UUID (BSON binary
//!   subtype 4), `state` is a real BSON document (see [`codec`] for how keys
//!   like `$ref` are escaped), so runs are queryable from `mongosh`.
//! * `adam_journal`: one document per recorded step, `_id` = `"<run>:<seq>"`.
//! * `adam_push` (schema version 3): one document per A2A push-notification configuration,
//!   `_id` = `"<run>:<config id>"` (a run id is 36 characters, so the join is unambiguous).
//!   `config` holds the webhook **with its credentials, as the client gave them**; `cursor` is how
//!   far delivery got. Deleted with its run by the purge.
//!
//! # Concurrency without transactions
//!
//! Every operation is a single-document atomic write, so this store works on a
//! **standalone `mongod`** as well as replica sets and Atlas; no multi-document
//! transactions are used.
//!
//! * Commits: `findOneAndUpdate({_id, version: expected}, {$inc: {version: 1}, ..})`.
//! * Journal: `insertOne` with a deterministic `_id`; a duplicate-key error
//!   means another writer won, and their entry is returned.
//! * Claiming: read a page of due candidates, then `updateMany` them with the
//!   due/lease conditions re-checked in the filter and a fresh `lease_token`,
//!   then read back what carries that token. MongoDB re-evaluates the filter
//!   per document under its write lock, so two workers can't both lease a run.
//! * Owner: a pinned claim (`ClaimScope::Pinned`) adds `owner: null or worker` to the same
//!   filter and `$set`s `owner` in the same `updateMany`. `{ owner: null }` matches a missing
//!   field, so runs written before schema version 2 need no migration. Releasing a lease
//!   leaves `owner` alone.
//! * Push configurations: claiming is a loop of `findOneAndUpdate` (filter: active, due, no live
//!   lease; sort: `next_attempt_at`), each one atomic on its document; progress is
//!   `findOneAndUpdate({_id, version: expected}, ..)`; putting an id again is an update that
//!   bumps `version`, or an insert at 1 when there is none.
//! * One open run per conversation: every run has an `open_key` with a plain
//!   unique index. Open runs with a conversation use
//!   `open_conversation_key(agent, conversation)`; every other run uses
//!   `"~<run id>"`, which is unique by construction. This avoids partial or
//!   sparse indexes, which not every MongoDB-compatible server supports.

pub mod codec;

use std::time::Duration;

use adam_core::store::{add_ttl, now, open_conversation_key, sched_at, truncate_ms};
use adam_core::{
    ClaimScope, ConversationScope, JournalEntry, Lease, NewPushConfig, NewRun, PushProgress,
    PushRecord, PushState, RunId, RunQuery, RunRecord, RunStatus, RunUpdate, Store, StoreError,
    StoreResult,
};
use adam_error::ErrorClass;
use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use futures::TryStreamExt;
use mongodb::bson::{self, Bson, Document, doc};
use mongodb::error::{ErrorKind, WriteFailure};
use mongodb::options::{
    CollectionOptions, IndexOptions, ReadPreference, ReturnDocument, SelectionCriteria,
};
use mongodb::{Client, Collection, Database, IndexModel};
use uuid::Uuid;

use codec::{bson_to_json, json_to_bson};

/// Current schema version written to the `<prefix>meta` collection.
///
/// * 1: the first schema.
/// * 2: `owner` on runs (see [`ClaimScope`]). A missing field reads as no owner, so nothing
///   is rewritten; the number only says which release last migrated.
/// * 3: the `push` collection (A2A push-notification configurations and their delivery progress)
///   and the index `ListTasks` reads runs by (`adam_list`).
pub const SCHEMA_VERSION: i32 = 3;

const DUPLICATE_KEY: i32 = 11000;
const OPEN_CONVERSATION_INDEX: &str = "adam_open_conversation";
/// Purge deletes in batches of this many runs.
const PURGE_BATCH: i64 = 500;

/// A [`Store`] backed by MongoDB (5.0+, standalone or replica set).
#[derive(Clone, Debug)]
pub struct MongoStore {
    db: Database,
    prefix: String,
    runs: Collection<Document>,
    journal: Collection<Document>,
    push: Collection<Document>,
}

impl MongoStore {
    /// Connect to `uri` and use database `db`. Call [`Store::migrate`] before first use.
    pub async fn connect(uri: &str, db: &str) -> StoreResult<Self> {
        let client = Client::with_uri_str(uri).await.map_err(classify)?;
        Ok(Self::new(client.database(db)))
    }

    /// Use an existing database handle, e.g. the one your app already has.
    pub fn new(db: Database) -> Self {
        Self::build(db, "adam_".to_owned())
    }

    /// Use a different collection-name prefix (default `adam_`).
    pub fn with_collection_prefix(self, prefix: &str) -> StoreResult<Self> {
        let valid = !prefix.is_empty()
            && prefix.len() <= 40
            && prefix
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if !valid {
            return Err(StoreError::InvalidInput(format!(
                "invalid collection prefix {prefix:?}"
            )));
        }
        Ok(Self::build(self.db, prefix.to_owned()))
    }

    pub fn database(&self) -> &Database {
        &self.db
    }

    fn build(db: Database, prefix: String) -> Self {
        // Every read must see this store's own preceding writes (claim
        // read-back, journal winner lookup), so pin reads to the primary even
        // if the connection string prefers secondaries.
        let primary = CollectionOptions::builder()
            .selection_criteria(SelectionCriteria::ReadPreference(ReadPreference::Primary))
            .build();
        let runs = db.collection_with_options(&format!("{prefix}runs"), primary.clone());
        let journal = db.collection_with_options(&format!("{prefix}journal"), primary.clone());
        let push = db.collection_with_options(&format!("{prefix}push"), primary);
        Self {
            db,
            prefix,
            runs,
            journal,
            push,
        }
    }

    /// Whether the run exists and is not being purged.
    async fn run_exists(&self, id: RunId) -> StoreResult<bool> {
        let found = self
            .runs
            .find_one(doc! { "_id": uuid(id), "purging": { "$ne": true } })
            .projection(doc! { "_id": 1 })
            .await
            .map_err(classify)?;
        Ok(found.is_some())
    }

    /// `_id`s of the runs matching `filter`.
    async fn ids(&self, filter: Document, limit: Option<i64>) -> StoreResult<Vec<Bson>> {
        let mut find = self.runs.find(filter).projection(doc! { "_id": 1 });
        if let Some(limit) = limit {
            find = find.limit(limit);
        }
        let docs: Vec<Document> = find
            .await
            .map_err(classify)?
            .try_collect()
            .await
            .map_err(classify)?;
        Ok(docs
            .into_iter()
            .filter_map(|d| d.get("_id").cloned())
            .collect())
    }

    /// A live run: present and not tombstoned by an in-progress purge.
    async fn load_live(&self, id: RunId) -> StoreResult<Option<RunRecord>> {
        let found = self
            .runs
            .find_one(doc! { "_id": uuid(id), "purging": { "$ne": true } })
            .await
            .map_err(classify)?;
        found.as_ref().map(run_from_doc).transpose()
    }

    async fn find_journal(&self, run: RunId, seq: u64) -> StoreResult<Option<JournalEntry>> {
        let found = self
            .journal
            .find_one(doc! { "_id": journal_id(run, seq) })
            .await
            .map_err(classify)?;
        found.as_ref().map(entry_from_doc).transpose()
    }
}

// ---------------------------------------------------------------------------
// Encoding helpers

fn uuid(id: RunId) -> Bson {
    Bson::from(bson::Uuid::from_bytes(id.0.into_bytes()))
}

fn date(t: DateTime<Utc>) -> Bson {
    Bson::DateTime(bson::DateTime::from_millis(t.timestamp_millis()))
}

fn opt_date(t: Option<DateTime<Utc>>) -> Bson {
    t.map(date).unwrap_or(Bson::Null)
}

fn journal_id(run: RunId, seq: u64) -> String {
    format!("{run}:{seq}")
}

/// The filters of a [`RunQuery`] (not its position or its limit), without runs a purge has
/// tombstoned.
fn run_query_filter(query: &RunQuery) -> Document {
    let mut filter = doc! { "agent": &query.agent, "purging": { "$ne": true } };
    match &query.scope {
        // An anchored, escaped prefix is a range over the index `(agent, conversation_id, ..)`.
        ConversationScope::Prefix(prefix) => {
            filter.insert(
                "conversation_id",
                doc! { "$regex": format!("^{}", regex_escape(prefix)) },
            );
        }
        ConversationScope::Exact(conversation) => {
            filter.insert("conversation_id", conversation);
        }
    }
    if let Some(statuses) = &query.statuses {
        let statuses: Vec<&str> = statuses.iter().map(|s| s.as_str()).collect();
        filter.insert("status", doc! { "$in": statuses });
    }
    if let Some(since) = query.updated_since {
        filter.insert("updated_at", doc! { "$gte": date(truncate_ms(since)) });
    }
    filter
}

/// `text` as a regular expression that matches exactly that text: every character that is not a
/// letter, a digit or `_` is escaped.
fn regex_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    for c in text.chars() {
        if c.is_alphanumeric() || c == '_' {
            out.push(c);
        } else {
            out.push('\\');
            out.push(c);
        }
    }
    out
}

fn push_key(run: RunId, id: &str) -> String {
    format!("{run}:{id}")
}

fn push_from_doc(d: &Document) -> StoreResult<PushRecord> {
    let state = get_str(d, "state")?.ok_or_else(|| bad("state"))?;
    Ok(PushRecord {
        run: get_uuid(d, "run_id")?.ok_or_else(|| bad("run_id"))?,
        id: get_str(d, "id")?.ok_or_else(|| bad("id"))?,
        agent: get_str(d, "agent")?.ok_or_else(|| bad("agent"))?,
        owner: get_str(d, "owner")?.ok_or_else(|| bad("owner"))?,
        config: bson_to_json(d.get("config").ok_or_else(|| bad("config"))?)?,
        cursor: bson_to_json(d.get("cursor").ok_or_else(|| bad("cursor"))?)?,
        state: PushState::parse(&state).ok_or_else(|| bad("state"))?,
        attempts: u32::try_from(get_u64(d, "attempts")?).map_err(|_| bad("attempts"))?,
        last_error: get_str(d, "last_error")?,
        next_attempt_at: get_date(d, "next_attempt_at")?.ok_or_else(|| bad("next_attempt_at"))?,
        version: get_u64(d, "version")?,
        created_at: get_date(d, "created_at")?.ok_or_else(|| bad("created_at"))?,
        updated_at: get_date(d, "updated_at")?.ok_or_else(|| bad("updated_at"))?,
    })
}

fn open_key(id: RunId, agent: &str, conversation: Option<&str>, status: RunStatus) -> String {
    match conversation {
        Some(conv) if status.is_open() => open_conversation_key(agent, conv),
        _ => format!("~{id}"),
    }
}

fn to_i64(n: u64, what: &str) -> StoreResult<i64> {
    i64::try_from(n).map_err(|_| StoreError::InvalidInput(format!("{what} {n} exceeds i64::MAX")))
}

fn bad(field: &str) -> StoreError {
    StoreError::Corrupt(format!(
        "missing or malformed field {field:?} in stored document"
    ))
}

fn get_uuid(d: &Document, field: &str) -> StoreResult<Option<RunId>> {
    match d.get(field) {
        None | Some(Bson::Null) => Ok(None),
        Some(Bson::Binary(b)) => Uuid::from_slice(&b.bytes)
            .map(|u| Some(RunId(u)))
            .map_err(|_| bad(field)),
        Some(_) => Err(bad(field)),
    }
}

fn get_date(d: &Document, field: &str) -> StoreResult<Option<DateTime<Utc>>> {
    match d.get(field) {
        None | Some(Bson::Null) => Ok(None),
        Some(Bson::DateTime(t)) => Utc
            .timestamp_millis_opt(t.timestamp_millis())
            .single()
            .map(Some)
            .ok_or_else(|| bad(field)),
        Some(_) => Err(bad(field)),
    }
}

fn get_str(d: &Document, field: &str) -> StoreResult<Option<String>> {
    match d.get(field) {
        None | Some(Bson::Null) => Ok(None),
        Some(Bson::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(bad(field)),
    }
}

fn get_u64(d: &Document, field: &str) -> StoreResult<u64> {
    let n = match d.get(field) {
        Some(Bson::Int64(n)) => *n,
        Some(Bson::Int32(n)) => i64::from(*n),
        _ => return Err(bad(field)),
    };
    u64::try_from(n).map_err(|_| bad(field))
}

fn run_from_doc(d: &Document) -> StoreResult<RunRecord> {
    let status = get_str(d, "status")?.ok_or_else(|| bad("status"))?;
    Ok(RunRecord {
        id: get_uuid(d, "_id")?.ok_or_else(|| bad("_id"))?,
        agent: get_str(d, "agent")?.ok_or_else(|| bad("agent"))?,
        conversation_id: get_str(d, "conversation_id")?,
        parent_id: get_uuid(d, "parent_id")?,
        status: RunStatus::parse(&status).ok_or_else(|| bad("status"))?,
        state: bson_to_json(d.get("state").ok_or_else(|| bad("state"))?)?,
        wake_at: get_date(d, "wake_at")?,
        version: get_u64(d, "version")?,
        created_at: get_date(d, "created_at")?.ok_or_else(|| bad("created_at"))?,
        updated_at: get_date(d, "updated_at")?.ok_or_else(|| bad("updated_at"))?,
    })
}

fn entry_from_doc(d: &Document) -> StoreResult<JournalEntry> {
    Ok(JournalEntry {
        seq: get_u64(d, "seq")?,
        name: get_str(d, "name")?.ok_or_else(|| bad("name"))?,
        ok: d.get_bool("ok").map_err(|_| bad("ok"))?,
        payload: bson_to_json(d.get("payload").ok_or_else(|| bad("payload"))?)?,
        recorded_at: get_date(d, "recorded_at")?.ok_or_else(|| bad("recorded_at"))?,
    })
}

/// Decide what a driver error means to the caller and box it as the source.
///
/// Network, DNS, pool and server-selection failures, and errors the server labels transient
/// or retryable, may succeed later: `Transient`. A reply that cannot be decoded is `Corrupt`.
/// A refused login is `Unauthenticated`, a rejected argument or TLS setup is `Invalid`, and
/// any other rejected command is `Internal`: repeating it cannot help.
fn classify(err: mongodb::error::Error) -> StoreError {
    let class = if err.contains_label("TransientTransactionError")
        || err.contains_label("RetryableWriteError")
    {
        ErrorClass::Transient
    } else {
        match err.kind.as_ref() {
            ErrorKind::Io(_)
            | ErrorKind::DnsResolve { .. }
            | ErrorKind::ConnectionPoolCleared { .. }
            | ErrorKind::ServerSelection { .. }
            | ErrorKind::Transaction { .. } => ErrorClass::Transient,
            ErrorKind::BsonDeserialization(_) | ErrorKind::InvalidResponse { .. } => {
                ErrorClass::Corrupt
            }
            ErrorKind::Authentication { .. } => ErrorClass::Unauthenticated,
            ErrorKind::InvalidArgument { .. } | ErrorKind::InvalidTlsConfig { .. } => {
                ErrorClass::Invalid
            }
            _ => ErrorClass::Internal,
        }
    };
    StoreError::Backend {
        class,
        source: Box::new(err),
    }
}

fn is_duplicate_key(err: &mongodb::error::Error) -> bool {
    match err.kind.as_ref() {
        ErrorKind::Write(WriteFailure::WriteError(e)) => e.code == DUPLICATE_KEY,
        ErrorKind::Command(e) => e.code == DUPLICATE_KEY,
        ErrorKind::InsertMany(e) => e
            .write_errors
            .as_ref()
            .is_some_and(|errs| errs.iter().any(|w| w.code == DUPLICATE_KEY)),
        _ => false,
    }
}

// ---------------------------------------------------------------------------

#[async_trait]
impl Store for MongoStore {
    async fn migrate(&self) -> StoreResult<()> {
        let unique = |name: &str| {
            IndexOptions::builder()
                .name(name.to_owned())
                .unique(true)
                .build()
        };
        let named = |name: &str| IndexOptions::builder().name(name.to_owned()).build();
        let run_indexes = [
            // Claiming: due runs per agent, earliest first.
            IndexModel::builder()
                .keys(doc! { "agent": 1, "sched_at": 1, "_id": 1 })
                .options(named("adam_due"))
                .build(),
            // At most one open run per conversation (see the crate docs).
            IndexModel::builder()
                .keys(doc! { "open_key": 1 })
                .options(unique(OPEN_CONVERSATION_INDEX))
                .build(),
            // Retention sweeps.
            IndexModel::builder()
                .keys(doc! { "agent": 1, "status": 1, "updated_at": 1 })
                .options(named("adam_finished"))
                .build(),
            IndexModel::builder()
                .keys(doc! { "parent_id": 1 })
                .options(named("adam_parent"))
                .build(),
            // Listing an owner's runs, newest first (`ListTasks`): an anchored prefix on
            // `conversation_id` is a range over this index.
            IndexModel::builder()
                .keys(doc! { "agent": 1, "conversation_id": 1, "updated_at": -1, "_id": -1 })
                .options(named("adam_list"))
                .build(),
        ];
        let journal_indexes = [IndexModel::builder()
            .keys(doc! { "run_id": 1, "seq": 1 })
            .options(named("adam_journal_run"))
            .build()];
        let push_indexes = [
            // Claiming: due configs per agent, earliest first. The run's own configs are `_id`
            // prefixed, listed by `run_id`.
            IndexModel::builder()
                .keys(doc! { "agent": 1, "state": 1, "next_attempt_at": 1 })
                .options(named("adam_push_due"))
                .build(),
            IndexModel::builder()
                .keys(doc! { "run_id": 1, "id": 1 })
                .options(named("adam_push_run"))
                .build(),
        ];
        ensure_indexes(&self.runs, run_indexes).await?;
        ensure_indexes(&self.journal, journal_indexes).await?;
        ensure_indexes(&self.push, push_indexes).await?;
        self.db
            .collection::<Document>(&format!("{}meta", self.prefix))
            .update_one(
                doc! { "_id": "schema_version" },
                doc! { "$setOnInsert": { "value": SCHEMA_VERSION } },
            )
            .upsert(true)
            .await
            .map_err(classify)?;
        // Raise an older version; never lower a newer one.
        self.db
            .collection::<Document>(&format!("{}meta", self.prefix))
            .update_one(
                doc! { "_id": "schema_version", "value": { "$lt": SCHEMA_VERSION } },
                doc! { "$set": { "value": SCHEMA_VERSION } },
            )
            .await
            .map_err(classify)?;
        Ok(())
    }

    async fn create_run(&self, new: NewRun) -> StoreResult<RunRecord> {
        let t = now();
        let wake_at = new.wake_at.map(truncate_ms);
        let doc = doc! {
            "_id": uuid(new.id),
            "agent": &new.agent,
            "conversation_id": new.conversation_id.as_deref().map(Bson::from).unwrap_or(Bson::Null),
            "parent_id": new.parent_id.map(uuid).unwrap_or(Bson::Null),
            "status": new.status.as_str(),
            "state": json_to_bson(&new.state)?,
            "wake_at": opt_date(wake_at),
            "sched_at": opt_date(sched_at(new.status, wake_at, t)),
            "version": 1_i64,
            "open_key": open_key(new.id, &new.agent, new.conversation_id.as_deref(), new.status),
            "lease_owner": Bson::Null,
            "lease_until": Bson::Null,
            "lease_token": Bson::Null,
            "owner": Bson::Null,
            "created_at": date(t),
            "updated_at": date(t),
        };
        match self.runs.insert_one(&doc).await {
            Ok(_) => run_from_doc(&doc),
            Err(err) if is_duplicate_key(&err) => {
                // The server names the violated index in the message; fall back
                // to a lookup for servers that word it differently.
                let msg = err.to_string();
                let busy = if msg.contains(OPEN_CONVERSATION_INDEX) {
                    true
                } else if msg.contains("_id_") {
                    false
                } else {
                    !self.run_exists(new.id).await?
                };
                if !busy {
                    Err(StoreError::AlreadyExists(new.id))
                } else {
                    Err(StoreError::ConversationBusy {
                        agent: new.agent,
                        conversation_id: new.conversation_id.unwrap_or_default(),
                    })
                }
            }
            Err(err) => Err(classify(err)),
        }
    }

    async fn load_run(&self, id: RunId) -> StoreResult<Option<RunRecord>> {
        let found = self
            .runs
            .find_one(doc! { "_id": uuid(id) })
            .await
            .map_err(classify)?;
        found.as_ref().map(run_from_doc).transpose()
    }

    async fn commit_run(
        &self,
        id: RunId,
        expected: u64,
        update: RunUpdate,
    ) -> StoreResult<RunRecord> {
        // open_key depends on agent and conversation, which never change, so
        // read them from the current document. If the CAS below fails the read
        // was pointless, but a commit that loses a race is the rare path.
        let current = self.load_live(id).await?.ok_or(StoreError::NotFound(id))?;
        if current.version != expected {
            return Err(StoreError::Conflict {
                run: id,
                expected,
                actual: current.version,
            });
        }
        let t = now();
        let wake_at = update.wake_at.map(truncate_ms);
        let set = doc! {
            "status": update.status.as_str(),
            "state": json_to_bson(&update.state)?,
            "wake_at": opt_date(wake_at),
            "sched_at": opt_date(sched_at(update.status, wake_at, t)),
            "open_key": open_key(id, &current.agent, current.conversation_id.as_deref(), update.status),
            "updated_at": date(t),
        };
        let result = self
            .runs
            .find_one_and_update(
                doc! {
                    "_id": uuid(id),
                    "version": to_i64(expected, "version")?,
                    // A tombstoned run is being purged; it must not come back to life.
                    "purging": { "$ne": true },
                },
                doc! { "$set": set, "$inc": { "version": 1_i64 } },
            )
            .return_document(ReturnDocument::After)
            .await;
        match result {
            Ok(Some(doc)) => run_from_doc(&doc),
            Ok(None) => match self.load_live(id).await? {
                None => Err(StoreError::NotFound(id)),
                Some(run) => Err(StoreError::Conflict {
                    run: id,
                    expected,
                    actual: run.version,
                }),
            },
            Err(err) if is_duplicate_key(&err) => Err(StoreError::ConversationBusy {
                agent: current.agent,
                conversation_id: current.conversation_id.unwrap_or_default(),
            }),
            Err(err) => Err(classify(err)),
        }
    }

    async fn open_run_for_conversation(
        &self,
        agent: &str,
        conversation_id: &str,
    ) -> StoreResult<Option<RunRecord>> {
        let found = self
            .runs
            .find_one(doc! { "open_key": open_conversation_key(agent, conversation_id) })
            .await
            .map_err(classify)?;
        found.as_ref().map(run_from_doc).transpose()
    }

    async fn journal_get(&self, run: RunId, seq: u64) -> StoreResult<Option<JournalEntry>> {
        self.find_journal(run, seq).await
    }

    async fn journal_put(&self, run: RunId, entry: JournalEntry) -> StoreResult<JournalEntry> {
        if let Some(existing) = self.find_journal(run, entry.seq).await? {
            return check_same_step(run, existing, entry);
        }
        if !self.run_exists(run).await? {
            return Err(StoreError::NotFound(run));
        }
        let recorded_at = truncate_ms(entry.recorded_at);
        let doc = doc! {
            "_id": journal_id(run, entry.seq),
            "run_id": uuid(run),
            "seq": to_i64(entry.seq, "journal seq")?,
            "name": &entry.name,
            "ok": entry.ok,
            "payload": json_to_bson(&entry.payload)?,
            "recorded_at": date(recorded_at),
        };
        match self.journal.insert_one(&doc).await {
            Ok(_) => {
                // The run check above and the insert are not atomic: if a purge
                // removed the run in between, don't leave an orphan behind for a
                // future run with the same (deterministic) id to replay.
                if !self.run_exists(run).await? {
                    self.journal
                        .delete_one(doc! { "_id": journal_id(run, entry.seq) })
                        .await
                        .map_err(classify)?;
                    return Err(StoreError::NotFound(run));
                }
                entry_from_doc(&doc)
            }
            Err(err) if is_duplicate_key(&err) => {
                let existing = self.find_journal(run, entry.seq).await?.ok_or_else(|| {
                    StoreError::Corrupt(format!("journal entry {run}/{} vanished", entry.seq))
                })?;
                check_same_step(run, existing, entry)
            }
            Err(err) => Err(classify(err)),
        }
    }

    async fn journal_list(&self, run: RunId) -> StoreResult<Vec<JournalEntry>> {
        let docs: Vec<Document> = self
            .journal
            .find(doc! { "run_id": uuid(run) })
            .sort(doc! { "seq": 1 })
            .await
            .map_err(classify)?
            .try_collect()
            .await
            .map_err(classify)?;
        docs.iter().map(entry_from_doc).collect()
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
        let busy: Vec<Bson> = busy.iter().map(|id| uuid(*id)).collect();
        let mut claimable = doc! {
            "agent": { "$in": agents },
            "_id": { "$nin": &busy },
            "sched_at": { "$lte": date(now) },
            "$or": [ { "lease_until": Bson::Null }, { "lease_until": { "$lte": date(now) } } ],
        };
        let token = Uuid::now_v7().to_string();
        let mut set = doc! {
            "lease_owner": worker,
            "lease_until": date(until),
            "lease_token": &token,
        };
        match scope {
            ClaimScope::Any => {}
            ClaimScope::Pinned => {
                // A second `$or` needs `$and`. `{ owner: null }` also matches a missing field.
                claimable.insert(
                    "$and",
                    vec![doc! { "$or": [ { "owner": Bson::Null }, { "owner": worker } ] }],
                );
                set.insert("owner", worker);
            }
        }
        let mut claimed = 0_usize;
        let mut tried: Vec<Bson> = Vec::new();
        // A few rounds, because candidates read in step 1 can be taken by
        // another worker before step 2; retrying fills the page when more due
        // runs are available.
        for _ in 0..3 {
            let want = limit - claimed;
            let candidates: Vec<Bson> = self
                .runs
                .find(claimable.clone())
                .sort(doc! { "sched_at": 1, "_id": 1 })
                .limit(i64::try_from(want).unwrap_or(i64::MAX))
                .projection(doc! { "_id": 1 })
                .await
                .map_err(classify)?
                .try_collect::<Vec<Document>>()
                .await
                .map_err(classify)?
                .into_iter()
                .filter_map(|d| d.get("_id").cloned())
                .collect();
            if candidates.is_empty() {
                break;
            }
            let mut filter = claimable.clone();
            filter.insert("_id", doc! { "$in": &candidates });
            tried.extend(candidates.iter().cloned());
            let updated = self
                .runs
                .update_many(filter, doc! { "$set": &set })
                .await
                .map_err(classify)?;
            claimed += usize::try_from(updated.modified_count).unwrap_or(usize::MAX);
            if claimed >= limit || updated.modified_count as usize == candidates.len() {
                break;
            }
        }
        if claimed == 0 {
            return Ok(Vec::new());
        }
        let docs: Vec<Document> = self
            .runs
            .find(doc! { "_id": { "$in": tried }, "lease_token": &token, "lease_owner": worker })
            .sort(doc! { "sched_at": 1, "_id": 1 })
            .await
            .map_err(classify)?
            .try_collect()
            .await
            .map_err(classify)?;
        docs.iter()
            .map(|d| {
                Ok(Lease {
                    run: run_from_doc(d)?,
                    worker: worker.to_owned(),
                    until,
                })
            })
            .collect()
    }

    async fn renew_lease(
        &self,
        id: RunId,
        worker: &str,
        now: DateTime<Utc>,
        ttl: Duration,
    ) -> StoreResult<bool> {
        let now = truncate_ms(now);
        let result = self
            .runs
            .update_one(
                doc! { "_id": uuid(id), "lease_owner": worker, "lease_until": { "$gt": date(now) } },
                doc! { "$set": { "lease_until": date(add_ttl(now, ttl)) } },
            )
            .await
            .map_err(classify)?;
        Ok(result.matched_count == 1)
    }

    async fn release_lease(&self, id: RunId, worker: &str) -> StoreResult<()> {
        self.runs
            .update_one(
                doc! { "_id": uuid(id), "lease_owner": worker },
                doc! { "$set": { "lease_owner": Bson::Null, "lease_until": Bson::Null, "lease_token": Bson::Null } },
            )
            .await
            .map_err(classify)?;
        Ok(())
    }

    async fn lease_until(&self, id: RunId) -> StoreResult<Option<DateTime<Utc>>> {
        let found = self
            .runs
            .find_one(doc! { "_id": uuid(id) })
            .await
            .map_err(classify)?;
        found.map_or(Ok(None), |d| get_date(&d, "lease_until"))
    }

    async fn list_runs(&self, query: &RunQuery) -> StoreResult<Vec<RunRecord>> {
        // `limit(0)` means "no limit" to MongoDB, and a page of none is none.
        if query.limit == 0 {
            return Ok(Vec::new());
        }
        let mut filter = run_query_filter(query);
        if let Some((at, id)) = query.after {
            let at = date(truncate_ms(at));
            filter.insert(
                "$or",
                vec![
                    doc! { "updated_at": { "$lt": at.clone() } },
                    doc! { "updated_at": at, "_id": { "$lt": uuid(id) } },
                ],
            );
        }
        let docs: Vec<Document> = self
            .runs
            .find(filter)
            .sort(doc! { "updated_at": -1, "_id": -1 })
            .limit(i64::try_from(query.limit).unwrap_or(i64::MAX))
            .await
            .map_err(classify)?
            .try_collect()
            .await
            .map_err(classify)?;
        docs.iter().map(run_from_doc).collect()
    }

    async fn count_runs(&self, query: &RunQuery) -> StoreResult<u64> {
        self.runs
            .count_documents(run_query_filter(query))
            .await
            .map_err(classify)
    }

    async fn push_put(&self, new: NewPushConfig) -> StoreResult<PushRecord> {
        if !self.run_exists(new.run).await? {
            return Err(StoreError::NotFound(new.run));
        }
        let key = push_key(new.run, &new.id);
        let t = now();
        let fresh = doc! {
            "agent": &new.agent,
            "owner": &new.owner,
            "config": json_to_bson(&new.config)?,
            "cursor": json_to_bson(&new.cursor)?,
            "state": PushState::Active.as_str(),
            "attempts": 0_i64,
            "last_error": Bson::Null,
            "next_attempt_at": date(t),
            "lease_owner": Bson::Null,
            "lease_until": Bson::Null,
            "updated_at": date(t),
        };
        let mut stored = None;
        // Replace an existing config, or insert it. Two puts of one id can race, so a duplicate
        // key on insert means "it exists now": go round once more and replace it.
        for _ in 0..2 {
            let replaced = self
                .push
                .find_one_and_update(
                    doc! { "_id": &key },
                    doc! { "$set": fresh.clone(), "$inc": { "version": 1_i64 } },
                )
                .return_document(ReturnDocument::After)
                .await
                .map_err(classify)?;
            if let Some(found) = replaced {
                stored = Some(found);
                break;
            }
            let mut inserted = fresh.clone();
            inserted.insert("_id", &key);
            inserted.insert("run_id", uuid(new.run));
            inserted.insert("id", &new.id);
            inserted.insert("version", 1_i64);
            inserted.insert("created_at", date(t));
            match self.push.insert_one(&inserted).await {
                Ok(_) => {
                    stored = Some(inserted);
                    break;
                }
                Err(err) if is_duplicate_key(&err) => {}
                Err(err) => return Err(classify(err)),
            }
        }
        let stored = stored.ok_or_else(|| {
            StoreError::Corrupt(format!(
                "push config {key} neither exists nor can be created"
            ))
        })?;
        // The run check above and the write are not atomic: a purge that removed the run in
        // between must not leave an orphan for a future run with the same id.
        if !self.run_exists(new.run).await? {
            self.push
                .delete_one(doc! { "_id": &key })
                .await
                .map_err(classify)?;
            return Err(StoreError::NotFound(new.run));
        }
        push_from_doc(&stored)
    }

    async fn push_list(&self, run: RunId) -> StoreResult<Vec<PushRecord>> {
        let docs: Vec<Document> = self
            .push
            .find(doc! { "run_id": uuid(run) })
            .sort(doc! { "id": 1 })
            .await
            .map_err(classify)?
            .try_collect()
            .await
            .map_err(classify)?;
        docs.iter().map(push_from_doc).collect()
    }

    async fn push_delete(&self, run: RunId, id: &str) -> StoreResult<bool> {
        let done = self
            .push
            .delete_one(doc! { "_id": push_key(run, id) })
            .await
            .map_err(classify)?;
        Ok(done.deleted_count == 1)
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
        let until = add_ttl(now, ttl);
        let claimable = doc! {
            "agent": { "$in": agents },
            "state": PushState::Active.as_str(),
            "next_attempt_at": { "$lte": date(now) },
            "$or": [ { "lease_until": Bson::Null }, { "lease_until": { "$lte": date(now) } } ],
        };
        let mut claimed = Vec::new();
        for _ in 0..limit {
            // One atomic claim per document; the filter is re-checked under the document's lock,
            // so two workers never lease the same config.
            let found = self
                .push
                .find_one_and_update(
                    claimable.clone(),
                    doc! { "$set": { "lease_owner": worker, "lease_until": date(until) } },
                )
                .sort(doc! { "next_attempt_at": 1, "_id": 1 })
                .return_document(ReturnDocument::After)
                .await
                .map_err(classify)?;
            match found {
                Some(found) => claimed.push(push_from_doc(&found)?),
                None => break,
            }
        }
        Ok(claimed)
    }

    async fn push_commit(
        &self,
        run: RunId,
        id: &str,
        expected: u64,
        progress: PushProgress,
    ) -> StoreResult<PushRecord> {
        let key = push_key(run, id);
        let set = doc! {
            "state": progress.state.as_str(),
            "cursor": json_to_bson(&progress.cursor)?,
            "attempts": i64::from(progress.attempts),
            "last_error": progress.last_error.as_deref().map(Bson::from).unwrap_or(Bson::Null),
            "next_attempt_at": date(truncate_ms(progress.next_attempt_at)),
            "lease_owner": Bson::Null,
            "lease_until": Bson::Null,
            "updated_at": date(now()),
        };
        let committed = self
            .push
            .find_one_and_update(
                doc! { "_id": &key, "version": to_i64(expected, "version")? },
                doc! { "$set": set, "$inc": { "version": 1_i64 } },
            )
            .return_document(ReturnDocument::After)
            .await
            .map_err(classify)?;
        if let Some(committed) = committed {
            return push_from_doc(&committed);
        }
        let current = self
            .push
            .find_one(doc! { "_id": &key })
            .await
            .map_err(classify)?;
        match current {
            None => Err(StoreError::NotFound(run)),
            Some(current) => Err(StoreError::Conflict {
                run,
                expected,
                actual: get_u64(&current, "version")?,
            }),
        }
    }

    async fn purge_finished(&self, agent: &str, before: DateTime<Utc>) -> StoreResult<u64> {
        // `updated_at < before` at millisecond precision, rounding `before` up so
        // a sub-millisecond cutoff selects the same runs as in the other stores.
        let mut cutoff = before.timestamp_millis();
        if !before.timestamp_subsec_nanos().is_multiple_of(1_000_000) {
            cutoff += 1;
        }
        let finished = doc! {
            "agent": agent,
            "status": { "$in": ["done", "failed"] },
            "updated_at": { "$lt": Bson::DateTime(bson::DateTime::from_millis(cutoff)) },
        };
        let mut total = 0;
        loop {
            let ids = self.ids(finished.clone(), Some(PURGE_BATCH)).await?;
            if ids.is_empty() {
                return Ok(total);
            }
            // 1. Tombstone. From here on commit_run treats these runs as gone, so
            //    none can be re-opened after its journal is deleted. The filter
            //    re-checks "finished", so a run re-opened since the find above is
            //    left alone.
            let mut filter = finished.clone();
            filter.insert("_id", doc! { "$in": &ids });
            self.runs
                .update_many(filter, doc! { "$set": { "purging": true } })
                .await
                .map_err(classify)?;
            let doomed = self
                .ids(doc! { "_id": { "$in": &ids }, "purging": true }, None)
                .await?;
            // 2. Journal, then 3. runs. A crash between steps leaves tombstoned
            //    runs, which the next sweep picks up again.
            self.journal
                .delete_many(doc! { "run_id": { "$in": &doomed } })
                .await
                .map_err(classify)?;
            self.push
                .delete_many(doc! { "run_id": { "$in": &doomed } })
                .await
                .map_err(classify)?;
            let deleted = self
                .runs
                .delete_many(doc! { "_id": { "$in": &doomed }, "purging": true })
                .await
                .map_err(classify)?;
            total += deleted.deleted_count;
            if (ids.len() as i64) < PURGE_BATCH {
                return Ok(total);
            }
        }
    }
}

/// Create the indexes that don't exist yet. Re-issuing `createIndexes` for
/// existing indexes is a no-op on MongoDB, but skipping them saves a round of
/// catalog work on every boot and sidesteps servers that handle it poorly.
async fn ensure_indexes<const N: usize>(
    coll: &Collection<Document>,
    models: [IndexModel; N],
) -> StoreResult<()> {
    const NAMESPACE_NOT_FOUND: i32 = 26;
    let existing = match coll.list_index_names().await {
        Ok(names) => names,
        Err(err) if matches!(err.kind.as_ref(), ErrorKind::Command(c) if c.code == NAMESPACE_NOT_FOUND) => {
            Vec::new()
        }
        Err(err) => return Err(classify(err)),
    };
    let missing: Vec<IndexModel> = models
        .into_iter()
        .filter(|m| {
            let name = m.options.as_ref().and_then(|o| o.name.as_deref());
            !name.is_some_and(|n| existing.iter().any(|e| e == n))
        })
        .collect();
    if !missing.is_empty() {
        coll.create_indexes(missing).await.map_err(classify)?;
    }
    Ok(())
}

fn check_same_step(
    run: RunId,
    existing: JournalEntry,
    requested: JournalEntry,
) -> StoreResult<JournalEntry> {
    if existing.name != requested.name {
        return Err(StoreError::NonDeterminism {
            run,
            seq: requested.seq,
            recorded: existing.name,
            requested: requested.name,
        });
    }
    Ok(existing)
}

#[cfg(test)]
mod classify_tests {
    use super::*;
    use adam_error::Classify;

    fn class_of(kind: impl Into<ErrorKind>) -> ErrorClass {
        classify(mongodb::error::Error::from(kind.into())).class()
    }

    #[test]
    fn driver_errors_are_classified_by_what_they_mean() {
        let io = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        assert_eq!(class_of(io), ErrorClass::Transient);
        let decode = bson::from_document::<u32>(doc! {}).unwrap_err();
        assert_eq!(class_of(decode), ErrorClass::Corrupt);
        assert_eq!(
            class_of(ErrorKind::SessionsNotSupported),
            ErrorClass::Internal
        );
        assert_eq!(
            classify(mongodb::error::Error::custom("x")).class(),
            ErrorClass::Internal
        );
    }

    #[test]
    fn the_driver_error_is_kept_as_the_source() {
        let e = classify(std::io::Error::from(std::io::ErrorKind::TimedOut).into());
        assert!(e.is_retryable());
        assert!(std::error::Error::source(&e).is_some_and(|s| s.is::<mongodb::error::Error>()));
    }
}
