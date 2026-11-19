//! MongoDB implementation of [`adam_core::Store`].
//!
//! # Collections
//!
//! Five collections (names prefixed, `adam_` by default):
//!
//! * `adam_runs`: one document per run. `_id` is the run's UUID (BSON binary
//!   subtype 4), `state` is a real BSON document (see [`codec`] for how keys
//!   like `$ref` are escaped), so runs are queryable from `mongosh`.
//! * `adam_journal`: one document per recorded step, `_id` = `"<run>:<seq>"`.
//! * `adam_push` (schema version 3): one document per A2A push-notification configuration,
//!   `_id` = `"<run>:<config id>"` (a run id is 36 characters, so the join is unambiguous).
//!   `config` holds the webhook **with its credentials, as the client gave them**; `cursor` is how
//!   far delivery got. Deleted with its run by the purge.
//! * `adam_blob` (schema version 5, part 1): file bytes out of the run. `_id` is the digest (64
//!   lower-case hex), `bytes` the content, `size` its length, `created_at` when it was first
//!   written. Global and content-addressed: every run that names a digest shares it.
//! * `adam_run_blob` (schema version 5, part 1): the append-only set of digests one run
//!   references, `_id` = `"<run>:<digest>"`, deleted with its run by the purge.
//! * `adam_body` (schema version 5, part 2): one document per transcript row, `_id` =
//!   `"<run>:<commit>:<idx>"`, `message` the row as BSON, `written_at` when the set was written.
//!   A commit names its set by the `commit` its head carries; a set no head names is ignored and
//!   collected an hour after it was superseded. Deleted with its run by the purge.
//!
//! A run document written before schema version 5, part 2 has neither a `head` nor a
//! `head_version` field. `{head_version: null}` matches a missing field, as `owner` did for
//! version 2, so no document is rewritten by the migration.
//!
//! # Concurrency without transactions
//!
//! Every operation is a single-document atomic write, so this store works on a
//! **standalone `mongod`** as well as replica sets and Atlas; no multi-document
//! transactions are used.
//!
//! * Commits: `findOneAndUpdate({_id, version: expected}, {$inc: {version: 1}, ..})`, after a
//!   read projected to the metadata (no `state`) for `agent`/`conversation_id` and the record.
//!   A commit's blob references and transcript rows are inserted **before** the CAS: rows a lost
//!   CAS leaves behind are ignored (nothing names them) and go with the run's purge, or with the
//!   superseded-set GC an hour later. The body insert is checked for completeness first — a row
//!   that did not land aborts the commit before the CAS, so a head never names a partial set.
//! * Journal: `insertOne` with a deterministic `_id = "<run>:<seq>"`; a duplicate key means an
//!   earlier writer won, and their entry is read back and returned. After an insert, one check
//!   that the run still exists (a purge that removed it must not leave an orphan), so a journaled
//!   step is two round trips rather than the four of read, check, insert, check.
//! * Blobs: `insertOne` on `_id = <digest>`; a duplicate key is a no-op, because the bytes behind
//!   a digest never change. The references are one unordered `insertMany`, duplicates ignored.
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

use std::collections::HashSet;
use std::time::Duration;

use adam_core::store::{
    add_ttl, head_body, now, open_conversation_key, sched_at, set_head_body, storable,
    storable_owned, truncate_ms,
};
use adam_core::{
    BlobId, BodyRow, BodyWrite, ClaimScope, ConversationScope, JournalEntry, Lease, NewPushConfig,
    NewRun, PushProgress, PushRecord, PushState, RunHeader, RunId, RunQuery, RunRecord, RunStatus,
    RunUpdate, Store, StoreError, StoreResult,
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
use serde_json::Value;
use uuid::Uuid;

use codec::{bson_to_json, json_to_bson};

/// Current schema version written to the `<prefix>meta` collection.
///
/// * 1: the first schema.
/// * 2: `owner` on runs (see [`ClaimScope`]). A missing field reads as no owner, so nothing
///   is rewritten; the number only says which release last migrated.
/// * 3: the `push` collection (A2A push-notification configurations and their delivery progress)
///   and the index `ListTasks` reads runs by (`adam_list`).
/// * 4: `adam_list_status`, the same listing with `status` before the sort keys, so a status
///   filter is answered from the index. `migrate()` creates a missing index on every run, so a
///   version 3 database gets it without a rewrite.
/// * 5, part 1: file bytes out of the run. `adam_blob` (content-addressed) and `adam_run_blob`
///   (the digests a run references). Nothing is rewritten: a version 4 document has no blob field
///   at all.
/// * 5, part 2: the shadow. `head` and `head_version` on a run document, and `adam_body` (one
///   document per transcript row). Nothing is rewritten either: a version 4 document has neither
///   field, and a commit that writes no head leaves the stored one alone.
pub const SCHEMA_VERSION: i32 = 5;

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
    blobs: Collection<Document>,
    run_blobs: Collection<Document>,
    body: Collection<Document>,
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
        let push = db.collection_with_options(&format!("{prefix}push"), primary.clone());
        let blobs = db.collection_with_options(&format!("{prefix}blob"), primary.clone());
        let run_blobs = db.collection_with_options(&format!("{prefix}run_blob"), primary.clone());
        let body = db.collection_with_options(&format!("{prefix}body"), primary);
        Self {
            db,
            prefix,
            runs,
            journal,
            push,
            blobs,
            run_blobs,
            body,
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

    /// The fields `commit_run` reads from the current document before its CAS: no `state`, which
    /// the CAS does not need and a commit must not transfer.
    async fn commit_context(&self, id: RunId) -> StoreResult<Option<CommitContext>> {
        let found = self
            .runs
            .find_one(doc! { "_id": uuid(id), "purging": { "$ne": true } })
            .projection(doc! {
                "agent": 1, "conversation_id": 1, "parent_id": 1, "version": 1, "created_at": 1,
                "head": 1, "head_version": 1
            })
            .await
            .map_err(classify)?;
        found.as_ref().map(commit_context_from_doc).transpose()
    }

    /// The version of a live run, or `None` if it is gone (or tombstoned by a purge): what a failed
    /// CAS reports, without reading the document.
    async fn live_version(&self, id: RunId) -> StoreResult<Option<u64>> {
        let found = self
            .runs
            .find_one(doc! { "_id": uuid(id), "purging": { "$ne": true } })
            .projection(doc! { "version": 1 })
            .await
            .map_err(classify)?;
        found.as_ref().map(|d| get_u64(d, "version")).transpose()
    }

    async fn find_journal(&self, run: RunId, seq: u64) -> StoreResult<Option<JournalEntry>> {
        let found = self
            .journal
            .find_one(doc! { "_id": journal_id(run, seq) })
            .await
            .map_err(classify)?;
        found.as_ref().map(entry_from_doc).transpose()
    }

    /// Append the blob references of `run`, ignoring the digests it already references: the set is
    /// append-only, and a commit that is retried writes each row once.
    async fn insert_run_blobs(&self, run: RunId, digests: &[BlobId]) -> StoreResult<()> {
        let mut seen = HashSet::new();
        let docs: Vec<Document> = digests
            .iter()
            .filter(|digest| seen.insert(digest.as_str()))
            .map(|digest| {
                doc! {
                    "_id": run_blob_id(run, digest),
                    "run_id": uuid(run),
                    "digest": digest.as_str(),
                }
            })
            .collect();
        match self.run_blobs.insert_many(docs).ordered(false).await {
            Ok(_) => Ok(()),
            Err(err) if only_duplicate_keys(&err) => Ok(()),
            Err(err) => Err(classify(err)),
        }
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

/// The `_id` of one run's reference to a digest: a run id is 36 characters, so the join is
/// unambiguous.
fn run_blob_id(run: RunId, digest: &BlobId) -> String {
    format!("{run}:{digest}")
}

/// A body generation id as BSON.
fn generation_uuid(generation: Uuid) -> Bson {
    Bson::from(bson::Uuid::from_bytes(generation.into_bytes()))
}

/// The `_id` of one body row: `(run, generation, idx)`, the generation's identity and the row's
/// position.
fn body_id(run: RunId, generation: Uuid, idx: u32) -> String {
    format!("{run}:{generation}:{idx}")
}

/// How long a superseded body generation stays readable before the next commit of its run collects
/// it.
const BODY_GC_GRACE: chrono::Duration = chrono::Duration::hours(1);

/// One row as the document `adam_body` holds.
fn body_doc(
    run: RunId,
    generation: Uuid,
    writer: Uuid,
    row: &BodyRow,
    written_at: DateTime<Utc>,
) -> StoreResult<Document> {
    let idx = i32::try_from(row.idx).map_err(|_| {
        StoreError::InvalidInput(format!("body index {} exceeds i32::MAX", row.idx))
    })?;
    Ok(doc! {
        "_id": body_id(run, generation, row.idx),
        "run_id": uuid(run),
        "generation": generation_uuid(generation),
        "writer": generation_uuid(writer),
        "idx": idx,
        "message": json_to_bson(&storable(&row.message))?,
        "written_at": date(written_at),
    })
}

/// Whether a body insert landed every row: `inserted + duplicate_keys == expected`.
///
/// A duplicate key is a row that is already there (a retry of the same attempt); any other failure
/// means a row is missing, and the commit must abort **before** the CAS so a head never names a
/// generation that is missing a row.
fn body_insert_is_complete(inserted: usize, duplicates: usize, expected: usize) -> bool {
    inserted + duplicates == expected
}

/// The `(inserted, duplicates)` of an unordered `insert_many` that failed, from the write errors it
/// reports. `None` when the error is not an `InsertMany` answer, or when the server reported a
/// **write concern** error: the write may or may not have landed, so nothing can be said about what
/// is complete, and the commit must abort before the CAS.
fn body_insert_counts(err: &mongodb::error::Error, expected: usize) -> Option<(usize, usize)> {
    let ErrorKind::InsertMany(failure) = err.kind.as_ref() else {
        return None;
    };
    if failure.write_concern_error.is_some() {
        return None;
    }
    let errors = failure.write_errors.as_ref()?;
    let duplicates = errors
        .iter()
        .filter(|write| write.code == DUPLICATE_KEY)
        .count();
    Some((expected.saturating_sub(errors.len()), duplicates))
}
/// The digests a v5 head's file artifacts name: what a losing commit must not collect.
fn head_digests(head: &Value) -> HashSet<BlobId> {
    head.get("artifacts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|artifact| {
            artifact
                .get("file")?
                .get("digest")?
                .as_str()
                .and_then(BlobId::from_hex)
        })
        .collect()
}

/// The filters of a [`RunQuery`] (not its position or its limit).
///
/// A run a purge has tombstoned is not filtered out: `purging` has no index, so checking it here
/// would make the listing fetch every candidate document (state included) to test one boolean. The
/// tombstone is short-lived but not instant: the sweep deletes a batch of them after deleting their
/// journals and push configs, and a crash in between leaves them until the next sweep.
/// `commit_run` still refuses to revive one.
fn run_query_filter(query: &RunQuery) -> Document {
    let mut filter = doc! { "agent": &query.agent };
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

/// A document of [`MongoStore::run_header`]'s projection: the record's metadata and its lease,
/// never `state`.
fn run_header_from_doc(d: &Document) -> StoreResult<RunHeader> {
    let status = get_str(d, "status")?.ok_or_else(|| bad("status"))?;
    Ok(RunHeader {
        id: get_uuid(d, "_id")?.ok_or_else(|| bad("_id"))?,
        agent: get_str(d, "agent")?.ok_or_else(|| bad("agent"))?,
        conversation_id: get_str(d, "conversation_id")?,
        parent_id: get_uuid(d, "parent_id")?,
        status: RunStatus::parse(&status).ok_or_else(|| bad("status"))?,
        wake_at: get_date(d, "wake_at")?,
        version: get_u64(d, "version")?,
        created_at: get_date(d, "created_at")?.ok_or_else(|| bad("created_at"))?,
        updated_at: get_date(d, "updated_at")?.ok_or_else(|| bad("updated_at"))?,
        lease_until: get_date(d, "lease_until")?,
    })
}

/// The fields [`MongoStore::commit_context`] reads: everything `commit_run` needs to build the
/// record it returns and the `open_key`, and nothing else.
struct CommitContext {
    version: u64,
    agent: String,
    conversation_id: Option<String>,
    parent_id: Option<RunId>,
    created_at: DateTime<Utc>,
    /// The v5 head, when one was written; a commit that appends reads its `body` generation.
    head: Option<serde_json::Value>,
    /// The version the head was written at; a stale head names no current generation.
    head_version: Option<u64>,
}

fn commit_context_from_doc(d: &Document) -> StoreResult<CommitContext> {
    let head = match d.get("head") {
        None | Some(Bson::Null) => None,
        Some(head) => Some(bson_to_json(head)?),
    };
    let head_version = match d.get("head_version") {
        None | Some(Bson::Null) => None,
        Some(_) => Some(get_u64(d, "head_version")?),
    };
    Ok(CommitContext {
        version: get_u64(d, "version")?,
        agent: get_str(d, "agent")?.ok_or_else(|| bad("agent"))?,
        conversation_id: get_str(d, "conversation_id")?,
        parent_id: get_uuid(d, "parent_id")?,
        created_at: get_date(d, "created_at")?.ok_or_else(|| bad("created_at"))?,
        head,
        head_version,
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

/// Whether every write of an unordered `insert_many` failed only because the document is already
/// there: the operation's answer for "the rows I asked for exist".
fn only_duplicate_keys(err: &mongodb::error::Error) -> bool {
    match err.kind.as_ref() {
        ErrorKind::InsertMany(e) => e
            .write_errors
            .as_ref()
            .is_some_and(|errs| !errs.is_empty() && errs.iter().all(|w| w.code == DUPLICATE_KEY)),
        _ => is_duplicate_key(err),
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
            // `conversation_id` is a range over this index, and one conversation is an equality,
            // which the index's `updated_at, _id` order then serves. A prefix is a range, so the
            // sort cannot come from the index and is a blocking one over the matching entries.
            IndexModel::builder()
                .keys(doc! { "agent": 1, "conversation_id": 1, "updated_at": -1, "_id": -1 })
                .options(named("adam_list"))
                .build(),
            // The same listing with a status filter. `status` before `conversation_id` makes it an
            // equality bound, so the index yields the runs of that status and the prefix is a
            // range inside it: a page fetches its own rows instead of every candidate (measured
            // with `explain` on MongoDB 7, 2 000 runs: 10 documents examined for a page of 10,
            // against 500 fetched to test the filter without it). A listing of **one**
            // conversation is read in index order, no sort; a **prefix** listing still sorts its
            // matches (the range breaks the order), which is the default `ListTasks` path.
            IndexModel::builder()
                .keys(doc! { "agent": 1, "status": 1, "conversation_id": 1, "updated_at": -1, "_id": -1 })
                .options(named("adam_list_status"))
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
        // Schema version 5, part 1: the blob sweep asks "which blobs were written before the
        // cutoff", its reference check asks "does any run name this digest", and a purge (or a
        // lost-CAS cleanup) asks "which digests does this run name".
        let blob_indexes = [IndexModel::builder()
            .keys(doc! { "created_at": 1 })
            .options(named("adam_blob_gc"))
            .build()];
        let run_blob_indexes = [
            IndexModel::builder()
                .keys(doc! { "digest": 1 })
                .options(named("adam_run_blob_digest"))
                .build(),
            IndexModel::builder()
                .keys(doc! { "run_id": 1 })
                .options(named("adam_run_blob_run"))
                .build(),
        ];
        // Schema version 5, part 2: a generation is read by `(run, generation)` in index order, and
        // the GC asks "which generations of this run were written before the cutoff". The name is
        // new because `ensure_indexes` matches by name only: a database migrated from the earlier
        // S2a shape keeps its old `adam_body_run` index without the new keys.
        let body_indexes = [
            IndexModel::builder()
                .keys(doc! { "run_id": 1, "generation": 1, "idx": 1 })
                .options(named("adam_body_generation"))
                .build(),
            IndexModel::builder()
                .keys(doc! { "written_at": 1 })
                .options(named("adam_body_gc"))
                .build(),
        ];
        ensure_indexes(&self.runs, run_indexes).await?;
        ensure_indexes(&self.journal, journal_indexes).await?;
        ensure_indexes(&self.push, push_indexes).await?;
        ensure_indexes(&self.blobs, blob_indexes).await?;
        ensure_indexes(&self.run_blobs, run_blob_indexes).await?;
        ensure_indexes(&self.body, body_indexes).await?;
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
        let NewRun {
            id,
            agent,
            conversation_id,
            parent_id,
            status,
            state,
            wake_at,
        } = new;
        let wake_at = wake_at.map(truncate_ms);
        let state = storable_owned(state);
        let doc = doc! {
            "_id": uuid(id),
            "agent": &agent,
            "conversation_id": conversation_id.as_deref().map(Bson::from).unwrap_or(Bson::Null),
            "parent_id": parent_id.map(uuid).unwrap_or(Bson::Null),
            "status": status.as_str(),
            "state": json_to_bson(&state)?,
            "wake_at": opt_date(wake_at),
            "sched_at": opt_date(sched_at(status, wake_at, t)),
            "version": 1_i64,
            "open_key": open_key(id, &agent, conversation_id.as_deref(), status),
            "lease_owner": Bson::Null,
            "lease_until": Bson::Null,
            "lease_token": Bson::Null,
            "owner": Bson::Null,
            "created_at": date(t),
            "updated_at": date(t),
        };
        match self.runs.insert_one(&doc).await {
            Ok(_) => Ok(RunRecord {
                id,
                agent,
                conversation_id,
                parent_id,
                status,
                // The contract of `create_run`: the caller has the state it wrote.
                state: serde_json::Value::Null,
                wake_at,
                version: 1,
                created_at: t,
                updated_at: t,
            }),
            Err(err) if is_duplicate_key(&err) => {
                // The server names the violated index in the message; fall back
                // to a lookup for servers that word it differently.
                let msg = err.to_string();
                let busy = if msg.contains(OPEN_CONVERSATION_INDEX) {
                    true
                } else if msg.contains("_id_") {
                    false
                } else {
                    !self.run_exists(id).await?
                };
                if !busy {
                    Err(StoreError::AlreadyExists(id))
                } else {
                    Err(StoreError::ConversationBusy {
                        agent,
                        conversation_id: conversation_id.unwrap_or_default(),
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

    async fn load_head(&self, id: RunId) -> StoreResult<Option<RunRecord>> {
        let found = self
            .runs
            .find_one(doc! { "_id": uuid(id) })
            .await
            .map_err(classify)?;
        let Some(doc) = found else {
            return Ok(None);
        };
        let mut rec = run_from_doc(&doc)?;
        // A missing or null `head_version` is a run no commit has written a head for; a head
        // written at an older version was superseded by a commit that did not rebuild it, and is
        // stale. Either way the whole state is served, and the next step re-splits.
        let head_version = match doc.get("head_version") {
            None | Some(Bson::Null) => None,
            Some(_) => Some(get_u64(&doc, "head_version")?),
        };
        if head_version == Some(rec.version)
            && let Some(head) = doc.get("head")
        {
            rec.state = bson_to_json(head)?;
        }
        Ok(Some(rec))
    }

    async fn run_header(&self, id: RunId) -> StoreResult<Option<RunHeader>> {
        // One document, projected to the header and the lease: the state is never read.
        let found = self
            .runs
            .find_one(doc! { "_id": uuid(id) })
            .projection(doc! {
                "agent": 1, "conversation_id": 1, "parent_id": 1, "status": 1, "wake_at": 1,
                "version": 1, "created_at": 1, "updated_at": 1, "lease_until": 1
            })
            .await
            .map_err(classify)?;
        found.as_ref().map(run_header_from_doc).transpose()
    }

    async fn commit_run(
        &self,
        id: RunId,
        expected: u64,
        update: RunUpdate,
    ) -> StoreResult<RunRecord> {
        // open_key depends on agent and conversation, which never change, so read them from the
        // current document, projected to the metadata the CAS and the returned record need: a
        // commit never reads the state it is about to replace. If the CAS below fails the read was
        // pointless, but a commit that loses a race is the rare path.
        let current = self
            .commit_context(id)
            .await?
            .ok_or(StoreError::NotFound(id))?;
        if current.version != expected {
            return Err(StoreError::Conflict {
                run: id,
                expected,
                actual: current.version,
            });
        }
        let t = now();
        let RunUpdate {
            status,
            state,
            head,
            body,
            blobs,
            wake_at,
        } = update;
        let wake_at = wake_at.map(truncate_ms);
        let state = storable_owned(state);
        // The generation this commit writes to: a compaction names a new one, an append keeps the
        // current head's, and a run with no current head starts one.
        let current_generation = if current.head_version == Some(expected) {
            current.head.as_ref().and_then(head_body)
        } else {
            None
        };
        let mut body_plan = None;
        let mut head = head;
        if let Some(body) = body {
            let BodyWrite {
                generation,
                rows,
                writer,
            } = body;
            let target = generation
                .or(current_generation.map(|(generation, _)| generation))
                .unwrap_or_else(Uuid::new_v4);
            // A new generation writes the whole transcript; an append writes only the rows after
            // the head's `len`. The runtime guarantees the rows start with the old transcript.
            let start = match (generation, current_generation) {
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
            if let Some(head) = &mut head {
                set_head_body(head, target, len)?;
            }
            body_plan = Some((target, start, writer, rows, t));
        }
        // The rows go first, as the head that names them would: there is no multi-document
        // transaction, so a commit that loses the CAS deletes the rows its `writer` inserted. The
        // completeness check is what keeps a head from naming a generation missing a row.
        if let Some((generation, start, writer, rows, written_at)) = &body_plan {
            let docs = rows
                .iter()
                .skip(*start)
                .map(|row| body_doc(id, *generation, *writer, row, *written_at))
                .collect::<StoreResult<Vec<_>>>()?;
            if !docs.is_empty() {
                let expected = docs.len();
                match self.body.insert_many(&docs).ordered(false).await {
                    Ok(result) => {
                        if !body_insert_is_complete(result.inserted_ids.len(), 0, expected) {
                            return Err(StoreError::Corrupt(format!(
                                "body generation {generation} of run {id}: only {} of {expected} rows landed",
                                result.inserted_ids.len()
                            )));
                        }
                    }
                    Err(err) => match body_insert_counts(&err, expected) {
                        // Every failure is a row that is already there (a retry of the same attempt).
                        Some((inserted, duplicates))
                            if body_insert_is_complete(inserted, duplicates, expected) => {}
                        _ => return Err(classify(err)),
                    },
                }
            }
        }
        // The refs go first, as the head that names them would: there is no multi-document
        // transaction, so a commit that loses the CAS deletes them. They are ignored by every
        // reader and deleted with the run's purge.
        if !blobs.is_empty() {
            self.insert_run_blobs(id, &blobs).await?;
        }
        let mut set = doc! {
            "status": status.as_str(),
            "state": json_to_bson(&state)?,
            "wake_at": opt_date(wake_at),
            "sched_at": opt_date(sched_at(status, wake_at, t)),
            "open_key": open_key(id, &current.agent, current.conversation_id.as_deref(), status),
            "updated_at": date(t),
        };
        // Hoisted so the bound is visible at `expected + 1` below: a version that does not fit
        // the filter cannot reach the arithmetic.
        let expected_i = to_i64(expected, "version")?;
        if let Some(head) = head {
            let head_version = expected_i.checked_add(1).ok_or_else(|| {
                StoreError::InvalidInput(format!("version {expected} of run {id} cannot advance"))
            })?;
            set.insert("head", json_to_bson(&storable(&head))?);
            set.insert("head_version", head_version);
        }
        let result = self
            .runs
            .find_one_and_update(
                doc! {
                    "_id": uuid(id),
                    "version": expected_i,
                    // A tombstoned run is being purged; it must not come back to life.
                    "purging": { "$ne": true },
                },
                doc! { "$set": set, "$inc": { "version": 1_i64 } },
            )
            .projection(doc! { "_id": 1 })
            .return_document(ReturnDocument::After)
            .await;
        match result {
            Ok(Some(_)) => {
                // The superseded generations are collected only after the CAS: a commit that lost
                // names nothing, so it has nothing to supersede.
                if let Some((generation, _, _, _, written_at)) = body_plan {
                    let _ = self
                        .gc_body(id, generation, written_at - BODY_GC_GRACE)
                        .await;
                }
                Ok(RunRecord {
                    id,
                    agent: current.agent,
                    conversation_id: current.conversation_id,
                    parent_id: current.parent_id,
                    status,
                    // The contract of `commit_run`: the caller has the state it wrote.
                    state: serde_json::Value::Null,
                    wake_at,
                    version: expected + 1,
                    created_at: current.created_at,
                    updated_at: t,
                })
            }
            Ok(None) => match self.live_version(id).await? {
                None => {
                    // The run was purged between the rows/refs and the CAS: no head can ever name
                    // them, and nothing else would collect them.
                    self.run_blobs
                        .delete_many(doc! { "run_id": uuid(id) })
                        .await
                        .map_err(classify)?;
                    self.body
                        .delete_many(doc! { "run_id": uuid(id) })
                        .await
                        .map_err(classify)?;
                    Err(StoreError::NotFound(id))
                }
                Some(actual) => {
                    // The loser removes its own rows and refs, but never a row or a digest the
                    // winner's current head names.
                    if let Some((generation, _, writer, _, _)) = &body_plan {
                        self.delete_loser(id, *generation, *writer, actual, &blobs)
                            .await?;
                    }
                    Err(StoreError::Conflict {
                        run: id,
                        expected,
                        actual,
                    })
                }
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
        let JournalEntry {
            seq,
            name,
            ok,
            payload,
            recorded_at,
        } = entry;
        let entry = JournalEntry {
            seq,
            name,
            ok,
            payload: storable_owned(payload),
            recorded_at: truncate_ms(recorded_at),
        };
        let id = journal_id(run, entry.seq);
        let doc = doc! {
            "_id": &id,
            "run_id": uuid(run),
            "seq": to_i64(entry.seq, "journal seq")?,
            "name": &entry.name,
            "ok": entry.ok,
            "payload": json_to_bson(&entry.payload)?,
            "recorded_at": date(entry.recorded_at),
        };
        // Insert first: the write is one round trip, and a duplicate key means an earlier writer
        // won and their entry is the one to return. (A pre-read would add a round trip to every
        // step to save one on the replay that only a crash causes.)
        match self.journal.insert_one(&doc).await {
            Ok(_) => {
                // The insert and the run check are not atomic: if a purge removed the run in
                // between, don't leave an orphan behind for a future run with the same
                // (deterministic) id to replay.
                if !self.run_exists(run).await? {
                    self.journal
                        .delete_one(doc! { "_id": &id })
                        .await
                        .map_err(classify)?;
                    return Err(StoreError::NotFound(run));
                }
                // The entry this call wrote, not a conversion of the document back.
                Ok(entry)
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
            .projection(doc! { "lease_until": 1 })
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
            "config": json_to_bson(storable(&new.config).as_ref())?,
            "cursor": json_to_bson(storable(&new.cursor).as_ref())?,
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
            "cursor": json_to_bson(storable(&progress.cursor).as_ref())?,
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
                // Nothing left to purge; the sweep below still runs, as it does in the other
                // stores: a call that purges no run still collects unreferenced blobs.
                break;
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
            // 2. Journal, then 3. the run's blob references, then 4. runs. A crash between steps
            //    leaves tombstoned runs, which the next sweep picks up again.
            self.journal
                .delete_many(doc! { "run_id": { "$in": &doomed } })
                .await
                .map_err(classify)?;
            self.push
                .delete_many(doc! { "run_id": { "$in": &doomed } })
                .await
                .map_err(classify)?;
            self.run_blobs
                .delete_many(doc! { "run_id": { "$in": &doomed } })
                .await
                .map_err(classify)?;
            // The transcript goes with the run, as its journal and references do.
            self.body
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
                break;
            }
        }
        // The refs went with their runs; a digest no surviving run references and that was written
        // before the cutoff goes in the same sweep.
        self.sweep_blobs(before).await?;
        Ok(total)
    }

    fn keeps_bodies(&self) -> bool {
        true
    }

    async fn gc_body(
        &self,
        run: RunId,
        keep_generation: Uuid,
        before: DateTime<Utc>,
    ) -> StoreResult<u64> {
        MongoStore::gc_body(self, run, keep_generation, before).await
    }

    async fn body_list(&self, run: RunId, generation: Uuid) -> StoreResult<Vec<BodyRow>> {
        let docs: Vec<Document> = self
            .body
            .find(doc! { "run_id": uuid(run), "generation": generation_uuid(generation) })
            .sort(doc! { "idx": 1 })
            .await
            .map_err(classify)?
            .try_collect()
            .await
            .map_err(classify)?;
        docs.iter()
            .map(|doc| {
                let idx = u32::try_from(get_u64(doc, "idx")?)
                    .map_err(|_| StoreError::Corrupt("negative body index".into()))?;
                let message = bson_to_json(doc.get("message").ok_or_else(|| bad("message"))?)?;
                Ok(BodyRow { idx, message })
            })
            .collect()
    }

    fn keeps_blobs(&self) -> bool {
        true
    }

    async fn put_blob(&self, id: &BlobId, bytes: &[u8]) -> StoreResult<()> {
        let doc = doc! {
            "_id": id.as_str(),
            "bytes": Bson::Binary(bson::Binary {
                subtype: bson::spec::BinarySubtype::Generic,
                bytes: bytes.to_vec(),
            }),
            "size": to_i64(bytes.len() as u64, "blob size")?,
            "created_at": date(now()),
        };
        // Idempotent: the bytes behind a digest never change, so a duplicate key is the row that is
        // already there.
        match self.blobs.insert_one(&doc).await {
            Ok(_) => Ok(()),
            Err(err) if is_duplicate_key(&err) => Ok(()),
            Err(err) => Err(classify(err)),
        }
    }

    async fn get_blob(&self, id: &BlobId) -> StoreResult<Option<Vec<u8>>> {
        let found = self
            .blobs
            .find_one(doc! { "_id": id.as_str() })
            .await
            .map_err(classify)?;
        match found.as_ref().and_then(|doc| doc.get("bytes")) {
            None => Ok(None),
            Some(Bson::Binary(binary)) => Ok(Some(binary.bytes.clone())),
            Some(_) => Err(StoreError::Corrupt(format!(
                "blob {id} has no binary `bytes`"
            ))),
        }
    }

    async fn purge_blobs(&self, before: DateTime<Utc>) -> StoreResult<u64> {
        self.sweep_blobs(before).await
    }
}

impl MongoStore {
    /// Delete the rows a losing commit inserted (its `writer`), and the references it added,
    /// except what the winner's current head names. A loser must not poison the next append, and
    /// it must not remove a row or a digest the winner's head serves.
    async fn delete_loser(
        &self,
        id: RunId,
        generation: Uuid,
        writer: Uuid,
        actual: u64,
        refs: &[BlobId],
    ) -> StoreResult<()> {
        let found = self
            .runs
            .find_one(doc! { "_id": uuid(id) })
            .projection(doc! { "head": 1, "head_version": 1 })
            .await
            .map_err(classify)?;
        let (keep_len, keep_digests) = match found.as_ref() {
            Some(doc) if get_u64(doc, "head_version").ok() == Some(actual) => match doc.get("head")
            {
                None | Some(Bson::Null) => (0, HashSet::new()),
                Some(head) => {
                    let head = bson_to_json(head)?;
                    let keep_len = head_body(&head)
                        .filter(|(named, _)| *named == generation)
                        .map(|(_, len)| len)
                        .unwrap_or(0);
                    (keep_len, head_digests(&head))
                }
            },
            _ => (0, HashSet::new()),
        };
        self.body
            .delete_many(doc! {
                "run_id": uuid(id),
                "generation": generation_uuid(generation),
                "writer": generation_uuid(writer),
                "idx": { "$gte": i64::from(keep_len) },
            })
            .await
            .map_err(classify)?;
        let doomed: Vec<&str> = refs
            .iter()
            .filter(|digest| !keep_digests.contains(*digest))
            .map(BlobId::as_str)
            .collect();
        if !doomed.is_empty() {
            self.run_blobs
                .delete_many(doc! { "run_id": uuid(id), "digest": { "$in": doomed } })
                .await
                .map_err(classify)?;
        }
        Ok(())
    }

    /// Delete the body generations of `run` other than `keep_generation` that were written before
    /// `before`, and return how many rows went. The generation a commit names is never collected,
    /// however old it is; a superseded one is collected by the next commit of the run, with a
    /// one-hour grace.
    ///
    /// Exposed so a test (or an operator) can collect with an explicit cutoff: the grace is a
    /// constant of the commit path, not a value a caller has to guess.
    pub async fn gc_body(
        &self,
        run: RunId,
        keep_generation: Uuid,
        before: DateTime<Utc>,
    ) -> StoreResult<u64> {
        let deleted = self
            .body
            .delete_many(doc! {
                "run_id": uuid(run),
                "generation": { "$ne": generation_uuid(keep_generation) },
                "written_at": { "$lt": date(before) },
            })
            .await
            .map_err(classify)?;
        Ok(deleted.deleted_count)
    }

    /// Delete unreferenced blobs written before `before`, in batches; returns how many.
    ///
    /// Candidates are paged by `_id` (the digest) so a batch that is entirely referenced still
    /// moves the cursor: a digest a surviving run names is kept, however old it is, and a shared
    /// digest is deleted only with the last run that references it.
    async fn sweep_blobs(&self, before: DateTime<Utc>) -> StoreResult<u64> {
        let mut total = 0;
        let mut last: Option<String> = None;
        loop {
            let mut filter = doc! { "created_at": { "$lt": date(before) } };
            if let Some(last) = &last {
                filter.insert("_id", doc! { "$gt": last });
            }
            let candidates: Vec<String> = self
                .blobs
                .find(filter)
                .projection(doc! { "_id": 1 })
                .sort(doc! { "_id": 1 })
                .limit(PURGE_BATCH)
                .await
                .map_err(classify)?
                .try_collect::<Vec<Document>>()
                .await
                .map_err(classify)?
                .into_iter()
                .filter_map(|doc| doc.get_str("_id").ok().map(str::to_owned))
                .collect();
            let Some(page_end) = candidates.last().cloned() else {
                return Ok(total);
            };
            last = Some(page_end);
            let referenced: HashSet<String> = self
                .run_blobs
                .find(doc! { "digest": { "$in": candidates.clone() } })
                .projection(doc! { "digest": 1 })
                .await
                .map_err(classify)?
                .try_collect::<Vec<Document>>()
                .await
                .map_err(classify)?
                .into_iter()
                .filter_map(|doc| doc.get_str("digest").ok().map(str::to_owned))
                .collect();
            let doomed: Vec<String> = candidates
                .iter()
                .filter(|digest| !referenced.contains(*digest))
                .cloned()
                .collect();
            if !doomed.is_empty() {
                let deleted = self
                    .blobs
                    .delete_many(doc! { "_id": { "$in": doomed } })
                    .await
                    .map_err(classify)?;
                total += deleted.deleted_count;
            }
            if (candidates.len() as i64) < PURGE_BATCH {
                return Ok(total);
            }
        }
    }
}

/// Create the indexes that don't exist yet. Re-issuing `createIndexes` for
/// existing indexes is a no-op on MongoDB, but skipping them saves a round of
/// catalog work on every boot and sidesteps servers that handle it poorly.
///
/// **The match is by name only.** Changing an index's keys without changing its name leaves an
/// existing database on the old keys for ever, silently: `adam_list_status`'s order is a contract
/// now, and a future change to it needs a new name (the PostgreSQL adapter needed an explicit
/// `DROP INDEX` for exactly this reason when version 4 replaced `runs_list`).
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
mod conversion_counter {
    use std::cell::Cell;

    // Thread-local: a write test that asserts no conversion must not see the codec's unit
    // tests, which run in parallel on their own threads.
    thread_local! {
        static CALLS: Cell<usize> = const { Cell::new(0) };
    }

    pub fn bump() {
        CALLS.with(|calls| calls.set(calls.get() + 1));
    }

    pub fn calls() -> usize {
        CALLS.with(Cell::get)
    }
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

#[cfg(test)]
mod round_trip_tests {
    use std::sync::{Arc, Mutex};

    use mongodb::event::EventHandler;
    use mongodb::event::command::CommandEvent;
    use mongodb::options::ClientOptions;
    use serde_json::json;

    use super::*;

    /// A store whose client reports every command it starts, so a test can count the round trips
    /// of one operation. The callback runs on the operation's own task, so nothing races.
    async fn counted() -> Option<(MongoStore, Arc<Mutex<Vec<String>>>)> {
        let uri = adam_core::testing::test_env("ADAM_TEST_MONGODB_URI")?;
        let db = std::env::var("ADAM_TEST_MONGODB_DB").unwrap_or_else(|_| "adam_test".into());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let mut options = ClientOptions::parse(&uri).await.expect("options");
        options.command_event_handler = Some(EventHandler::callback(move |event: CommandEvent| {
            if let CommandEvent::Started(event) = event {
                sink.lock().unwrap().push(event.command_name);
            }
        }));
        let client = Client::with_options(options).expect("client");
        Some((MongoStore::new(client.database(&db)), seen))
    }

    /// The commands a store operation sent since the last call, ignoring the driver's own
    /// handshakes.
    fn sent(seen: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        let mut names = seen.lock().unwrap();
        let taken = names.clone();
        names.clear();
        taken
            .into_iter()
            .filter(|name| {
                !matches!(
                    name.as_str(),
                    "hello" | "isMaster" | "ismaster" | "ping" | "endSessions"
                )
            })
            .collect()
    }

    /// A journaled step is `journal_get` (one read) plus `journal_put`. `journal_put` used to cost
    /// four round trips (read, run check, insert, run check); an upsert that returns the winner
    /// costs one write and the run check.
    #[tokio::test(flavor = "current_thread")]
    async fn a_journal_entry_is_one_write_and_one_check() {
        let Some((store, seen)) = counted().await else {
            return;
        };
        store.migrate().await.expect("migrate");
        let run = store
            .create_run(NewRun::new("round-trip", json!({})))
            .await
            .expect("create");
        sent(&seen);

        let entry = store
            .journal_put(run.id, JournalEntry::ok(0, "tool", json!({"x": 1})))
            .await
            .expect("put");
        assert_eq!(entry.payload, json!({"x": 1}));
        let commands = sent(&seen);
        assert!(
            commands.len() <= 2,
            "a journal entry costs at most two round trips: {commands:?}"
        );
        assert!(
            commands
                .iter()
                .any(|name| name == "findAndModify" || name == "update" || name == "insert"),
            "one of them writes: {commands:?}"
        );

        // A replay writes nothing new: the winner's document comes back in the same round trip.
        let again = store
            .journal_put(run.id, JournalEntry::ok(0, "tool", json!({"x": 2})))
            .await
            .expect("put");
        assert_eq!(again.payload, json!({"x": 1}), "the first writer wins");
        let commands = sent(&seen);
        assert!(
            commands.len() <= 2,
            "a replay costs at most two round trips: {commands:?}"
        );
        assert_eq!(
            store.journal_get(run.id, 0).await.unwrap().unwrap(),
            entry,
            "the stored entry is the first one"
        );
    }
}

#[cfg(test)]
mod returned_value_tests {
    use super::*;
    use serde_json::json;

    async fn store() -> Option<MongoStore> {
        let uri = adam_core::testing::test_env("ADAM_TEST_MONGODB_URI")?;
        let db = std::env::var("ADAM_TEST_MONGODB_DB").unwrap_or_else(|_| "adam_test".into());
        let store = MongoStore::connect(&uri, &db).await.expect("connect");
        store.migrate().await.expect("migrate");
        Some(store)
    }

    fn agent() -> String {
        format!("returned-{}", Uuid::now_v7().simple())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_write_returns_the_cleaned_value_it_stored() {
        let Some(store) = store().await else { return };
        let agent = agent();

        let created = store
            .create_run(NewRun::new(
                &agent,
                json!({"nested": {"k\u{0}ey": ["a\u{0}b", {"deep": [1, 2, {"text": "x\u{0}y"}]}]}}),
            ))
            .await
            .unwrap();
        let want = json!({
            "nested": {"k\u{FFFD}ey": ["a\u{FFFD}b", {"deep": [1, 2, {"text": "x\u{FFFD}y"}]}]},
        });
        assert!(created.state.is_null(), "a write does not echo the state");
        assert_eq!(
            store.load_run(created.id).await.unwrap().unwrap().state,
            want,
            "the stored state is the cleaned one"
        );

        let committed = store
            .commit_run(
                created.id,
                created.version,
                RunUpdate::new(
                    RunStatus::Parked,
                    json!({"turn": 2, "log": ["ok", "bad\u{0}byte"]}),
                ),
            )
            .await
            .unwrap();
        assert!(committed.state.is_null());
        let stored = store.load_run(created.id).await.unwrap().unwrap();
        assert_eq!(
            stored.state,
            json!({"turn": 2, "log": ["ok", "bad\u{FFFD}byte"]})
        );
        assert_eq!(
            stored,
            RunRecord {
                state: stored.state.clone(),
                ..committed
            },
            "every other field of the returned record is the stored one"
        );

        let entry = store
            .journal_put(
                created.id,
                JournalEntry::ok(
                    0,
                    "tool",
                    json!({"out": "a\u{0}b", "nested": {"k\u{0}ey": [1, {"x": "y"}]}}),
                ),
            )
            .await
            .unwrap();
        assert_eq!(
            entry.payload,
            json!({"out": "a\u{FFFD}b", "nested": {"k\u{FFFD}ey": [1, {"x": "y"}]}}),
            "a journal entry returns the payload it stored"
        );
        assert_eq!(
            entry,
            store.journal_get(created.id, 0).await.unwrap().unwrap(),
            "the returned entry is the stored one"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_write_returns_a_large_value_without_converting_it_back() {
        let Some(store) = store().await else { return };
        let agent = agent();

        let big = json!({"blob": "x".repeat(1024 * 1024), "nested": {"turn": 1}});
        let before = conversion_counter::calls();
        let created = store
            .create_run(NewRun::new(&agent, big.clone()))
            .await
            .unwrap();
        assert_eq!(
            conversion_counter::calls() - before,
            0,
            "create_run converts nothing back"
        );
        assert!(created.state.is_null(), "a write does not echo the state");
        assert_eq!(
            store.load_run(created.id).await.unwrap().unwrap().state,
            big
        );

        // A read does convert, so a counter that never moved would fail here.
        let before = conversion_counter::calls();
        let loaded = store.load_run(created.id).await.unwrap().unwrap();
        assert_eq!(
            conversion_counter::calls() - before,
            1,
            "load_run converts the stored state once"
        );
        assert_eq!(loaded.state, big);

        let next = json!({"blob": "y".repeat(1024 * 1024), "nested": {"turn": 2}});
        let before = conversion_counter::calls();
        let committed = store
            .commit_run(
                created.id,
                created.version,
                RunUpdate::new(RunStatus::Runnable, next.clone()),
            )
            .await
            .unwrap();
        // The projected pre-read leaves the old state out and the record it returns adds none.
        assert_eq!(
            conversion_counter::calls() - before,
            0,
            "commit_run converts neither the old state nor the new one"
        );
        assert!(committed.state.is_null());
        let stored = store.load_run(created.id).await.unwrap().unwrap();
        assert_eq!(stored.state, next);
        assert_eq!(
            stored,
            RunRecord {
                state: stored.state.clone(),
                ..committed
            }
        );

        let payload = json!({"content": "z".repeat(1024 * 1024)});
        let before = conversion_counter::calls();
        let entry = store
            .journal_put(created.id, JournalEntry::ok(0, "tool", payload.clone()))
            .await
            .unwrap();
        assert_eq!(
            conversion_counter::calls() - before,
            0,
            "journal_put converts nothing back"
        );
        assert_eq!(entry.payload, payload);

        let before = conversion_counter::calls();
        let got = store.journal_get(created.id, 0).await.unwrap().unwrap();
        assert_eq!(
            conversion_counter::calls() - before,
            1,
            "journal_get converts the stored payload once"
        );
        assert_eq!(got, entry);
    }
}

#[cfg(test)]
mod body_insert_tests {
    use super::body_insert_is_complete;

    /// The completeness check counts a duplicate key as a row that is there, and nothing else: a
    /// partial insert must fail the check so the commit aborts before the CAS.
    #[test]
    fn a_body_insert_is_complete_only_when_every_row_landed() {
        assert!(body_insert_is_complete(3, 0, 3), "all inserted");
        assert!(
            body_insert_is_complete(2, 1, 3),
            "two inserted and one already there"
        );
        assert!(body_insert_is_complete(0, 3, 3), "a retry of the whole set");
        assert!(
            !body_insert_is_complete(2, 0, 3),
            "one row missing, nothing accounts for it"
        );
        assert!(
            !body_insert_is_complete(1, 1, 3),
            "one row landed, one duplicate, one lost"
        );
        assert!(!body_insert_is_complete(3, 1, 3), "more rows than asked");
    }
}

#[cfg(test)]
mod body_loser_tests {
    use adam_core::store::now;
    use adam_core::{BlobId, BodyRow, BodyWrite, NewRun, RunStatus, RunUpdate};
    use serde_json::json;

    use super::*;

    async fn store() -> Option<(MongoStore, String)> {
        let uri = adam_core::testing::test_env("ADAM_TEST_MONGODB_URI")?;
        let db = std::env::var("ADAM_TEST_MONGODB_DB").unwrap_or_else(|_| "adam_test".into());
        let prefix = format!("s2a_loser_{}_", &Uuid::now_v7().simple().to_string()[..8]);
        let store = MongoStore::connect(&uri, &db)
            .await
            .expect("connect")
            .with_collection_prefix(&prefix)
            .expect("prefix");
        store.migrate().await.expect("migrate");
        Some((store, prefix))
    }

    async fn clean(store: &MongoStore, prefix: &str) {
        for name in [
            "runs", "journal", "push", "blob", "run_blob", "body", "meta",
        ] {
            let _ = store
                .database()
                .collection::<Document>(&format!("{prefix}{name}"))
                .drop()
                .await;
        }
    }

    /// A head naming generation `generation` with `len` rows and one file artifact, `digest`.
    fn head(generation: Uuid, len: u32, digest: &BlobId) -> serde_json::Value {
        json!({
            "v": 5,
            "agent": {},
            "body": {"generation": generation.to_string(), "len": len},
            "artifacts": [{
                "name": "chart",
                "data": null,
                "file": {"filename": "c.svg", "digest": digest.as_str(), "size": 1},
            }],
        })
    }

    /// A loser's rows are deleted; a row the winner's head names is kept, and a reference the
    /// winner's head names survives.
    #[tokio::test(flavor = "current_thread")]
    async fn the_loser_rows_and_refs_are_deleted_but_what_the_head_names_is_kept() {
        let Some((store, prefix)) = store().await else {
            return;
        };
        let agent = format!("loser-{}", Uuid::now_v7().simple());
        let run = store
            .create_run(NewRun::new(&agent, json!({})))
            .await
            .expect("create");
        let generation = Uuid::new_v4();
        let writer = Uuid::new_v4();
        let digest = BlobId::from_hex(&"a".repeat(64)).expect("a digest");
        let other = BlobId::from_hex(&"b".repeat(64)).expect("a digest");
        store
            .commit_run(
                run.id,
                run.version,
                RunUpdate::new(RunStatus::Runnable, json!({"turn": 1}))
                    .head(head(generation, 2, &digest))
                    .body(BodyWrite {
                        generation: Some(generation),
                        rows: vec![
                            BodyRow {
                                idx: 0,
                                message: json!("winner"),
                            },
                            BodyRow {
                                idx: 1,
                                message: json!("kept"),
                            },
                        ],
                        writer,
                    }),
            )
            .await
            .expect("commit");
        let actual = store
            .load_run(run.id)
            .await
            .expect("load")
            .expect("the run")
            .version;
        // The loser's rows (an append it inserted before losing) and refs.
        let loser = Uuid::new_v4();
        let rows = [2_u32, 3]
            .into_iter()
            .map(|idx| {
                body_doc(
                    run.id,
                    generation,
                    loser,
                    &BodyRow {
                        idx,
                        message: json!(format!("loser {idx}")),
                    },
                    now(),
                )
                .expect("a row")
            })
            .collect::<Vec<_>>();
        store
            .body
            .insert_many(&rows)
            .await
            .expect("insert the loser rows");
        store
            .insert_run_blobs(run.id, &[digest.clone(), other.clone()])
            .await
            .expect("refs");

        store
            .delete_loser(
                run.id,
                generation,
                loser,
                actual,
                &[digest.clone(), other.clone()],
            )
            .await
            .expect("delete the loser");
        let rows = store.body_list(run.id, generation).await.expect("rows");
        assert_eq!(rows.len(), 2, "the loser's rows went, the winner's stayed");
        let refs: Vec<Document> = store
            .run_blobs
            .find(doc! { "run_id": uuid(run.id) })
            .await
            .expect("refs")
            .try_collect()
            .await
            .expect("refs");
        let digests: Vec<&str> = refs
            .iter()
            .filter_map(|doc| doc.get_str("digest").ok())
            .collect();
        assert!(
            digests.contains(&digest.as_str()),
            "the head's digest is kept"
        );
        assert!(
            !digests.contains(&other.as_str()),
            "the loser's other ref went"
        );

        // A row the winner's head names is never deleted, even with the loser's writer.
        let rows = [2_u32, 3]
            .into_iter()
            .map(|idx| {
                body_doc(
                    run.id,
                    generation,
                    loser,
                    &BodyRow {
                        idx,
                        message: json!(format!("named {idx}")),
                    },
                    now(),
                )
                .expect("a row")
            })
            .collect::<Vec<_>>();
        store
            .body
            .insert_many(&rows)
            .await
            .expect("insert the named rows");
        store
            .runs
            .update_one(
                doc! { "_id": uuid(run.id) },
                doc! { "$set": { "head.body.len": 4_i64 } },
            )
            .await
            .expect("the winner names four rows");
        store
            .delete_loser(run.id, generation, loser, actual, &[])
            .await
            .expect("delete the loser again");
        assert_eq!(
            store
                .body_list(run.id, generation)
                .await
                .expect("rows")
                .len(),
            4,
            "a row the winner's head names is kept"
        );
        clean(&store, &prefix).await;
    }
}

#[cfg(test)]
mod body_insert_error_tests {
    use mongodb::bson::{doc, from_document};
    use mongodb::error::{ErrorKind, InsertManyError};

    use super::body_insert_counts;

    fn insert_many(write_errors: bool, write_concern: bool) -> mongodb::error::Error {
        let mut document = doc! {};
        if write_errors {
            document.insert(
                "writeErrors",
                vec![doc! {
                    "index": 0_i32,
                    "code": 11000_i32,
                    "errmsg": "duplicate key",
                }],
            );
        }
        if write_concern {
            document.insert(
                "writeConcernError",
                doc! {
                    "code": 100_i32,
                    "codeName": "UnsatisfiableWriteConcern",
                    "errmsg": "not enough data-bearing nodes",
                },
            );
        }
        let error: InsertManyError = from_document(document).expect("an insert-many answer");
        mongodb::error::Error::from(ErrorKind::InsertMany(error))
    }

    /// A write-concern error is not a completeness answer, even when every write error is a
    /// duplicate: the write may or may not have landed, so the commit must abort.
    #[test]
    fn body_insert_counts_aborts_on_a_write_concern_error() {
        assert_eq!(
            body_insert_counts(&insert_many(true, true), 2),
            None,
            "a duplicate plus a write-concern failure is not complete"
        );
        assert_eq!(
            body_insert_counts(&insert_many(false, true), 2),
            None,
            "a write-concern failure alone is not complete"
        );
        assert_eq!(
            body_insert_counts(&insert_many(true, false), 2),
            Some((1, 1)),
            "a duplicate alone is a row that is already there"
        );
    }
}
