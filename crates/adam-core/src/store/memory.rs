//! In-memory [`Store`] for tests and `adam dev` without a database.
//! Not durable: everything is lost when the process exits.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::Mutex;

use super::{
    ClaimScope, JournalEntry, Lease, NewPushConfig, NewRun, PushProgress, PushRecord, PushState,
    RunId, RunQuery, RunRecord, RunUpdate, Store, StoreError, StoreResult, add_ttl, now,
    storable_owned, truncate_ms,
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
}

struct PushSlot {
    record: PushRecord,
    lease: Option<(String, DateTime<Utc>)>,
}

struct Slot {
    run: RunRecord,
    lease: Option<(String, DateTime<Utc>)>,
    /// Set by the first [`ClaimScope::Pinned`] claim; never cleared.
    owner: Option<String>,
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
            },
        );
        Ok(run)
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
        let mut inner = self.inner.lock().await;
        let current = inner.runs.get(&id).ok_or(StoreError::NotFound(id))?;
        if current.run.version != expected {
            return Err(StoreError::Conflict {
                run: id,
                expected,
                actual: current.run.version,
            });
        }
        if let (true, Some(conv)) = (update.status.is_open(), current.run.conversation_id.clone()) {
            let agent = current.run.agent.clone();
            if inner.open_run(&agent, &conv, Some(id)).is_some() {
                return Err(StoreError::ConversationBusy {
                    agent,
                    conversation_id: conv,
                });
            }
        }
        #[allow(
            clippy::expect_used,
            reason = "presence was checked above under the same lock"
        )]
        let slot = inner.runs.get_mut(&id).expect("checked above");
        slot.run.status = update.status;
        slot.run.state = storable_owned(update.state);
        slot.run.wake_at = update.wake_at.map(truncate_ms);
        slot.run.version += 1;
        slot.run.updated_at = now();
        Ok(slot.run.clone())
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
        now: DateTime<Utc>,
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
        now: DateTime<Utc>,
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

    async fn lease_until(&self, id: RunId) -> StoreResult<Option<DateTime<Utc>>> {
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
        now: DateTime<Utc>,
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

    async fn purge_finished(&self, agent: &str, before: DateTime<Utc>) -> StoreResult<u64> {
        let mut inner = self.inner.lock().await;
        let doomed: Vec<RunId> = inner
            .runs
            .values()
            .filter(|s| {
                s.run.agent == agent && s.run.status.is_terminal() && s.run.updated_at < before
            })
            .map(|s| s.run.id)
            .collect();
        for id in &doomed {
            inner.runs.remove(id);
            inner.push.retain(|(run, _), _| run != id);
            let keys: Vec<_> = inner
                .journal
                .range((*id, 0)..=(*id, u64::MAX))
                .map(|(k, _)| *k)
                .collect();
            for k in keys {
                inner.journal.remove(&k);
            }
        }
        Ok(doomed.len() as u64)
    }
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
            made.push(store.create_run(run).await.expect("create a run"));
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

        // A page in the middle is the slice after its cursor, and the filters still apply.
        let mut middle = RunQuery::new("a", ConversationScope::Prefix("owner:".into()), 3);
        middle.after = Some((made[499].updated_at, made[499].id));
        assert_eq!(
            store
                .list_runs(&middle)
                .await
                .expect("list")
                .iter()
                .map(|r| r.id)
                .collect::<Vec<_>>(),
            expected[500..503]
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
}
