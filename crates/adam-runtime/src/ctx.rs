//! [`Ctx`]: what an [`Agent`](crate::Agent) sees while it steps.

use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde::de::DeserializeOwned;

use adam_core::{DynStore, JournalEntry, RunId, StoreError};

use crate::agent::{AgentError, Inbound};
use crate::cancel::CancelToken;
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
    conversation_id: Option<String>,
    attempt: u32,
    seq: u64,
    inbox: Vec<Inbound>,
    consumed: usize,
    store: DynStore,
    clock: DynClock,
    cancel: CancelToken,
    emitter: Emitter,
}

/// A cloneable, owned handle for emitting [`RunEvent`]s of one run.
///
/// [`Ctx::emit`] borrows the `Ctx`, which cannot be held across a
/// [`Ctx::step`] (that takes `&mut self`) or moved into a spawned task. Take an
/// `Emitter` with [`Ctx::emitter`] before the step and move it in instead. It
/// behaves exactly like [`Ctx::emit`], including [`RunEvent::Artifact`] being
/// recorded with the transition's commit, and is `'static`.
#[derive(Clone)]
pub struct Emitter {
    run: RunId,
    agent: Arc<str>,
    sink: DynEventSink,
    artifacts: Arc<Mutex<Vec<Artifact>>>,
}

impl std::fmt::Debug for Emitter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Emitter")
            .field("run", &self.run)
            .field("agent", &self.agent)
            .finish_non_exhaustive()
    }
}

impl Emitter {
    /// An emitter detached from any [`Ctx`], for tests and for code that runs
    /// outside a transition. Events go to `sink`; artifacts are forwarded but
    /// recorded nowhere, since no transition commits them.
    pub fn new(run: RunId, agent: impl Into<String>, sink: DynEventSink) -> Self {
        Self {
            run,
            agent: Arc::from(agent.into()),
            sink,
            artifacts: Arc::default(),
        }
    }

    /// The run the events belong to.
    pub fn run_id(&self) -> RunId {
        self.run
    }

    /// Name of the agent the events are attributed to.
    pub fn agent(&self) -> &str {
        &self.agent
    }

    /// Same contract as [`Ctx::emit`]: best effort, not durable, except that
    /// [`RunEvent::Artifact`] is also recorded with the transition's commit
    /// (dropped again if the transition is retried).
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
    pub cancel: CancelToken,
}

impl Ctx {
    pub(crate) fn new(p: CtxParts) -> Self {
        Self {
            run: p.run,
            conversation_id: p.conversation_id,
            attempt: p.attempt,
            seq: p.seq,
            inbox: p.inbox,
            consumed: 0,
            store: p.store,
            clock: p.clock,
            cancel: p.cancel,
            emitter: Emitter {
                run: p.run,
                agent: Arc::from(p.agent),
                sink: p.sink,
                artifacts: Arc::default(),
            },
        }
    }

    pub(crate) fn into_outcome(self) -> CtxOutcome {
        CtxOutcome {
            seq: self.seq,
            consumed: self.consumed,
            artifacts: std::mem::take(
                &mut *self
                    .emitter
                    .artifacts
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner),
            ),
        }
    }

    /// The run being advanced.
    pub fn run_id(&self) -> RunId {
        self.run
    }

    /// Name of the agent running this transition.
    pub fn agent(&self) -> &str {
        &self.emitter.agent
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

    /// A handle on this transition's cancellation signal, cloneable and
    /// `'static`, so it can be moved into a [`Ctx::step`] closure or a spawned
    /// task (this `Ctx` cannot: `step` borrows it mutably). See [`CancelToken`].
    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    /// Whether the run was cancelled (or finished by someone else) while this
    /// transition runs. See [`CancelToken`].
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Resolves when the run is cancelled: `select!` it against long work to
    /// stop early. Never resolves for a run that is not cancelled. See
    /// [`CancelToken`].
    pub async fn cancelled(&self) {
        self.cancel.cancelled().await;
    }

    /// Progress for observers (A2A streaming, UIs). Not durable, best effort,
    /// with one exception: [`RunEvent::Artifact`] is also recorded with the
    /// transition's commit (see `RunView::artifacts`), and dropped again if
    /// the transition is retried.
    pub async fn emit(&self, event: RunEvent) {
        self.emitter.emit(event).await;
    }

    /// An owned handle that emits like [`Ctx::emit`] but does not borrow the
    /// `Ctx`, so it can be moved into a [`Ctx::step`] closure or a spawned
    /// task. See [`Emitter`].
    pub fn emitter(&self) -> Emitter {
        self.emitter.clone()
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

#[cfg(test)]
mod tests {
    use adam_core::MemoryStore;

    use super::*;
    use crate::clock::SystemClock;
    use crate::events::CollectingSink;

    async fn ctx(sink: &CollectingSink) -> Ctx {
        let store: DynStore = Arc::new(MemoryStore::new());
        let run = store
            .create_run(adam_core::NewRun::new("a", serde_json::json!({})))
            .await
            .expect("create run");
        Ctx::new(CtxParts {
            run: run.id,
            agent: "a".into(),
            conversation_id: None,
            attempt: 0,
            seq: 0,
            inbox: Vec::new(),
            store,
            sink: Arc::new(sink.clone()),
            clock: Arc::new(SystemClock),
            cancel: CancelToken::new(),
        })
    }

    #[tokio::test]
    async fn emitter_outlives_borrows_and_records_artifacts_like_ctx_emit() {
        let sink = CollectingSink::new();
        let mut ctx = ctx(&sink).await;
        let emitter = ctx.emitter();
        assert_eq!(emitter.run_id(), ctx.run_id());
        assert_eq!(emitter.agent(), "a");

        // Usable from another task while the Ctx is borrowed mutably.
        let task = tokio::spawn(async move {
            emitter
                .emit(RunEvent::Progress {
                    message: "working".into(),
                })
                .await;
            emitter
                .emit(RunEvent::Artifact {
                    name: "report".into(),
                    mime_type: None,
                    data: serde_json::json!(1),
                })
                .await;
        });
        let step: Result<Result<u8, String>, AgentError> =
            ctx.step("noop", || async { Ok(1) }).await;
        assert_eq!(step.expect("step").expect("inner"), 1);
        task.await.expect("emitter task");
        ctx.emit(RunEvent::Progress {
            message: "from ctx".into(),
        })
        .await;

        let events = sink.events_for(ctx.run_id());
        assert_eq!(events.len(), 3);
        assert!(matches!(&events[2], RunEvent::Progress { message } if message == "from ctx"));
        let outcome = ctx.into_outcome();
        assert_eq!(outcome.artifacts.len(), 1);
        assert_eq!(outcome.artifacts[0].name, "report");
    }

    #[tokio::test]
    async fn detached_emitter_forwards_to_its_sink() {
        let sink = CollectingSink::new();
        let run = RunId::new();
        let emitter = Emitter::new(run, "x", Arc::new(sink.clone()));
        emitter
            .emit(RunEvent::Progress {
                message: "hi".into(),
            })
            .await;
        assert_eq!(sink.events()[0].agent, "x");
        assert_eq!(sink.events_for(run).len(), 1);
    }
}
