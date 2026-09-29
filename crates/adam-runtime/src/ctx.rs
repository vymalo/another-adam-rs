//! [`Ctx`]: what an [`Agent`](crate::Agent) sees while it steps.

use std::future::Future;
use std::sync::{Mutex, PoisonError};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde::de::DeserializeOwned;

use adam_core::{DynStore, JournalEntry, RunId, StoreError};

use crate::agent::{AgentError, Inbound};
use crate::clock::DynClock;
use crate::events::{Artifact, DynEventSink, RunEvent};

/// Journal name of [`Ctx::now_journaled`].
const NOW_STEP: &str = "ctx.now";

/// Everything the runtime lends an agent for one transition.
///
/// # Replay
///
/// The journal is keyed by `(run, seq)` across all transitions of a run. The
/// committed run state records the next `seq`; a transition starts there, and
/// each [`Ctx::step`] takes the next one. After a crash, the re-executed
/// transition starts at the same `seq`, so steps that already ran return their
/// recorded result instead of running again.
pub struct Ctx {
    run: RunId,
    agent: String,
    conversation_id: Option<String>,
    attempt: u32,
    seq: u64,
    inbox: Vec<Inbound>,
    consumed: usize,
    store: DynStore,
    sink: DynEventSink,
    clock: DynClock,
    artifacts: Mutex<Vec<Artifact>>,
}

/// What the runtime needs back from a [`Ctx`] after the transition.
pub(crate) struct CtxOutcome {
    pub seq: u64,
    pub consumed: usize,
    pub artifacts: Vec<Artifact>,
}

pub(crate) struct CtxParts {
    pub run: RunId,
    pub agent: String,
    pub conversation_id: Option<String>,
    pub attempt: u32,
    pub seq: u64,
    pub inbox: Vec<Inbound>,
    pub store: DynStore,
    pub sink: DynEventSink,
    pub clock: DynClock,
}

impl Ctx {
    pub(crate) fn new(p: CtxParts) -> Self {
        Self {
            run: p.run,
            agent: p.agent,
            conversation_id: p.conversation_id,
            attempt: p.attempt,
            seq: p.seq,
            inbox: p.inbox,
            consumed: 0,
            store: p.store,
            sink: p.sink,
            clock: p.clock,
            artifacts: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn into_outcome(self) -> CtxOutcome {
        CtxOutcome {
            seq: self.seq,
            consumed: self.consumed,
            artifacts: self
                .artifacts
                .into_inner()
                .unwrap_or_else(PoisonError::into_inner),
        }
    }

    /// The run being advanced.
    pub fn run_id(&self) -> RunId {
        self.run
    }

    /// Name of the agent running this transition.
    pub fn agent(&self) -> &str {
        &self.agent
    }

    /// The conversation this run belongs to, if any.
    pub fn conversation_id(&self) -> Option<&str> {
        self.conversation_id.as_deref()
    }

    /// How many earlier tries of this transition failed transiently: `0` on
    /// the first try, `1` on the first retry, and so on. Reset by any
    /// transition that succeeds.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// The current time from the runtime's injected [`Clock`](crate::Clock).
    ///
    /// **Not journaled**: a replay after a crash sees a later time. Use it
    /// for things that may differ between executions (computing a `wake_at`
    /// to park with); use [`Ctx::now_journaled`] when a decision must repeat
    /// identically on replay.
    pub fn now(&self) -> DateTime<Utc> {
        self.clock.now()
    }

    /// Like [`Ctx::now`], but recorded in the journal (consuming one `seq`),
    /// so every replay of this transition sees the same instant.
    pub async fn now_journaled(&mut self) -> Result<DateTime<Utc>, AgentError> {
        let clock = self.clock.clone();
        let result: Result<DateTime<Utc>, String> = self
            .step(NOW_STEP, || async move { Ok(clock.now()) })
            .await?;
        result.map_err(|e| AgentError::Permanent(format!("journaled clock read failed: {e}")))
    }

    /// Journaled side effect.
    ///
    /// The first execution's result, `Ok` or `Err`, is recorded at the next
    /// `seq`; on replay it is returned without running `f`. A different `name`
    /// at a recorded `seq` fails with [`AgentError::NonDeterminism`], as does
    /// a recorded result that no longer decodes as `T`/`E`. If two workers
    /// race on the same step, the first recording wins and both see it.
    ///
    /// The outer `Result` is the runtime's (journal I/O); the inner one is the
    /// step's own outcome.
    ///
    /// # Retries
    ///
    /// When a transition fails with [`AgentError::Transient`] and is retried,
    /// the journal entries of the failed try are abandoned (the retry starts at
    /// a fresh `seq`), so its steps run again. That is what lets a recorded
    /// `Err` be tried once more.
    ///
    /// # Guarantees
    ///
    /// Once a step's outcome is recorded, it is never executed again: every
    /// replay (after a crash, a lost lease or a stale commit) returns the
    /// recorded outcome. The effect itself is **at-least-once**: `f` runs
    /// before its outcome is written, so a crash between `f` completing and
    /// the journal write landing makes the replay run `f` again, and a
    /// transient retry re-runs the steps of the failed try. Make side effects
    /// idempotent (idempotency keys, "create if absent", upserts) when running
    /// them twice would be harmful.
    #[tracing::instrument(skip(self, f), fields(run = %self.run, seq = self.seq))]
    pub async fn step<T, E, F, Fut>(&mut self, name: &str, f: F) -> Result<Result<T, E>, AgentError>
    where
        T: Serialize + DeserializeOwned,
        E: Serialize + DeserializeOwned,
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = Result<T, E>> + Send,
    {
        let seq = self.seq;
        let entry = match self
            .store
            .journal_get(self.run, seq)
            .await
            .map_err(store_error)?
        {
            Some(entry) => {
                if entry.name != name {
                    return Err(AgentError::NonDeterminism(format!(
                        "run {} step {seq}: journal has {:?}, code asked for {name:?}",
                        self.run, entry.name
                    )));
                }
                entry
            }
            None => {
                let (ok, payload) = match f().await {
                    Ok(v) => (true, serde_json::to_value(&v)),
                    Err(e) => (false, serde_json::to_value(&e)),
                };
                let payload = payload.map_err(|e| {
                    AgentError::Permanent(format!("step {name:?} result is not serializable: {e}"))
                })?;
                let entry = if ok {
                    JournalEntry::ok(seq, name, payload)
                } else {
                    JournalEntry::err(seq, name, payload)
                };
                self.store
                    .journal_put(self.run, entry)
                    .await
                    .map_err(store_error)?
            }
        };
        let decoded = decode::<T, E>(&entry).map_err(|e| {
            AgentError::NonDeterminism(format!(
                "run {} step {seq} ({name:?}): recorded result does not decode: {e}",
                self.run
            ))
        })?;
        self.seq = seq + 1;
        Ok(decoded)
    }

    /// Drain the inbound messages delivered before this transition started
    /// (and not consumed by an earlier one).
    ///
    /// Consumption is committed together with the transition, so a crash
    /// re-delivers the same messages to the replay. It is not journaled:
    /// messages that arrive between a crash and its replay are visible to the
    /// replay only. Messages delivered *during* a transition are seen by the
    /// next one (a `Park` then resumes at once).
    pub fn take_inbox(&mut self) -> Vec<Inbound> {
        let taken = std::mem::take(&mut self.inbox);
        self.consumed += taken.len();
        taken
    }

    /// Progress for observers (A2A streaming, UIs). Not durable, best effort,
    /// with one exception: [`RunEvent::Artifact`] is also recorded with the
    /// transition's commit (see `RunView::artifacts`), and dropped again if
    /// the transition is retried.
    pub async fn emit(&self, event: RunEvent) {
        if let RunEvent::Artifact {
            name,
            mime_type,
            data,
        } = &event
        {
            self.artifacts
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(Artifact {
                    name: name.clone(),
                    mime_type: mime_type.clone(),
                    data: data.clone(),
                });
        }
        self.sink.emit(self.run, &self.agent, event).await;
    }
}

fn store_error(e: StoreError) -> AgentError {
    match e {
        StoreError::NonDeterminism { .. } => AgentError::NonDeterminism(e.to_string()),
        other => AgentError::Store(other),
    }
}

fn decode<T: DeserializeOwned, E: DeserializeOwned>(
    entry: &JournalEntry,
) -> Result<Result<T, E>, serde_json::Error> {
    if entry.ok {
        serde_json::from_value(entry.payload.clone()).map(Ok)
    } else {
        serde_json::from_value(entry.payload.clone()).map(Err)
    }
}
