//! [`Ctx`]: what an [`Agent`](crate::Agent) sees while it steps.

use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};

use adam_core::time::Timestamp;
use futures::{StreamExt, TryStreamExt};
use serde::Serialize;
use serde::de::DeserializeOwned;

use adam_core::{DynStore, JournalEntry, RunId, StoreError};

use crate::agent::{AgentError, Inbound};
use crate::cancel::CancelToken;
use crate::child::{ChildStarter, ChildStatus, news_since};
use crate::clock::DynClock;
use crate::events::{Artifact, DynEventSink, RunEvent};
use crate::retry::RetryPolicy;
use crate::runtime::Runtime;

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
    /// How many messages the inbox held when the transition began.
    base_inbox: usize,
    consumed: usize,
    reopen: bool,
    /// See [`Ctx::read_already`].
    read: Vec<String>,
    store: DynStore,
    clock: DynClock,
    cancel: CancelToken,
    emitter: Emitter,
    runtime: Runtime,
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
            id,
            name,
            mime_type,
            data,
            file,
        } = &event
        {
            let mut artifact = Artifact::new(name.clone(), mime_type.clone(), data.clone());
            artifact.file.clone_from(file);
            artifact.id.clone_from(id);
            self.artifacts
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(artifact);
        }
        self.sink.emit(self.run, &self.agent, event).await;
    }

    /// Send `event` as an event of the run `run` instead of this emitter's own: for work this run
    /// does for another one. A child run's model call is its root's work, so its
    /// [`RunEvent::Usage`] goes to the root, where the subscribers of the task are. Best effort, like
    /// every event. An artifact sent to another run is not recorded by this transition (artifacts
    /// are recorded only for the run that commits them); `run` equal to this emitter's run is
    /// [`emit`](Self::emit).
    pub async fn emit_for(&self, run: RunId, event: RunEvent) {
        if run == self.run {
            self.emit(event).await;
        } else {
            self.sink.emit(run, &self.agent, event).await;
        }
    }
}

/// What the runtime needs back from a [`Ctx`] after the transition.
pub(crate) struct CtxOutcome {
    pub seq: u64,
    pub consumed: usize,
    /// See [`Ctx::reopen_on_arrival`].
    pub reopen: bool,
    /// See [`Ctx::read_already`].
    pub read: Vec<String>,
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
    /// The runtime that steps the run.
    pub runtime: Runtime,
}

impl Ctx {
    pub(crate) fn new(p: CtxParts) -> Self {
        Self {
            run: p.run,
            conversation_id: p.conversation_id,
            attempt: p.attempt,
            seq: p.seq,
            base_inbox: p.inbox.len(),
            inbox: p.inbox,
            consumed: 0,
            reopen: false,
            read: Vec::new(),
            store: p.store,
            clock: p.clock,
            cancel: p.cancel,
            runtime: p.runtime,
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
            reopen: self.reopen,
            read: self.read,
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
    pub fn now(&self) -> Timestamp {
        self.clock.now()
    }

    /// Like [`Ctx::now`], but recorded in the journal (consuming one `seq`),
    /// so every replay of this transition sees the same instant.
    pub async fn now_journaled(&mut self) -> Result<Timestamp, AgentError> {
        let clock = self.clock.clone();
        let result: Result<Timestamp, String> = self
            .step(NOW_STEP, || async move { Ok(clock.now()) })
            .await?;
        result.map_err(|e| AgentError::permanent(format!("journaled clock read failed: {e}")))
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
    /// A step is one journaled side effect, and there are several per transition: the span is
    /// `debug`, so a worker at `info` pays nothing for it.
    #[tracing::instrument(level = "debug", skip(self, f), fields(run = %self.run, seq = self.seq))]
    pub async fn step<T, E, F, Fut>(&mut self, name: &str, f: F) -> Result<Result<T, E>, AgentError>
    where
        T: Serialize + DeserializeOwned,
        E: Serialize + DeserializeOwned,
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = Result<T, E>> + Send,
    {
        let seq = self.seq;
        let recorded = self.recorded_at(seq, name).await?;
        let outcome = settle(&self.store, self.run, seq, name, recorded, f).await?;
        self.seq = seq + 1;
        Ok(outcome)
    }

    /// [`step`](Self::step) with its own retry policy, under `key` (ADR 0061): a recorded `Err`
    /// that is retryable ([`Classify::is_retryable`](adam_error::Classify::is_retryable)) fails the
    /// transition with [`AgentError::from_classified`] and
    /// [`with_retry(key, policy)`](AgentError::with_retry), so the run retries it under `policy`
    /// (when the policy retries the error's class) and counts its failures under `key`, apart from
    /// any other step's. Any other `Err` is returned as `step` returns it, for the agent to decide.
    ///
    /// The `key` is what the failures count under: a stable name, the same in every transition
    /// that may run the step (a step's name, never a call id). The policy is recorded at the key's
    /// first failure and read back from the run after that, so changing it in code does not
    /// change the retries of a run already failing.
    ///
    /// # Errors
    ///
    /// As [`step`](Self::step), plus the retryable `Err` above as [`AgentError::Transient`].
    pub async fn step_retrying<T, E, F, Fut>(
        &mut self,
        name: &str,
        key: &str,
        policy: RetryPolicy,
        f: F,
    ) -> Result<Result<T, E>, AgentError>
    where
        T: Serialize + DeserializeOwned,
        E: Serialize + DeserializeOwned + adam_error::Classify + Send + Sync + 'static,
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = Result<T, E>> + Send,
    {
        match self.step(name, f).await? {
            Err(e) if e.is_retryable() => Err(AgentError::from_classified(
                format!("step `{name}` failed"),
                e,
            )
            .with_retry(key, policy)),
            outcome => Ok(outcome),
        }
    }

    /// Take the next `names.len()` seqs, in order, for steps that will run at the same time:
    /// `seq` `k .. k+n-1` for `names[0] .. names[n-1]`, and the next [`step`](Self::step) takes
    /// `k+n` whichever of them finishes first.
    ///
    /// The journal is read at each seq now, so a recorded name that differs from the one asked for
    /// fails at once with [`AgentError::NonDeterminism`] and takes nothing. Each [`ReservedStep`]
    /// then runs with the contract of [`step`](Self::step): the recorded outcome if there is one,
    /// else `f`, recorded at the step's own seq. A step that is reserved and never run leaves a
    /// gap in the journal. That is harmless when the transition then fails (a cancel, an error: the
    /// replay fills the gap), and it is what [`step_all`](Self::step_all) and the agent loop of
    /// `adam-llm-agent` guarantee. A caller that reserves steps and returns `Done` or `Continue`
    /// without running them all makes a later replay find a different journal: **run every
    /// reserved step, or fail the transition**.
    ///
    /// # Errors
    ///
    /// [`AgentError::NonDeterminism`] as above; a store failure is [`AgentError::Store`].
    pub async fn reserve_steps(
        &mut self,
        names: &[String],
    ) -> Result<Vec<ReservedStep>, AgentError> {
        let mut reserved = Vec::with_capacity(names.len());
        for (offset, name) in names.iter().enumerate() {
            let seq = self.seq + offset as u64;
            let recorded = self.recorded_at(seq, name).await?;
            reserved.push(ReservedStep {
                run: self.run,
                seq,
                name: name.clone(),
                recorded,
                store: self.store.clone(),
            });
        }
        self.seq += names.len() as u64;
        Ok(reserved)
    }

    /// Run `steps` (a name and the work of each) at most `at_once` at a time, at seqs reserved in
    /// the order given ([`reserve_steps`](Self::reserve_steps)), and return their outcomes **in the
    /// order given**, whichever finished first. Each is recorded as it finishes; a replay returns
    /// what is recorded and runs only the rest (at-least-once, as [`step`](Self::step)). An
    /// `at_once` of `0` is `1`.
    ///
    /// The first runtime error (a journal failure, a `NonDeterminism`) is returned: no further step
    /// starts and the steps still running are dropped, which is the failure of the transition.
    ///
    /// # Errors
    ///
    /// As [`reserve_steps`](Self::reserve_steps) and [`step`](Self::step).
    pub async fn step_all<T, E, F, Fut>(
        &mut self,
        steps: Vec<(String, F)>,
        at_once: usize,
    ) -> Result<Vec<Result<T, E>>, AgentError>
    where
        T: Serialize + DeserializeOwned,
        E: Serialize + DeserializeOwned,
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = Result<T, E>> + Send,
    {
        let names: Vec<String> = steps.iter().map(|(name, _)| name.clone()).collect();
        let reserved = self.reserve_steps(&names).await?;
        let runs = reserved
            .into_iter()
            .zip(steps)
            .map(|(step, (_, f))| step.run(f));
        futures::stream::iter(runs)
            .buffered(at_once.max(1))
            .try_collect()
            .await
    }

    /// What the journal holds at `seq`, for the step `name`: `None` when nothing is recorded yet.
    async fn recorded_at(&self, seq: u64, name: &str) -> Result<Option<JournalEntry>, AgentError> {
        let found = self
            .store
            .journal_get(self.run, seq)
            .await
            .map_err(store_error)?;
        match found {
            Some(entry) if entry.name != name => Err(AgentError::non_determinism(format!(
                "run {} step {seq}: journal has {:?}, code asked for {name:?}",
                self.run, entry.name
            ))),
            other => Ok(other),
        }
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

    /// The inbound messages [`take_inbox`](Self::take_inbox) would return, left in place.
    ///
    /// For an agent that has to know what the person has said before a step decides what to do
    /// with it, for example one that checks a tool's argument against the person's own words: the
    /// messages stay in the inbox, so whatever steps next (a wrapped agent, say) still takes them.
    /// Reading is not consuming: nothing is committed for a peek.
    pub fn peek_inbox(&self) -> &[Inbound] {
        &self.inbox
    }

    /// How many messages were delivered to this run **while this transition runs**: the ones that
    /// arrived after it began, which [`take_inbox`](Self::take_inbox) and
    /// [`peek_inbox`](Self::peek_inbox) do not show (they show what was there at the start), and
    /// which the next transition reads.
    ///
    /// A child's [`RUN_FINISHED_KIND`](crate::RUN_FINISHED_KIND) notice is not counted. It is a hint
    /// for a parent that waits, and a parent that is finishing has settled its waits, from the
    /// notice or from the store ([`child_status`](Self::child_status)). The notice still wakes a
    /// run that parks.
    ///
    /// **Not journaled**: it is a live read of the run's record, and a replay sees the messages
    /// that are there then. An agent that is about to finish asks it, to answer what a person said
    /// meanwhile instead of ending; [`reopen_on_arrival`](Self::reopen_on_arrival) closes the
    /// window that is left between asking and committing.
    ///
    /// # Errors
    ///
    /// A store failure is [`AgentError::Store`]; a record that does not decode is
    /// [`AgentError::Permanent`].
    pub async fn arrived(&self) -> Result<usize, AgentError> {
        let Some(rec) = self.store.load_run(self.run).await.map_err(store_error)? else {
            return Ok(0);
        };
        // A v5 run's record is its head, which carries the inbox: no body is read.
        let stored = crate::head::Stored::decode(self.run, &rec.state)
            .map_err(|e| AgentError::permanent(format!("the run's record is unreadable: {e}")))?;
        Ok(news_since(stored.inbox(), self.base_inbox, &self.read))
    }

    /// Say which messages the run has read, by id: a copy of one that arrives while this
    /// transition runs is not news.
    ///
    /// A sender whose delivery was not acknowledged delivers the same message again, under the
    /// same id. The agent drops the copy when it reads its inbox, but a copy that lands while the
    /// transition runs would otherwise count in [`arrived`](Self::arrived) and reopen a `Done`
    /// ([`reopen_on_arrival`](Self::reopen_on_arrival)): the run would take a turn with nothing new
    /// to answer. After this call neither counts a message whose id is in `ids` (an empty id is
    /// never matched). The copy stays in the inbox; the next transition, if there is one, drops it.
    /// Each call replaces the ids of the call before.
    pub fn read_already(&mut self, ids: &[String]) {
        self.read = ids.to_vec();
    }

    /// Ask the runtime not to finish the run past a message that arrives while this transition
    /// runs.
    ///
    /// By default a `Done` this transition returns finishes the run, and a message delivered while
    /// it ran, which this transition never saw, stays unread in a run that is over. After this
    /// call, when a message arrived by the time the transition is committed, a returned
    /// [`Transition::Done`](crate::Transition::Done) is committed as
    /// [`Transition::Continue`](crate::Transition::Continue) instead: the run stays runnable with
    /// the state `Done` carried (its output is dropped), and the next transition reads the message
    /// before the agent can finish again. The check is made in the commit itself (the commit is a
    /// compare-and-set on the record, which a delivery changes), so no message can slip in between
    /// the check and the commit. A child's [`RUN_FINISHED_KIND`](crate::RUN_FINISHED_KIND) notice
    /// does not reopen the run, as for [`arrived`](Self::arrived). A `Fail`, a `Park` and a
    /// `Continue` are committed as always (a parked run is woken by any message that arrived,
    /// notices included), and a transition that does not call this is committed as before.
    ///
    /// An agent that calls this promises that its next `step` reads the inbox and that stepping it
    /// again after a `Done` is harmless.
    pub fn reopen_on_arrival(&mut self) {
        self.reopen = true;
    }

    /// Where the child run `run` stands now, read from the store.
    ///
    /// This is the fallback of the [`RUN_FINISHED_KIND`](crate::RUN_FINISHED_KIND) message: a parent
    /// that waits for a child parks with a timer, and when it wakes without the message it asks
    /// here. **Not journaled**: it is a live read, and a replay must see the child as it is then.
    ///
    /// `Ok(None)` when the run does not exist (a child purged after it finished).
    ///
    /// # Errors
    ///
    /// [`AgentError::Permanent`] when `run` is not a child of this run: a run reads its own
    /// children and no others, so an agent cannot probe the results of unrelated runs. A store
    /// failure is [`AgentError::Store`].
    pub async fn child_status(&self, run: RunId) -> Result<Option<ChildStatus>, AgentError> {
        let Some(rec) = self.store.load_run(run).await.map_err(store_error)? else {
            return Ok(None);
        };
        if rec.parent_id != Some(self.run) {
            return Err(AgentError::permanent(format!(
                "run {run} is not a child of run {}",
                self.run
            )));
        }
        Ok(Some(ChildStatus::from_record(&rec)))
    }

    /// The child run `run`'s own state, as its agent last committed it (what its agent serialized,
    /// not the runtime's envelope): for a parent that keeps something its child kept, such as the
    /// tokens its model calls took. **Not journaled**: a live read, like
    /// [`child_status`](Self::child_status); a finished child's state does not change any more.
    ///
    /// A v5 child's state is its head and the body generation it names, so the child's agent must
    /// be registered on this runtime — as an agent or a starter — for its `join` to rebuild the
    /// state (ADR 0055). A runtime that hosts only the parent cannot read a v5 child's whole state.
    ///
    /// `Ok(None)` when the run does not exist (a child purged after it finished).
    ///
    /// # Errors
    ///
    /// [`AgentError::Permanent`] when `run` is not a child of this run, or its record does not
    /// decode. A store failure is [`AgentError::Store`].
    pub async fn child_state(&self, run: RunId) -> Result<Option<serde_json::Value>, AgentError> {
        let Some(rec) = self.store.load_run(run).await.map_err(store_error)? else {
            return Ok(None);
        };
        if rec.parent_id != Some(self.run) {
            return Err(AgentError::permanent(format!(
                "run {run} is not a child of run {}",
                self.run
            )));
        }
        // A v5 child's state is its head and the body it names: joined once, as `Runtime::state`
        // does, because the parent asked for the whole state.
        let claimed =
            self.runtime.inner.claimed(&rec).await.map_err(|e| {
                AgentError::permanent(format!("the child's record is unreadable: {e}"))
            })?;
        Ok(Some(claimed.env.agent))
    }

    /// A handle for starting children of this run on the runtime that steps it: owned and cloneable,
    /// so it can be moved into a [`Ctx::step`] closure (see [`ChildStarter`]).
    pub fn child_starter(&self) -> ChildStarter {
        ChildStarter::new(
            self.runtime.clone(),
            self.run,
            self.conversation_id.as_deref(),
        )
    }

    /// A handle for starting root runs on the runtime that steps this run, only in `adam.schedule:`
    /// conversations: owned and cloneable, so it can be moved into a [`Ctx::step`] closure (see
    /// [`RunStarter`](crate::RunStarter)).
    pub fn run_starter(&self) -> crate::RunStarter {
        crate::RunStarter::new(self.runtime.clone())
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

/// The outcome of the step `name` at `seq`: `recorded` if the journal had it, else `f`'s, written
/// first. If two workers race, the first recording wins and both see it.
async fn settle<T, E, F, Fut>(
    store: &DynStore,
    run: RunId,
    seq: u64,
    name: &str,
    recorded: Option<JournalEntry>,
    f: F,
) -> Result<Result<T, E>, AgentError>
where
    T: Serialize + DeserializeOwned,
    E: Serialize + DeserializeOwned,
    F: FnOnce() -> Fut + Send,
    Fut: Future<Output = Result<T, E>> + Send,
{
    let entry = match recorded {
        Some(entry) => entry,
        None => {
            let (ok, payload) = match f().await {
                Ok(v) => (true, serde_json::to_value(&v)),
                Err(e) => (false, serde_json::to_value(&e)),
            };
            let payload = payload.map_err(|e| {
                AgentError::permanent(format!("step {name:?} result is not serializable"))
                    .with_source(e)
            })?;
            let entry = if ok {
                JournalEntry::ok(seq, name, payload)
            } else {
                JournalEntry::err(seq, name, payload)
            };
            store.journal_put(run, entry).await.map_err(store_error)?
        }
    };
    decode::<T, E>(entry).map_err(|e| {
        AgentError::non_determinism(format!(
            "run {run} step {seq} ({name:?}): recorded result does not decode"
        ))
        .with_source(e)
    })
}

/// A step whose seq [`Ctx::reserve_steps`] took: owned, so several can run at the same time while
/// the `Ctx` is free again.
#[must_use = "a reserved step that is never run leaves a gap in the journal"]
pub struct ReservedStep {
    run: RunId,
    seq: u64,
    name: String,
    recorded: Option<JournalEntry>,
    store: DynStore,
}

impl std::fmt::Debug for ReservedStep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReservedStep")
            .field("run", &self.run)
            .field("seq", &self.seq)
            .field("name", &self.name)
            .field("recorded", &self.recorded.is_some())
            .finish_non_exhaustive()
    }
}

impl ReservedStep {
    /// The journal seq this step records at.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// The name this step records under.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the journal held an outcome for this step when it was reserved: [`run`](Self::run)
    /// then returns it without running anything.
    pub fn is_recorded(&self) -> bool {
        self.recorded.is_some()
    }

    /// The step's outcome: the recorded one, or `f`'s, recorded at this step's seq. The contract
    /// is [`Ctx::step`]'s, including the at-least-once effect and a recorded result that no longer
    /// decodes being [`AgentError::NonDeterminism`].
    ///
    /// # Errors
    ///
    /// The outer `Result` is the runtime's (journal I/O); the inner one is the step's own outcome.
    pub async fn run<T, E, F, Fut>(self, f: F) -> Result<Result<T, E>, AgentError>
    where
        T: Serialize + DeserializeOwned,
        E: Serialize + DeserializeOwned,
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = Result<T, E>> + Send,
    {
        settle(
            &self.store,
            self.run,
            self.seq,
            &self.name,
            self.recorded,
            f,
        )
        .await
    }
}

fn store_error(e: StoreError) -> AgentError {
    match e {
        StoreError::NonDeterminism { .. } => {
            AgentError::non_determinism("the journal disagrees with the code").with_source(e)
        }
        other => AgentError::Store(other),
    }
}

fn decode<T: DeserializeOwned, E: DeserializeOwned>(
    entry: JournalEntry,
) -> Result<Result<T, E>, serde_json::Error> {
    // The entry is the runtime's own: its payload moves into the value, not a copy of it.
    if entry.ok {
        serde_json::from_value(entry.payload).map(Ok)
    } else {
        serde_json::from_value(entry.payload).map(Err)
    }
}

#[cfg(test)]
mod tests {
    use adam_core::MemoryStore;

    use super::*;
    use crate::clock::SystemClock;
    use crate::envelope::Envelope;
    use crate::events::CollectingSink;

    async fn ctx(sink: &CollectingSink) -> Ctx {
        let store: DynStore = Arc::new(MemoryStore::new());
        let runtime = Runtime::builder(store.clone()).build();
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
            runtime,
        })
    }

    /// A clock that never moves: what a journaled read must reproduce byte for byte.
    struct Fixed(Timestamp);

    impl crate::Clock for Fixed {
        fn now(&self) -> Timestamp {
            self.0
        }
    }

    /// `now_journaled` stores a bare timestamp string, with the bytes chrono wrote (`.120` keeps
    /// its three digits), and a replay returns it without reading the clock again.
    #[tokio::test]
    async fn a_journaled_clock_read_is_a_bare_timestamp_string() {
        let sink = CollectingSink::new();
        let mut ctx = ctx(&sink).await;
        let at: Timestamp = "2026-01-02T03:04:05.120Z".parse().expect("a timestamp");
        ctx.clock = Arc::new(Fixed(at));
        assert_eq!(ctx.now_journaled().await.expect("journaled"), at);
        let entry = ctx
            .store
            .journal_get(ctx.run_id(), 0)
            .await
            .expect("get")
            .expect("recorded");
        assert_eq!(entry.payload, serde_json::json!("2026-01-02T03:04:05.120Z"));

        let (store, run) = (ctx.store.clone(), ctx.run_id());
        let mut replay = ctx_on(&store, run, 0);
        replay.clock = Arc::new(Fixed(Timestamp::MAX));
        assert_eq!(replay.now_journaled().await.expect("replayed"), at);
    }

    #[tokio::test]
    async fn peeking_the_inbox_leaves_it_for_the_step_that_takes_it() {
        let sink = CollectingSink::new();
        let mut ctx = ctx(&sink).await;
        assert!(ctx.peek_inbox().is_empty());
        ctx.inbox = vec![
            Inbound::new("message", serde_json::json!({"text": "a"})),
            Inbound::new("message", serde_json::json!({"text": "b"})),
        ];
        assert_eq!(ctx.peek_inbox().len(), 2);
        assert_eq!(ctx.peek_inbox().len(), 2, "a peek consumes nothing");
        assert_eq!(ctx.take_inbox().len(), 2);
        assert!(ctx.peek_inbox().is_empty());
        assert_eq!(ctx.into_outcome().consumed, 2, "only the take is committed");
    }

    #[tokio::test]
    async fn the_child_starter_starts_children_of_this_run_only() {
        let sink = CollectingSink::new();
        let ctx = ctx(&sink).await;
        let starter = ctx.child_starter();
        assert_eq!(starter.parent(), ctx.run_id());
        assert!(format!("{starter:?}").contains("ChildStarter"));
        // The runtime behind it knows no agent, and says so (a registered one is covered by the
        // runtime and `adam-llm-agent` integration tests).
        let refused = starter
            .clone()
            .start(
                RunId::new(),
                "nobody",
                Inbound::new("message", serde_json::json!({})),
            )
            .await
            .unwrap_err();
        assert!(matches!(refused, crate::RuntimeError::UnknownAgent(name) if name == "nobody"));
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
                    id: None,
                    name: "report".into(),
                    mime_type: None,
                    data: serde_json::json!(1),
                    file: None,
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
    async fn an_event_for_another_run_reaches_that_runs_subscribers_and_records_nothing() {
        let sink = CollectingSink::new();
        let ctx = ctx(&sink).await;
        let root = RunId::new();
        let emitter = ctx.emitter();
        let usage = RunEvent::Usage(crate::UsageEvent::new(
            "c1",
            "m",
            adam_model::Usage::new(1, 2),
        ));
        emitter.emit_for(root, usage.clone()).await;
        emitter
            .emit_for(
                root,
                RunEvent::Artifact {
                    id: None,
                    name: "not mine".into(),
                    mime_type: None,
                    data: serde_json::json!(1),
                    file: None,
                },
            )
            .await;
        emitter.emit_for(ctx.run_id(), usage.clone()).await;
        assert_eq!(sink.events_for(root).len(), 2);
        assert_eq!(sink.events_for(root)[0], usage);
        assert_eq!(sink.events_for(ctx.run_id()), vec![usage]);
        assert_eq!(sink.events()[0].agent, "a");
        assert!(ctx.into_outcome().artifacts.is_empty());
    }

    #[tokio::test]
    async fn a_run_reads_the_state_of_its_own_children_only() {
        let sink = CollectingSink::new();
        let ctx = ctx(&sink).await;
        assert!(matches!(ctx.child_state(RunId::new()).await, Ok(None)));
        let mut child = adam_core::NewRun::new(
            "child",
            Envelope::new(serde_json::json!({"usage_totals": []}))
                .encode()
                .expect("an envelope"),
        );
        child.parent_id = Some(ctx.run_id());
        let child = ctx.store.create_run(child).await.expect("create the child");
        assert_eq!(
            ctx.child_state(child.id)
                .await
                .expect("a child")
                .expect("there"),
            serde_json::json!({"usage_totals": []})
        );
        let stranger = ctx
            .store
            .create_run(adam_core::NewRun::new("x", serde_json::json!({})))
            .await
            .expect("create a stranger");
        assert!(matches!(
            ctx.child_state(stranger.id).await,
            Err(AgentError::Permanent { .. })
        ));
    }

    /// A fresh `Ctx` of the run `run` on `store`, as the runtime builds one at `seq`.
    fn ctx_on(store: &DynStore, run: RunId, seq: u64) -> Ctx {
        Ctx::new(CtxParts {
            run,
            agent: "a".into(),
            conversation_id: None,
            attempt: 0,
            seq,
            inbox: Vec::new(),
            store: store.clone(),
            sink: Arc::new(CollectingSink::new()),
            clock: Arc::new(SystemClock),
            cancel: CancelToken::new(),
            runtime: Runtime::builder(store.clone()).build(),
        })
    }

    fn names(of: &[&str]) -> Vec<String> {
        of.iter().map(|n| (*n).to_owned()).collect()
    }

    #[tokio::test]
    async fn reserved_steps_take_consecutive_seqs_and_the_next_step_follows_them() {
        let sink = CollectingSink::new();
        let mut ctx = ctx(&sink).await;
        let reserved = ctx
            .reserve_steps(&names(&["a", "b", "c"]))
            .await
            .expect("reserve");
        let seqs: Vec<(u64, &str)> = reserved.iter().map(|r| (r.seq(), r.name())).collect();
        assert_eq!(seqs, vec![(0, "a"), (1, "b"), (2, "c")]);
        assert!(reserved.iter().all(|r| !r.is_recorded()));
        assert_eq!(ctx.seq, 3);
        let next: Result<Result<u8, String>, AgentError> = ctx.step("d", || async { Ok(4) }).await;
        assert_eq!(next.expect("step").expect("inner"), 4);
        let journal = ctx.store.journal_list(ctx.run_id()).await.expect("journal");
        assert_eq!(
            journal.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![3],
            "the reserved steps were never run: a gap, and `d` at k+n"
        );
        for r in reserved {
            let _: Result<u8, String> = r.run(|| async { Ok(1) }).await.expect("run");
        }
    }

    #[tokio::test]
    async fn a_recorded_name_that_differs_fails_at_the_reservation_and_takes_nothing() {
        let sink = CollectingSink::new();
        let mut ctx = ctx(&sink).await;
        ctx.store
            .journal_put(
                ctx.run_id(),
                JournalEntry::ok(1, "other", serde_json::json!(1)),
            )
            .await
            .expect("seed");
        let refused = ctx.reserve_steps(&names(&["a", "b", "c"])).await;
        assert!(matches!(refused, Err(AgentError::NonDeterminism { .. })));
        assert_eq!(ctx.seq, 0, "nothing was taken");
    }

    #[tokio::test]
    async fn after_a_crash_the_replay_runs_only_the_steps_that_were_not_recorded() {
        let sink = CollectingSink::new();
        let first = ctx(&sink).await;
        let (store, run) = (first.store.clone(), first.run_id());
        let runs = Arc::new(Mutex::new(Vec::<&str>::new()));
        let work = |name: &'static str, runs: Arc<Mutex<Vec<&'static str>>>| {
            move || async move {
                runs.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(name);
                Ok::<u8, String>(1)
            }
        };
        // The first try records the second step only, then dies.
        let mut ctx = first;
        let mut reserved = ctx
            .reserve_steps(&names(&["a", "b", "c"]))
            .await
            .expect("reserve");
        let b = reserved.remove(1);
        b.run(work("b", runs.clone()))
            .await
            .expect("b")
            .expect("ok");
        drop(reserved);

        let mut replay = ctx_on(&store, run, 0);
        let reserved = replay
            .reserve_steps(&names(&["a", "b", "c"]))
            .await
            .expect("reserve again");
        assert_eq!(
            reserved
                .iter()
                .map(ReservedStep::is_recorded)
                .collect::<Vec<_>>(),
            vec![false, true, false]
        );
        for (r, name) in reserved.into_iter().zip(["a", "b", "c"]) {
            r.run(work(name, runs.clone()))
                .await
                .expect("run")
                .expect("ok");
        }
        assert_eq!(
            *runs.lock().unwrap_or_else(PoisonError::into_inner),
            vec!["b", "a", "c"],
            "`b` ran once; the replay ran the gaps"
        );
        let journal = store.journal_list(run).await.expect("journal");
        assert_eq!(
            journal
                .iter()
                .map(|e| (e.seq, e.name.as_str()))
                .collect::<Vec<_>>(),
            vec![(0, "a"), (1, "b"), (2, "c")]
        );
    }

    #[tokio::test]
    async fn step_all_returns_the_outcomes_in_the_order_given_and_records_at_reserved_seqs() {
        tokio::time::pause();
        let sink = CollectingSink::new();
        let mut ctx = ctx(&sink).await;
        let delays = [30u64, 20, 10];
        let steps: Vec<(String, _)> = delays
            .iter()
            .enumerate()
            .map(|(i, ms)| {
                let ms = *ms;
                (format!("s{i}"), move || async move {
                    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                    Ok::<u64, String>(ms)
                })
            })
            .collect();
        let started = tokio::time::Instant::now();
        let out = ctx.step_all(steps, 10).await.expect("step_all");
        assert_eq!(out, vec![Ok(30), Ok(20), Ok(10)]);
        assert!(
            (30..40).contains(&started.elapsed().as_millis()),
            "all three ran at once: {:?}",
            started.elapsed()
        );
        assert_eq!(ctx.seq, 3);
        let journal = ctx.store.journal_list(ctx.run_id()).await.expect("journal");
        assert_eq!(
            journal
                .iter()
                .map(|e| (e.seq, e.name.as_str()))
                .collect::<Vec<_>>(),
            vec![(0, "s0"), (1, "s1"), (2, "s2")]
        );
    }

    /// A 1 MiB step payload survives a fresh step and a replay of it.
    #[tokio::test]
    async fn a_megabyte_step_payload_decodes_on_the_run_and_on_the_replay() {
        let sink = CollectingSink::new();
        let mut ctx = ctx(&sink).await;
        let big = "x".repeat(1024 * 1024);
        let first: Result<Result<String, String>, AgentError> =
            ctx.step("big", || async { Ok(big.clone()) }).await;
        assert_eq!(first.expect("step").expect("ok"), big);
        let recorded = ctx
            .store
            .journal_get(ctx.run_id(), 0)
            .await
            .expect("get")
            .expect("recorded");
        assert_eq!(recorded.payload.as_str().map(str::len), Some(1024 * 1024));

        // The replay reads the recorded 1 MiB back: the closure does not run.
        let (store, run) = (ctx.store.clone(), ctx.run_id());
        let mut replay = ctx_on(&store, run, 0);
        let again: Result<Result<String, String>, AgentError> =
            replay.step("big", || async { Ok(String::new()) }).await;
        assert_eq!(again.expect("replay").expect("ok"), big);
    }

    #[tokio::test]
    async fn a_reserved_step_another_worker_recorded_meanwhile_returns_the_recorded_outcome() {
        let sink = CollectingSink::new();
        let mut ctx = ctx(&sink).await;
        let mut reserved = ctx.reserve_steps(&names(&["a"])).await.expect("reserve");
        let step = reserved.remove(0);
        assert!(!step.is_recorded());
        // Another worker wins the race for the same seq after the reservation.
        ctx.store
            .journal_put(ctx.run_id(), JournalEntry::ok(0, "a", serde_json::json!(1)))
            .await
            .expect("the other worker records");
        let out: Result<u8, String> = step.run(|| async { Ok(2) }).await.expect("run");
        assert_eq!(out, Ok(1), "the first recording wins and both see it");
        let held = ctx.store.journal_get(ctx.run_id(), 0).await.expect("get");
        assert_eq!(held.expect("entry").payload, serde_json::json!(1));
    }

    #[tokio::test]
    async fn step_all_starts_no_step_after_a_runtime_error() {
        let sink = CollectingSink::new();
        let mut ctx = ctx(&sink).await;
        // The first step's recorded outcome no longer decodes: a runtime error when it is read.
        ctx.store
            .journal_put(
                ctx.run_id(),
                JournalEntry::ok(0, "s0", serde_json::json!("not a number")),
            )
            .await
            .expect("seed");
        let started = Arc::new(Mutex::new(Vec::<usize>::new()));
        let steps: Vec<(String, _)> = (0..3)
            .map(|i| {
                let started = started.clone();
                (format!("s{i}"), move || async move {
                    started
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(i);
                    Ok::<u8, String>(1)
                })
            })
            .collect();
        let out = ctx.step_all(steps, 1).await;
        assert!(
            matches!(out, Err(AgentError::NonDeterminism { .. })),
            "{out:?}"
        );
        assert!(
            started
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_empty(),
            "no later step started after the error"
        );
    }

    #[tokio::test]
    async fn step_all_runs_at_most_at_once_at_a_time() {
        tokio::time::pause();
        let sink = CollectingSink::new();
        let mut ctx = ctx(&sink).await;
        let steps: Vec<(String, _)> = (0..4)
            .map(|i| {
                (format!("s{i}"), || async {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    Ok::<u8, String>(1)
                })
            })
            .collect();
        let started = tokio::time::Instant::now();
        ctx.step_all(steps, 2).await.expect("step_all");
        assert!(
            (20..30).contains(&started.elapsed().as_millis()),
            "two at a time: {:?}",
            started.elapsed()
        );
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
