//! In-memory [`Store`] for tests and `adam dev` without a database.
//! Not durable: everything is lost when the process exits.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use crate::time::Timestamp;
use async_trait::async_trait;
use jiff::SignedDuration;
use tokio::sync::Mutex;
use uuid::Uuid;

use super::{
    BlobId, BodyRow, BodyWrite, ClaimScope, JournalEntry, Lease, NewPushConfig, NewRun,
    PushProgress, PushRecord, PushState, RunId, RunQuery, RunRecord, RunUpdate, Store, StoreError,
    StoreResult, add_ttl, head_body, now, set_head_body, storable_owned, truncate_ms,
};

#[derive(Default)]
pub struct MemoryStore {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    runs: HashMap<RunId, Slot>,
    journal: BTreeMap<(RunId, u64), JournalEntry>,
    push: BTreeMap<(RunId, String), PushSlot>,
    /// The transcript rows, by `(run, generation, idx)`: a commit appends to the current
    /// generation, a compaction starts a new one, and the old one is collected after a grace.
    bodies: BTreeMap<(RunId, Uuid, u32), BodySlot>,
    /// How many rows the store inserted, for the test that pins an append's delta.
    #[cfg(test)]
    body_rows_inserted: u64,
    /// File bytes by content address. A blob is global: several runs may reference one digest.
    blobs: HashMap<BlobId, BlobSlot>,
    /// The digests each run references, append-only (`RunUpdate::blobs`).
    run_blobs: HashMap<RunId, HashSet<BlobId>>,
}

struct BodySlot {
    message: serde_json::Value,
    written_at: Timestamp,
}

struct BlobSlot {
    bytes: Vec<u8>,
    created_at: Timestamp,
}

struct PushSlot {
    record: PushRecord,
    lease: Option<(String, Timestamp)>,
}

struct Slot {
    run: RunRecord,
    lease: Option<(String, Timestamp)>,
    /// Set by the first [`ClaimScope::Pinned`] claim; never cleared.
    owner: Option<String>,
    /// The v5 head last written by a commit, and the version it was written at. A commit that
    /// writes no head leaves both alone, so a stale head is never served.
    head: Option<serde_json::Value>,
    head_version: Option<u64>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Inner {
    fn open_run(
        &self,
        agent: &str,
        conversation_id: &str,
        except: Option<RunId>,
    ) -> Option<&RunRecord> {
        self.runs.values().map(|s| &s.run).find(|r| {
            Some(r.id) != except
                && r.status.is_open()
                && r.agent == agent
                && r.conversation_id.as_deref() == Some(conversation_id)
        })
    }
}

#[async_trait]
impl Store for MemoryStore {
    async fn migrate(&self) -> StoreResult<()> {
        Ok(())
    }

    async fn create_run(&self, new: NewRun) -> StoreResult<RunRecord> {
        let mut inner = self.inner.lock().await;
        if inner.runs.contains_key(&new.id) {
            return Err(StoreError::AlreadyExists(new.id));
        }
        if let (true, Some(conv)) = (new.status.is_open(), new.conversation_id.as_deref())
            && inner.open_run(&new.agent, conv, None).is_some()
        {
            return Err(StoreError::ConversationBusy {
                agent: new.agent,
                conversation_id: conv.to_owned(),
            });
        }
        let t = now();
        let run = RunRecord {
            id: new.id,
            agent: new.agent,
            conversation_id: new.conversation_id,
            parent_id: new.parent_id,
            status: new.status,
            state: storable_owned(new.state),
            wake_at: new.wake_at.map(truncate_ms),
            version: 1,
            created_at: t,
            updated_at: t,
        };
        inner.runs.insert(
            run.id,
            Slot {
                run: run.clone(),
                lease: None,
                owner: None,
                head: None,
                head_version: None,
            },
        );
        // The contract of `create_run`: the caller has the state it wrote.
        Ok(RunRecord {
            state: serde_json::Value::Null,
            ..run
        })
    }

    async fn load_run(&self, id: RunId) -> StoreResult<Option<RunRecord>> {
        Ok(self.inner.lock().await.runs.get(&id).map(|s| s.run.clone()))
    }

    async fn commit_run(
        &self,
        id: RunId,
        expected: u64,
        update: RunUpdate,
    ) -> StoreResult<RunRecord> {
        let RunUpdate {
            status,
            state,
            head,
            body,
            blobs,
            wake_at,
        } = update;
        let mut inner = self.inner.lock().await;
        let current = inner.runs.get(&id).ok_or(StoreError::NotFound(id))?;
        if current.run.version != expected {
            return Err(StoreError::Conflict {
                run: id,
                expected,
                actual: current.run.version,
            });
        }
        if let (true, Some(conv)) = (status.is_open(), current.run.conversation_id.clone()) {
            let agent = current.run.agent.clone();
            if inner.open_run(&agent, &conv, Some(id)).is_some() {
                return Err(StoreError::ConversationBusy {
                    agent,
                    conversation_id: conv,
                });
            }
        }
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
                writer: _,
            } = body;
            // The rows are a transcript: index `i` is the message at `i`. A wrong index would write
            // the wrong offset while the head's `len` still matched.
            for (position, row) in rows.iter().enumerate() {
                let position = u32::try_from(position).map_err(|_| {
                    StoreError::InvalidInput("a transcript is longer than u32".into())
                })?;
                if row.idx != position {
                    return Err(StoreError::InvalidInput(format!(
                        "body row {position} has index {}",
                        row.idx
                    )));
                }
            }
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
            body_plan = Some((target, start, rows));
        }
        let committed = {
            #[allow(
                clippy::expect_used,
                reason = "presence was checked above under the same lock"
            )]
            let slot = inner.runs.get_mut(&id).expect("checked above");
            slot.run.status = status;
            slot.run.state = storable_owned(state);
            slot.run.wake_at = wake_at.map(truncate_ms);
            slot.run.version += 1;
            slot.run.updated_at = now();
            if let Some(head) = head {
                slot.head = Some(storable_owned(head));
                slot.head_version = Some(slot.run.version);
            }
            // The contract of `commit_run`: the caller has the state it wrote.
            RunRecord {
                state: serde_json::Value::Null,
                ..slot.run.clone()
            }
        };
        if let Some((generation, start, rows)) = body_plan {
            let written_at = now();
            #[cfg(test)]
            {
                inner.body_rows_inserted +=
                    u64::try_from(rows.len().saturating_sub(start)).unwrap_or(u64::MAX);
            }
            for row in rows.into_iter().skip(start) {
                // Rows are written once: a retry of the same attempt keeps the first bytes.
                inner
                    .bodies
                    .entry((id, generation, row.idx))
                    .or_insert_with(|| BodySlot {
                        message: storable_owned(row.message),
                        written_at,
                    });
            }
            // A superseded generation older than the grace period goes with the commit that
            // superseded it.
            collect_bodies(&mut inner, id, generation, written_at - BODY_GC_GRACE);
        }
        // The refs go with the commit that names them, append-only: a digest is added once and
        // never removed while the run lives.
        if !blobs.is_empty() {
            inner
                .run_blobs
                .entry(id)
                .or_default()
                .extend(blobs.iter().cloned());
        }
        Ok(committed)
    }

    async fn load_head(&self, id: RunId) -> StoreResult<Option<RunRecord>> {
        let inner = self.inner.lock().await;
        let Some(slot) = inner.runs.get(&id) else {
            return Ok(None);
        };
        let mut rec = slot.run.clone();
        // A head a later commit did not rebuild is stale: serve the whole state instead.
        if slot.head_version == Some(rec.version)
            && let Some(head) = &slot.head
        {
            rec.state = head.clone();
        }
        Ok(Some(rec))
    }

    async fn open_run_for_conversation(
        &self,
        agent: &str,
        conversation_id: &str,
    ) -> StoreResult<Option<RunRecord>> {
        Ok(self
            .inner
            .lock()
            .await
            .open_run(agent, conversation_id, None)
            .cloned())
    }

    async fn journal_get(&self, run: RunId, seq: u64) -> StoreResult<Option<JournalEntry>> {
        Ok(self.inner.lock().await.journal.get(&(run, seq)).cloned())
    }

    async fn journal_put(&self, run: RunId, mut entry: JournalEntry) -> StoreResult<JournalEntry> {
        let mut inner = self.inner.lock().await;
        if let Some(existing) = inner.journal.get(&(run, entry.seq)) {
            if existing.name != entry.name {
                return Err(StoreError::NonDeterminism {
                    run,
                    seq: entry.seq,
                    recorded: existing.name.clone(),
                    requested: entry.name,
                });
            }
            return Ok(existing.clone());
        }
        if !inner.runs.contains_key(&run) {
            return Err(StoreError::NotFound(run));
        }
        entry.payload = storable_owned(std::mem::take(&mut entry.payload));
        entry.recorded_at = truncate_ms(entry.recorded_at);
        inner.journal.insert((run, entry.seq), entry.clone());
        Ok(entry)
    }

    async fn journal_list(&self, run: RunId) -> StoreResult<Vec<JournalEntry>> {
        let inner = self.inner.lock().await;
        Ok(inner
            .journal
            .range((run, 0)..=(run, u64::MAX))
            .map(|(_, e)| e.clone())
            .collect())
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
        let now = truncate_ms(now);
        let until = add_ttl(now, ttl);
        let mut inner = self.inner.lock().await;
        let mut due: Vec<_> = inner
            .runs
            .values()
            .filter(|s| agents.contains(&s.run.agent))
            .filter(|s| !busy.contains(&s.run.id))
            .filter(|s| s.run.sched_at().is_some_and(|at| at <= now))
            .filter(|s| s.lease.as_ref().is_none_or(|(_, u)| *u <= now))
            .filter(|s| match scope {
                ClaimScope::Any => true,
                ClaimScope::Pinned => s.owner.as_deref().is_none_or(|owner| owner == worker),
            })
            .map(|s| (s.run.sched_at(), s.run.id))
            .collect();
        due.sort();
        due.truncate(limit);
        Ok(due
            .into_iter()
            .map(|(_, id)| {
                #[allow(
                    clippy::expect_used,
                    reason = "the id was listed a moment ago under the same lock"
                )]
                let slot = inner.runs.get_mut(&id).expect("just listed");
                slot.lease = Some((worker.to_owned(), until));
                match scope {
                    ClaimScope::Any => {}
                    ClaimScope::Pinned => {
                        slot.owner.get_or_insert_with(|| worker.to_owned());
                    }
                }
                Lease {
                    run: slot.run.clone(),
                    worker: worker.to_owned(),
                    until,
                }
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
        let mut inner = self.inner.lock().await;
        let Some(slot) = inner.runs.get_mut(&id) else {
            return Ok(false);
        };
        match &slot.lease {
            Some((owner, until)) if owner == worker && *until > now => {
                slot.lease = Some((worker.to_owned(), add_ttl(now, ttl)));
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn release_lease(&self, id: RunId, worker: &str) -> StoreResult<()> {
        let mut inner = self.inner.lock().await;
        if let Some(slot) = inner.runs.get_mut(&id)
            && slot
                .lease
                .as_ref()
                .is_some_and(|(owner, _)| owner == worker)
        {
            slot.lease = None;
        }
        Ok(())
    }

    async fn lease_until(&self, id: RunId) -> StoreResult<Option<Timestamp>> {
        let inner = self.inner.lock().await;
        Ok(inner
            .runs
            .get(&id)
            .and_then(|slot| slot.lease.as_ref())
            .map(|(_, until)| *until))
    }

    async fn list_runs(&self, query: &RunQuery) -> StoreResult<Vec<RunRecord>> {
        let inner = self.inner.lock().await;
        // Sort references and clone the page only: a listing of a thousand runs must not copy a
        // thousand states to return ten of them.
        let mut found: Vec<&RunRecord> = inner
            .runs
            .values()
            .map(|s| &s.run)
            .filter(|r| query.matches(r))
            .filter(|r| {
                query
                    .after
                    .is_none_or(|(at, id)| (r.updated_at, r.id) < (at, id))
            })
            .collect();
        found.sort_by_key(|r| std::cmp::Reverse((r.updated_at, r.id)));
        found.truncate(query.limit);
        Ok(found.into_iter().cloned().collect())
    }

    async fn count_runs(&self, query: &RunQuery) -> StoreResult<u64> {
        let inner = self.inner.lock().await;
        Ok(inner
            .runs
            .values()
            .filter(|s| query.matches(&s.run))
            .count() as u64)
    }

    async fn push_put(&self, new: NewPushConfig) -> StoreResult<PushRecord> {
        let mut inner = self.inner.lock().await;
        if !inner.runs.contains_key(&new.run) {
            return Err(StoreError::NotFound(new.run));
        }
        let t = now();
        let key = (new.run, new.id.clone());
        let (version, created_at) = inner
            .push
            .get(&key)
            .map_or((1, t), |s| (s.record.version + 1, s.record.created_at));
        let record = PushRecord {
            run: new.run,
            id: new.id,
            agent: new.agent,
            owner: new.owner,
            config: storable_owned(new.config),
            cursor: storable_owned(new.cursor),
            state: PushState::Active,
            attempts: 0,
            last_error: None,
            next_attempt_at: t,
            version,
            created_at,
            updated_at: t,
        };
        inner.push.insert(
            key,
            PushSlot {
                record: record.clone(),
                lease: None,
            },
        );
        Ok(record)
    }

    async fn push_list(&self, run: RunId) -> StoreResult<Vec<PushRecord>> {
        let inner = self.inner.lock().await;
        Ok(inner
            .push
            .range((run, String::new())..)
            .take_while(|((r, _), _)| *r == run)
            .map(|(_, s)| s.record.clone())
            .collect())
    }

    async fn push_delete(&self, run: RunId, id: &str) -> StoreResult<bool> {
        let mut inner = self.inner.lock().await;
        Ok(inner.push.remove(&(run, id.to_owned())).is_some())
    }

    async fn push_claim_due(
        &self,
        agents: &[String],
        worker: &str,
        now: Timestamp,
        ttl: Duration,
        limit: usize,
    ) -> StoreResult<Vec<PushRecord>> {
        let now = truncate_ms(now);
        let until = add_ttl(now, ttl);
        let mut inner = self.inner.lock().await;
        let mut due: Vec<_> = inner
            .push
            .iter()
            .filter(|(_, s)| {
                s.record.state == PushState::Active
                    && agents.contains(&s.record.agent)
                    && s.record.next_attempt_at <= now
                    && s.lease.as_ref().is_none_or(|(_, u)| *u <= now)
            })
            .map(|(k, s)| (s.record.next_attempt_at, k.clone()))
            .collect();
        due.sort();
        due.truncate(limit);
        Ok(due
            .into_iter()
            .filter_map(|(_, key)| {
                let slot = inner.push.get_mut(&key)?;
                slot.lease = Some((worker.to_owned(), until));
                Some(slot.record.clone())
            })
            .collect())
    }

    async fn push_commit(
        &self,
        run: RunId,
        id: &str,
        expected: u64,
        progress: PushProgress,
    ) -> StoreResult<PushRecord> {
        let mut inner = self.inner.lock().await;
        let slot = inner
            .push
            .get_mut(&(run, id.to_owned()))
            .ok_or(StoreError::NotFound(run))?;
        if slot.record.version != expected {
            return Err(StoreError::Conflict {
                run,
                expected,
                actual: slot.record.version,
            });
        }
        let record = &mut slot.record;
        record.state = progress.state;
        record.cursor = storable_owned(progress.cursor);
        record.attempts = progress.attempts;
        record.last_error = progress.last_error;
        record.next_attempt_at = truncate_ms(progress.next_attempt_at);
        record.version += 1;
        record.updated_at = now();
        slot.lease = None;
        Ok(slot.record.clone())
    }

    async fn purge_finished(&self, agent: &str, before: Timestamp) -> StoreResult<u64> {
        let mut inner = self.inner.lock().await;
        let doomed: Vec<RunId> = inner
            .runs
            .values()
            .filter(|s| {
                s.run.agent == agent && s.run.status.is_terminal() && s.run.updated_at < before
            })
            .map(|s| s.run.id)
            .collect();
        let doomed_set: HashSet<RunId> = doomed.iter().copied().collect();
        for id in &doomed {
            inner.runs.remove(id);
            inner.push.retain(|(run, _), _| run != id);
            inner.run_blobs.remove(id);
            let keys: Vec<_> = inner
                .journal
                .range((*id, 0)..=(*id, u64::MAX))
                .map(|(k, _)| *k)
                .collect();
            for k in keys {
                inner.journal.remove(&k);
            }
        }
        // A run's transcript goes with it, as its journal and references do.
        inner
            .bodies
            .retain(|(run, _, _), _| !doomed_set.contains(run));
        // The refs went with their runs; a digest no surviving run references and that was written
        // before the cutoff is collected in the same sweep, as the adapters do.
        sweep_blobs(&mut inner, before);
        Ok(doomed.len() as u64)
    }

    fn keeps_bodies(&self) -> bool {
        true
    }

    async fn gc_body(
        &self,
        run: RunId,
        keep_generation: Uuid,
        before: Timestamp,
    ) -> StoreResult<u64> {
        let mut inner = self.inner.lock().await;
        Ok(collect_bodies(&mut inner, run, keep_generation, before))
    }

    async fn body_list(&self, run: RunId, generation: Uuid) -> StoreResult<Vec<BodyRow>> {
        let inner = self.inner.lock().await;
        Ok(inner
            .bodies
            .range((run, generation, 0)..=(run, generation, u32::MAX))
            .map(|((_, _, idx), slot)| BodyRow {
                idx: *idx,
                message: slot.message.clone(),
            })
            .collect())
    }

    fn keeps_blobs(&self) -> bool {
        true
    }

    async fn blob_stored(&self, id: &BlobId) -> StoreResult<bool> {
        Ok(self.inner.lock().await.blobs.contains_key(id))
    }

    async fn put_blob(&self, id: &BlobId, bytes: &[u8]) -> StoreResult<()> {
        let mut inner = self.inner.lock().await;
        // Idempotent, and the bytes behind an id never change: the first put wins.
        inner.blobs.entry(id.clone()).or_insert_with(|| BlobSlot {
            bytes: bytes.to_vec(),
            created_at: now(),
        });
        Ok(())
    }

    async fn get_blob(&self, id: &BlobId) -> StoreResult<Option<Vec<u8>>> {
        let inner = self.inner.lock().await;
        Ok(inner.blobs.get(id).map(|slot| slot.bytes.clone()))
    }

    async fn purge_blobs(&self, before: Timestamp) -> StoreResult<u64> {
        let mut inner = self.inner.lock().await;
        Ok(sweep_blobs(&mut inner, before))
    }
}

/// How long a superseded body generation stays readable before the next commit of its run collects
/// it.
const BODY_GC_GRACE: SignedDuration = SignedDuration::from_hours(1);

/// Delete the body rows of `run` outside `keep_generation` that were written before `before`, and
/// return how many rows went. The generation a commit names is never collected, however old it is;
/// a superseded one is collected once the grace period has passed.
fn collect_bodies(inner: &mut Inner, run: RunId, keep_generation: Uuid, before: Timestamp) -> u64 {
    let before_len = inner.bodies.len();
    inner.bodies.retain(|(r, generation, _), slot| {
        !(*r == run && *generation != keep_generation && slot.written_at < before)
    });
    (before_len - inner.bodies.len()) as u64
}

/// Delete the blobs written before `before` that no run references; returns how many.
fn sweep_blobs(inner: &mut Inner, before: Timestamp) -> u64 {
    let referenced: HashSet<BlobId> = inner.run_blobs.values().flatten().cloned().collect();
    let doomed: Vec<BlobId> = inner
        .blobs
        .iter()
        .filter(|(id, slot)| slot.created_at < before && !referenced.contains(*id))
        .map(|(id, _)| id.clone())
        .collect();
    for id in &doomed {
        inner.blobs.remove(id);
    }
    doomed.len() as u64
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::RunStatus;
    use crate::store::ConversationScope;

    /// A thousand runs, paged by the `(updated_at, id)` keyset: every page is the next slice of the
    /// order the store promises, and the last one is short. The listing sorts references and clones
    /// only the page (see `list_runs`), which this checks by its result: the same order and the same
    /// records a full sort would give.
    #[tokio::test]
    async fn a_listing_of_a_thousand_runs_pages_by_updated_at_and_id() {
        let store = MemoryStore::new();
        let mut made = Vec::with_capacity(1000);
        for n in 0..1000 {
            let mut run = NewRun::new("a", json!({"n": n})).conversation(format!("owner:ctx{n}"));
            if n % 2 == 0 {
                run = run.status(RunStatus::Parked);
            }
            let created = store.create_run(run).await.expect("create a run");
            // A create returns the record without its state; the listing comparisons want the
            // stored one.
            made.push(
                store
                    .load_run(created.id)
                    .await
                    .expect("load")
                    .expect("the created run"),
            );
        }
        // What the store's contract says the order is, computed from the records themselves.
        made.sort_by_key(|r| std::cmp::Reverse((r.updated_at, r.id)));
        let expected: Vec<RunId> = made.iter().map(|r| r.id).collect();

        let mut page = RunQuery::new("a", ConversationScope::Prefix("owner:".into()), 10);
        let mut seen = Vec::new();
        loop {
            let runs = store.list_runs(&page).await.expect("list");
            assert!(runs.len() <= 10);
            let last = runs.last().map(|r| (r.updated_at, r.id));
            seen.extend(runs.iter().map(|r| r.id));
            match last {
                Some(after) if runs.len() == 10 => page.after = Some(after),
                _ => break,
            }
        }
        assert_eq!(seen, expected, "every run once, in the documented order");

        // A page in the middle is the slice after its cursor, records and all, and the filters
        // still apply.
        let mut middle = RunQuery::new("a", ConversationScope::Prefix("owner:".into()), 3);
        middle.after = Some((made[499].updated_at, made[499].id));
        assert_eq!(
            store
                .list_runs(&middle)
                .await
                .expect("list")
                .iter()
                .map(|r| (r.id, r.updated_at, r.version, r.state.clone()))
                .collect::<Vec<_>>(),
            made[500..503]
                .iter()
                .map(|r| (r.id, r.updated_at, r.version, r.state.clone()))
                .collect::<Vec<_>>()
        );
        middle.statuses = Some(vec![RunStatus::Parked]);
        assert!(
            store
                .list_runs(&middle)
                .await
                .expect("list")
                .iter()
                .all(|r| r.status == RunStatus::Parked)
        );
        assert!(
            store
                .list_runs(&RunQuery::new(
                    "a",
                    ConversationScope::Prefix("other:".into()),
                    10
                ))
                .await
                .expect("list")
                .is_empty(),
            "another owner's namespace is not listed"
        );
    }

    /// The head a commit writes is served by `load_head` at the version it was written at; a commit
    /// that writes no head (a delivery, a cancel, a park) leaves it stale, and `load_head` falls
    /// back to the whole state.
    #[tokio::test]
    async fn a_head_round_trips_and_a_headless_commit_makes_it_stale() {
        let store = MemoryStore::new();
        let run = store
            .create_run(NewRun::new("a", json!({"v": 1, "turn": 0})))
            .await
            .expect("create");
        let head = json!({"v": 5, "agent": {"turn": 1}, "body": {"generation": null, "len": 1}});
        store
            .commit_run(
                run.id,
                1,
                RunUpdate::new(RunStatus::Runnable, json!({"v": 1, "turn": 1}))
                    .head(head.clone())
                    .body(BodyWrite {
                        generation: None,
                        rows: vec![BodyRow {
                            idx: 0,
                            message: json!("one"),
                        }],
                        writer: Uuid::new_v4(),
                    }),
            )
            .await
            .expect("commit");
        let loaded = store
            .load_head(run.id)
            .await
            .expect("load")
            .expect("the run");
        assert_eq!(loaded.version, 2);
        assert_eq!(loaded.state["agent"], head["agent"]);
        assert_eq!(loaded.state["body"]["len"], 1);
        assert!(
            loaded.state["body"]["generation"].as_str().is_some(),
            "the store resolved and patched the generation"
        );

        // No head on this commit: the stored one is now stale.
        store
            .commit_run(
                run.id,
                2,
                RunUpdate::new(RunStatus::Runnable, json!({"v": 1, "turn": 2})),
            )
            .await
            .expect("commit");
        let loaded = store
            .load_head(run.id)
            .await
            .expect("load")
            .expect("the run");
        assert_eq!(
            loaded.state,
            json!({"v": 1, "turn": 2}),
            "a stale head is not served: the whole state is"
        );
    }

    /// A normal commit inserts only the rows after the head's `len`: the store's own count of
    /// inserted rows is the delta, not the transcript. (The data looks the same either way,
    /// because a duplicate row is ignored.)
    #[tokio::test]
    async fn a_normal_append_inserts_only_the_delta() {
        let store = MemoryStore::new();
        let run = store
            .create_run(NewRun::new("a", json!({})))
            .await
            .expect("create");
        let generation = Uuid::new_v4();
        let head =
            |len: usize| json!({"v": 5, "agent": {}, "body": {"generation": null, "len": len}});
        store
            .commit_run(
                run.id,
                run.version,
                RunUpdate::new(RunStatus::Runnable, json!({"turn": 1}))
                    .head(head(2))
                    .body(BodyWrite {
                        generation: Some(generation),
                        rows: vec![
                            BodyRow {
                                idx: 0,
                                message: json!("a"),
                            },
                            BodyRow {
                                idx: 1,
                                message: json!("b"),
                            },
                        ],
                        writer: Uuid::new_v4(),
                    }),
            )
            .await
            .expect("the first split");
        store
            .commit_run(
                run.id,
                run.version + 1,
                RunUpdate::new(RunStatus::Runnable, json!({"turn": 2}))
                    .head(head(4))
                    .body(BodyWrite {
                        generation: None,
                        rows: vec![
                            BodyRow {
                                idx: 0,
                                message: json!("a"),
                            },
                            BodyRow {
                                idx: 1,
                                message: json!("b"),
                            },
                            BodyRow {
                                idx: 2,
                                message: json!("c"),
                            },
                            BodyRow {
                                idx: 3,
                                message: json!("d"),
                            },
                        ],
                        writer: Uuid::new_v4(),
                    }),
            )
            .await
            .expect("the append");
        let inner = store.inner.lock().await;
        assert_eq!(
            inner.body_rows_inserted, 4,
            "the append inserted the two new rows, not the four of the transcript"
        );
        assert_eq!(inner.bodies.len(), 4);
    }

    /// The GC keeps the generation the commit names and collects an older superseded one; the
    /// cutoff is explicit, so the one-hour grace is not needed to observe it.
    #[tokio::test]
    async fn the_body_gc_keeps_the_current_generation_and_collects_an_old_one() {
        let store = MemoryStore::new();
        let run = store
            .create_run(NewRun::new("a", json!({})))
            .await
            .expect("create");
        let first = Uuid::from_u128(1);
        let second = Uuid::from_u128(2);
        let writer = Uuid::from_u128(9);
        let rows = |generation: Uuid, text: &str| BodyWrite {
            generation: Some(generation),
            rows: vec![BodyRow {
                idx: 0,
                message: json!(text),
            }],
            writer,
        };
        store
            .commit_run(
                run.id,
                1,
                RunUpdate::new(RunStatus::Runnable, json!({"turn": 1})).body(rows(first, "old")),
            )
            .await
            .expect("commit");
        store
            .commit_run(
                run.id,
                2,
                RunUpdate::new(RunStatus::Runnable, json!({"turn": 2}))
                    .body(rows(second, "current")),
            )
            .await
            .expect("commit");
        assert_eq!(
            store.body_list(run.id, first).await.expect("list"),
            vec![BodyRow {
                idx: 0,
                message: json!("old")
            }]
        );
        assert_eq!(
            store.body_list(run.id, second).await.expect("list"),
            vec![BodyRow {
                idx: 0,
                message: json!("current")
            }]
        );

        // Age the superseded generation past the cutoff, then collect with that cutoff.
        let cutoff = now();
        {
            let mut inner = store.inner.lock().await;
            for ((_, generation, _), slot) in inner.bodies.iter_mut() {
                if *generation == first {
                    slot.written_at = cutoff - SignedDuration::from_hours(2);
                }
            }
            assert_eq!(collect_bodies(&mut inner, run.id, second, cutoff), 1);
        }
        assert!(
            store
                .body_list(run.id, first)
                .await
                .expect("list")
                .is_empty(),
            "the superseded generation is collected"
        );
        assert_eq!(
            store.body_list(run.id, second).await.expect("list").len(),
            1,
            "the current generation is never collected, however old"
        );
        // Even a cutoff in the future leaves the current generation alone.
        {
            let mut inner = store.inner.lock().await;
            assert_eq!(
                collect_bodies(
                    &mut inner,
                    run.id,
                    second,
                    cutoff + SignedDuration::from_hours(24)
                ),
                0
            );
        }
        assert_eq!(
            store.body_list(run.id, second).await.expect("list").len(),
            1
        );
    }
}
