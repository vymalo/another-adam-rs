//! [`StorePushStore`]: A2A push-notification state in the run store.
//!
//! The configs and each one's delivery progress live next to the run they belong to
//! ([`Store::push_put`](adam_core::Store::push_put) and its siblings), so a restart or another
//! replica loses nothing and a purged run takes its configs with it. Only the mapping between
//! the A2A types and the store's opaque JSON is here.

use std::time::Duration;

use a2a::TaskPushNotificationConfig;
use adam_a2a::push::{
    NewPushConfig, PushCursor, PushProgress, PushRecord, PushState, PushStore, PushStoreError,
};
use adam_core::{DynStore, RunId, StoreError};
use adam_error::{Classify, ErrorClass};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

/// A [`PushStore`] over an `adam_core::Store`, for one agent.
#[derive(Clone)]
pub struct StorePushStore {
    store: DynStore,
    agent: String,
}

impl std::fmt::Debug for StorePushStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorePushStore")
            .field("agent", &self.agent)
            .finish_non_exhaustive()
    }
}

impl StorePushStore {
    /// The push state of `agent`'s tasks, kept in `store`.
    pub fn new(store: DynStore, agent: impl Into<String>) -> Self {
        Self {
            store,
            agent: agent.into(),
        }
    }
}

fn run_id(task_id: &str) -> Option<RunId> {
    Uuid::parse_str(task_id).ok().map(RunId)
}

fn map_err(err: StoreError) -> PushStoreError {
    match err {
        StoreError::NotFound(_) => PushStoreError::NotFound,
        StoreError::Conflict { .. } => PushStoreError::Conflict,
        err => match err.class() {
            ErrorClass::Transient | ErrorClass::RateLimited | ErrorClass::Conflict => {
                PushStoreError::Unavailable(Box::new(err))
            }
            _ => PushStoreError::Internal(Box::new(err)),
        },
    }
}

fn state_in(state: adam_core::PushState) -> PushState {
    match state {
        adam_core::PushState::Active => PushState::Active,
        adam_core::PushState::Done => PushState::Done,
        adam_core::PushState::GaveUp => PushState::GaveUp,
    }
}

fn state_out(state: PushState) -> adam_core::PushState {
    match state {
        PushState::Active => adam_core::PushState::Active,
        PushState::Done => adam_core::PushState::Done,
        PushState::GaveUp => adam_core::PushState::GaveUp,
    }
}

/// The record as the port has it. A config the store holds but that cannot be read (a damaged
/// document) is `Err`: the caller decides what to do with that one record.
fn record_in(rec: adam_core::PushRecord) -> Result<PushRecord, Box<adam_core::PushRecord>> {
    let Ok(config) = serde_json::from_value::<TaskPushNotificationConfig>(rec.config.clone())
    else {
        return Err(Box::new(rec));
    };
    // A cursor that cannot be read starts over: what the webhook already heard may be sent again
    // (at least once), which is better than never sending again.
    let cursor = serde_json::from_value::<PushCursor>(rec.cursor.clone()).unwrap_or_default();
    Ok(PushRecord {
        task_id: rec.run.to_string(),
        id: rec.id,
        owner: rec.owner,
        config,
        cursor,
        state: state_in(rec.state),
        attempts: rec.attempts,
        last_error: rec.last_error,
        next_attempt_at: rec.next_attempt_at,
        version: rec.version,
    })
}

#[async_trait]
impl PushStore for StorePushStore {
    async fn put(&self, new: NewPushConfig) -> Result<PushRecord, PushStoreError> {
        let run = run_id(&new.task_id).ok_or(PushStoreError::NotFound)?;
        let config =
            serde_json::to_value(&new.config).map_err(|e| PushStoreError::Internal(Box::new(e)))?;
        let cursor =
            serde_json::to_value(&new.cursor).map_err(|e| PushStoreError::Internal(Box::new(e)))?;
        let rec = self
            .store
            .push_put(adam_core::NewPushConfig {
                run,
                id: new.id,
                agent: self.agent.clone(),
                owner: new.owner,
                config,
                cursor,
            })
            .await
            .map_err(map_err)?;
        record_in(rec).map_err(|_| {
            PushStoreError::Internal("a push config that was just stored cannot be read".into())
        })
    }

    async fn list(&self, task_id: &str) -> Result<Vec<PushRecord>, PushStoreError> {
        let Some(run) = run_id(task_id) else {
            return Ok(Vec::new());
        };
        let records = self.store.push_list(run).await.map_err(map_err)?;
        // A record that cannot be read is left out of the answer (and is not the client's
        // business); `claim_due` is where it is dealt with.
        Ok(records
            .into_iter()
            .filter_map(|r| record_in(r).ok())
            .collect())
    }

    async fn delete(&self, task_id: &str, id: &str) -> Result<bool, PushStoreError> {
        let Some(run) = run_id(task_id) else {
            return Ok(false);
        };
        self.store.push_delete(run, id).await.map_err(map_err)
    }

    async fn claim_due(
        &self,
        worker: &str,
        now: DateTime<Utc>,
        ttl: Duration,
        limit: usize,
    ) -> Result<Vec<PushRecord>, PushStoreError> {
        let claimed = self
            .store
            .push_claim_due(std::slice::from_ref(&self.agent), worker, now, ttl, limit)
            .await
            .map_err(map_err)?;
        let mut records = Vec::with_capacity(claimed.len());
        for rec in claimed {
            match record_in(rec) {
                Ok(record) => records.push(record),
                Err(rec) => {
                    // A damaged config cannot be delivered, now or later: say so and stop.
                    tracing::error!(run = %rec.run, config_id = %rec.id, "a stored push config cannot be read; giving up on it");
                    let _ = self
                        .store
                        .push_commit(
                            rec.run,
                            &rec.id,
                            rec.version,
                            adam_core::PushProgress {
                                state: adam_core::PushState::GaveUp,
                                cursor: rec.cursor.clone(),
                                attempts: rec.attempts,
                                last_error: Some("the stored configuration cannot be read".into()),
                                next_attempt_at: now,
                            },
                        )
                        .await;
                }
            }
        }
        Ok(records)
    }

    async fn commit(
        &self,
        task_id: &str,
        id: &str,
        expected_version: u64,
        progress: PushProgress,
    ) -> Result<PushRecord, PushStoreError> {
        let run = run_id(task_id).ok_or(PushStoreError::NotFound)?;
        let cursor = serde_json::to_value(&progress.cursor)
            .map_err(|e| PushStoreError::Internal(Box::new(e)))?;
        let rec = self
            .store
            .push_commit(
                run,
                id,
                expected_version,
                adam_core::PushProgress {
                    state: state_out(progress.state),
                    cursor,
                    attempts: progress.attempts,
                    last_error: progress.last_error,
                    next_attempt_at: progress.next_attempt_at,
                },
            )
            .await
            .map_err(map_err)?;
        record_in(rec).map_err(|_| {
            PushStoreError::Internal("a push config that was just stored cannot be read".into())
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use adam_core::{MemoryStore, NewRun};
    use serde_json::json;

    use super::*;

    fn config(url: &str) -> TaskPushNotificationConfig {
        TaskPushNotificationConfig {
            url: url.into(),
            id: Some("c1".into()),
            task_id: String::new(),
            token: Some("t".into()),
            authentication: None,
            tenant: None,
        }
    }

    #[tokio::test]
    async fn configs_round_trip_through_the_store_and_a_task_id_that_is_no_run_is_not_found() {
        let store: DynStore = Arc::new(MemoryStore::new());
        let run = store
            .create_run(NewRun::new("agent", json!({})))
            .await
            .unwrap();
        let push = StorePushStore::new(store.clone(), "agent");
        let task = run.id.to_string();
        let put = push
            .put(NewPushConfig {
                task_id: task.clone(),
                id: "c1".into(),
                owner: "token-0".into(),
                config: config("https://hooks.example.com/x"),
                cursor: PushCursor::default(),
            })
            .await
            .unwrap();
        assert_eq!((put.version, put.state), (1, PushState::Active));
        assert_eq!(put.config.token.as_deref(), Some("t"));
        let listed = push.list(&task).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].owner, "token-0");
        // Not a run id: nothing to find, and nothing can be put.
        assert!(push.list("nope").await.unwrap().is_empty());
        assert!(!push.delete("nope", "c1").await.unwrap());
        assert!(matches!(
            push.put(NewPushConfig {
                task_id: "nope".into(),
                id: "c1".into(),
                owner: "o".into(),
                config: config("https://hooks.example.com/x"),
                cursor: PushCursor::default()
            })
            .await,
            Err(PushStoreError::NotFound)
        ));
        // A run that does not exist is not found either.
        assert!(matches!(
            push.put(NewPushConfig {
                task_id: RunId::new().to_string(),
                id: "c1".into(),
                owner: "o".into(),
                config: config("https://hooks.example.com/x"),
                cursor: PushCursor::default()
            })
            .await,
            Err(PushStoreError::NotFound)
        ));
        // Claim and commit, and a stale version conflicts.
        let claimed = push
            .claim_due(
                "w",
                Utc::now() + chrono::Duration::seconds(1),
                Duration::from_secs(30),
                5,
            )
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1);
        let progress = |state| PushProgress {
            state,
            cursor: PushCursor::default(),
            attempts: 0,
            last_error: None,
            next_attempt_at: Utc::now(),
        };
        let done = push
            .commit(&task, "c1", claimed[0].version, progress(PushState::Done))
            .await
            .unwrap();
        assert_eq!((done.version, done.state), (2, PushState::Done));
        assert!(matches!(
            push.commit(&task, "c1", 1, progress(PushState::Active))
                .await,
            Err(PushStoreError::Conflict)
        ));
        // Another agent's push store never claims this agent's configs.
        let other = StorePushStore::new(store, "other-agent");
        assert!(
            other
                .claim_due(
                    "w",
                    Utc::now() + chrono::Duration::seconds(1),
                    Duration::from_secs(30),
                    5
                )
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_damaged_config_is_given_up_on_and_not_returned() {
        let store: DynStore = Arc::new(MemoryStore::new());
        let run = store
            .create_run(NewRun::new("agent", json!({})))
            .await
            .unwrap();
        store
            .push_put(adam_core::NewPushConfig {
                run: run.id,
                id: "bad".into(),
                agent: "agent".into(),
                owner: "o".into(),
                config: json!({"not": "a config"}),
                cursor: json!("not a cursor"),
            })
            .await
            .unwrap();
        let push = StorePushStore::new(store.clone(), "agent");
        assert!(push.list(&run.id.to_string()).await.unwrap().is_empty());
        let claimed = push
            .claim_due(
                "w",
                Utc::now() + chrono::Duration::seconds(1),
                Duration::from_secs(30),
                5,
            )
            .await
            .unwrap();
        assert!(claimed.is_empty());
        let rec = &store.push_list(run.id).await.unwrap()[0];
        assert_eq!(rec.state, adam_core::PushState::GaveUp);
        assert!(
            rec.last_error
                .as_deref()
                .unwrap()
                .contains("cannot be read")
        );
    }
}
