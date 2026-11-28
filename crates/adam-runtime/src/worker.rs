//! The worker loop: claim due runs, advance them one transition, commit with
//! CAS, renew leases while stepping, release.

use std::collections::HashSet;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use adam_core::time::Timestamp;
use futures::stream::BoxStream;
use futures::{FutureExt, StreamExt};
use serde_json::Value;
use tokio::task::{JoinHandle, JoinSet};

use adam_core::{
    BlobId, BodyRow, BodyWrite, DynStore, Lease, RunId, RunRecord, RunStatus, RunUpdate,
    StoreError, StoreResult,
};
use adam_error::{Classify, ErrorClass, report};
use tracing::Instrument as _;
use uuid::Uuid;

use crate::agent::{AgentError, Transition};
use crate::blob::blob_id;
use crate::cancel::CancelToken;
use crate::child::news_since;
use crate::ctx::{Ctx, CtxOutcome, CtxParts};
use crate::envelope::Envelope;
use crate::erased::{ErasedAgent, Stepped};
use crate::events::Artifact;
use crate::head::{Claimed, Head, HeadParts, Stored};
use crate::notify::{Delivery, Signal};
use crate::retry::{
    Decision, RUNTIME_RETRY_KEY, RetryRecord, StepRetry, Verdict, class_name, decide,
};
use crate::runtime::{Inner, MAX_COMMIT_RETRIES, Runtime, RuntimeError};
use crate::seal::{Seal, Sealer, attach, chain_over};

type InFlight = Arc<Mutex<HashSet<RunId>>>;

/// The `detail` of the status event a worker emits when it first claims a run.
pub(crate) const CLAIMED: &str = "claimed";

impl Runtime {
    /// Worker loop until `shutdown` resolves: claim due runs, advance each by
    /// one transition, commit it (compare-and-swap), renew the lease while
    /// stepping, release it.
    ///
    /// Up to `concurrency` runs are advanced at the same time, at most one
    /// step per run. A run that `Continue`s is committed, released and picked
    /// up again by the next claim (by any worker, unless the runtime claims with
    /// [`ClaimScope::Pinned`](adam_core::ClaimScope::Pinned): then only by the run's owner), which keeps scheduling fair.
    ///
    /// On shutdown no new runs are claimed, in-flight transitions finish and
    /// commit, and their leases are released before this returns. To stop
    /// harder, drop the future: in-flight steps are aborted and their runs are
    /// picked up by another worker once the leases expire.
    #[tracing::instrument(skip_all, fields(worker = %self.inner.cfg.worker_id))]
    pub async fn run_worker(
        &self,
        shutdown: impl Future<Output = ()> + Send,
    ) -> Result<(), RuntimeError> {
        let inner = &self.inner;
        tokio::pin!(shutdown);
        let mut wake = inner.wake.subscribe();
        let mut tasks: JoinSet<()> = JoinSet::new();
        let in_flight: InFlight = Arc::default();
        // Only full agents are claimed: a starter-only registration can start
        // a run but has no `step`, so its runs are left to a worker that has.
        let agents: Vec<String> = inner
            .agents
            .iter()
            .filter(|(_, r)| r.agent().is_some())
            .map(|(name, _)| name.clone())
            .collect();
        if agents.is_empty() {
            tracing::warn!("no agent to step is registered, this worker claims nothing");
        }
        // Subscribe before the first claim, so a signal published from here
        // on is not missed. Held until this function returns.
        let _signals = inner.notifier.as_ref().map(|notifier| {
            spawn_signal_consumer(
                inner.clone(),
                notifier.subscribe(),
                agents.iter().cloned().collect(),
            )
        });

        loop {
            if shutdown.as_mut().now_or_never().is_some() {
                break;
            }
            while let Some(joined) = tasks.try_join_next() {
                log_join(joined);
            }
            // Mark local wake-ups seen *before* claiming, so one arriving
            // during the claim still ends the wait below.
            wake.borrow_and_update();

            let free = inner.cfg.concurrency.saturating_sub(tasks.len());
            if free > 0 && !agents.is_empty() {
                // What we are stepping is not offered to the claim, even when its lease has
                // lapsed under a slow step. Only this loop adds to the set, so a run outside it
                // now is still outside it when the claim returns, and a step that left it has
                // committed and released before: no claim returns a snapshot that is older than
                // what this worker wrote, and none is cleared by the release of an earlier step.
                let busy: Vec<RunId> = in_flight
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .iter()
                    .copied()
                    .collect();
                let claimed = inner
                    .store
                    .claim_due(
                        &agents,
                        &inner.cfg.worker_id,
                        inner.cfg.claim_scope,
                        &busy,
                        inner.clock.now(),
                        inner.cfg.lease_ttl,
                        free,
                    )
                    .await;
                match claimed {
                    Ok(leases) => {
                        for lease in leases {
                            let run = lease.run.id;
                            if !in_flight
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .insert(run)
                            {
                                // A store that honours `busy` never returns a run we are
                                // stepping. One that does not must not make us step it twice:
                                // hand the lease back, so it is not extended behind the
                                // step's back. The version CAS settles who wins.
                                tracing::error!(%run, "the store claimed a run that is still in flight, releasing it");
                                if let Err(e) =
                                    inner.store.release_lease(run, &inner.cfg.worker_id).await
                                {
                                    tracing::warn!(%run, error = %e, "releasing the lease failed");
                                }
                                continue;
                            }
                            let guard = InFlightGuard {
                                set: in_flight.clone(),
                                run,
                            };
                            tasks.spawn(advance(inner.clone(), lease, guard));
                        }
                    }
                    Err(e) => tracing::error!(error = %e, "claiming due runs failed"),
                }
            }

            let has_capacity = inner.cfg.concurrency > tasks.len();
            let joined = async {
                if tasks.is_empty() {
                    std::future::pending().await
                } else {
                    tasks.join_next().await
                }
            };
            tokio::select! {
                biased;
                () = &mut shutdown => break,
                Some(res) = joined => log_join(res),
                _ = wake.changed(), if has_capacity => {}
                () = tokio::time::sleep(inner.cfg.poll_interval), if has_capacity => {}
            }
        }

        // Graceful: let in-flight transitions finish, commit and release, for at most the
        // drain grace when one is set.
        let drain = async {
            while let Some(joined) = tasks.join_next().await {
                log_join(joined);
            }
        };
        let finished = match inner.cfg.drain_grace {
            Some(grace) => tokio::time::timeout(grace, drain).await.is_ok(),
            None => {
                drain.await;
                true
            }
        };
        if !finished {
            // Out of time: abort what is left. A step that was aborted committed nothing, so the
            // run is as the last commit left it; hand the lease back so another worker takes it
            // now.
            let left: Vec<RunId> = in_flight
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .copied()
                .collect();
            tracing::warn!(
                runs = left.len(),
                "drain grace over, aborting the steps in flight"
            );
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
            for run in left {
                if let Err(e) = inner.store.release_lease(run, &inner.cfg.worker_id).await {
                    tracing::warn!(%run, error = %e, "releasing the lease failed; it will expire");
                }
            }
        }
        Ok(())
    }
}

fn log_join(res: Result<(), tokio::task::JoinError>) {
    if let Err(e) = res
        && e.is_panic()
    {
        tracing::error!("run task panicked: {e}");
    }
}

/// Removes the run from the worker's in-flight set, even on unwind.
struct InFlightGuard {
    set: InFlight,
    run: RunId,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.set
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.run);
    }
}

struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn spawn_renewer(inner: Arc<Inner>, run: RunId) -> AbortOnDrop {
    let every = (inner.cfg.lease_ttl / 3).max(Duration::from_millis(1));
    AbortOnDrop(tokio::spawn(async move {
        loop {
            tokio::time::sleep(every).await;
            match inner
                .store
                .renew_lease(
                    run,
                    &inner.cfg.worker_id,
                    inner.clock.now(),
                    inner.cfg.lease_ttl,
                )
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    tracing::warn!(%run, "lease lost while stepping; the commit will be rejected if another worker advanced the run");
                    return;
                }
                Err(e) => tracing::warn!(%run, error = %e, "lease renewal failed"),
            }
        }
    }))
}

/// Turns the notifier's deliveries into local effects: a run of an agent this
/// worker steps became runnable, poll now; a run was cancelled, fire its token
/// if we are stepping it; signals may have been lost, do both checks for
/// everything. Only hints: whatever they miss the poll and the cancel watch
/// find.
fn spawn_signal_consumer(
    inner: Arc<Inner>,
    mut deliveries: BoxStream<'static, Delivery>,
    agents: HashSet<String>,
) -> AbortOnDrop {
    AbortOnDrop(tokio::spawn(async move {
        while let Some(delivery) = deliveries.next().await {
            match delivery {
                Delivery::Signal(Signal::Runnable { agent, .. }) => {
                    if agents.contains(&agent) {
                        inner.notify_workers();
                    }
                }
                Delivery::Signal(Signal::Finished { run }) => inner.fire_cancel(run),
                Delivery::Resync => {
                    inner.notify_workers();
                    recheck_in_flight(&inner).await;
                }
            }
        }
        tracing::debug!("the notifier stream ended; polling carries on alone");
    }))
}

/// Read every run this runtime is stepping and fire the token of those that
/// turned terminal (or vanished), as the per-step cancel watch would at its
/// next poll.
async fn recheck_in_flight(inner: &Inner) {
    let runs: Vec<RunId> = inner
        .in_flight
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .keys()
        .copied()
        .collect();
    for run in runs {
        match inner.store.run_header(run).await {
            Ok(Some(header)) if header.status.is_open() => {}
            Ok(_) => inner.fire_cancel(run),
            Err(e) => tracing::debug!(%run, error = %e, "resync could not read the run"),
        }
    }
}

/// Why a run is cancelled when the run it works for has ended.
pub(crate) const PARENT_ENDED: &str = "the parent run ended";

/// The longest a parked run that works for a parent goes without the runtime looking at the parent:
/// its stored wake is never later than this from the commit. The agent is not stepped for it.
pub(crate) const CHILD_WAIT_POLL: Duration = Duration::from_secs(60);

/// The cancel watch of a step in flight reads the parent at most this often.
const PARENT_CHECK_EVERY: Duration = Duration::from_secs(1);

/// A wake that arrives before its deadline (the clock of the store and the clock of this runtime
/// disagree) parks the run again to the deadline plus this, and does not step the agent.
const DEADLINE_SKEW: Duration = Duration::from_secs(1);

/// Whether the run `parent` is `Failed` (any reason, a cancel included): the runs that work for it
/// have nobody to work for. Only a parent that was **read** and is `Failed` says so. A parent that
/// is `Done` has no working rounds left and a kept-open session stays usable; one that cannot be
/// loaded (the store is down, or retention purged a finished run) is unknown and cancels nothing, which
/// assumes retention is longer than the time a failed parent takes to be seen (a minute).
async fn parent_has_ended(inner: &Inner, parent: RunId) -> bool {
    match inner.store.run_header(parent).await {
        Ok(Some(header)) => header.status == RunStatus::Failed,
        Ok(None) => {
            tracing::debug!(%parent, "the parent run is not in the store; carrying on");
            false
        }
        Err(e) => {
            tracing::debug!(%parent, error = %e, "could not read the parent run; carrying on");
            false
        }
    }
}

/// Fail `run` because its parent ended. Idempotent: a run that is terminal is left as it is.
async fn cancel_for_parent(inner: &Inner, run: RunId) {
    tracing::info!(%run, "the parent run ended; cancelling this run");
    match inner.cancel_run(run, PARENT_ENDED).await {
        Ok(()) | Err(RuntimeError::NotFound(_)) => {}
        Err(e) => {
            tracing::warn!(%run, error = %report(&e), "cancelling a run whose parent ended failed; it is tried again");
        }
    }
}

/// What the failure of a run past its time limit says.
fn time_limit_reason(limit_secs: Option<u64>) -> String {
    match limit_secs {
        Some(secs) if secs >= 60 && secs % 60 == 0 => {
            format!(
                "time limit: the run did not finish within {} min",
                secs / 60
            )
        }
        Some(secs) => format!("time limit: the run did not finish within {secs} s"),
        None => "time limit: the run did not finish in time".to_owned(),
    }
}

/// Fail `run` because it is past its deadline. Idempotent, like a cancel.
async fn cancel_for_deadline(inner: &Inner, run: RunId, limit_secs: Option<u64>) {
    tracing::info!(%run, "the run is past its time limit; cancelling it");
    match inner.cancel_run(run, &time_limit_reason(limit_secs)).await {
        Ok(()) | Err(RuntimeError::NotFound(_)) => {}
        Err(e) => {
            tracing::warn!(%run, error = %report(&e), "cancelling a run past its time limit failed; it is tried again");
        }
    }
}

/// What a worker watches for a run it steps besides the run's own status: the run it works for
/// and its deadline, both only for a child run.
#[derive(Clone, Copy)]
struct Watched {
    parent: Option<RunId>,
    deadline: Option<Timestamp>,
    limit_secs: Option<u64>,
}

/// Fires `token` once the run turns terminal (or vanishes) in the store, which
/// is how a cancel issued by another process reaches a step running here. A child run is also
/// cancelled here when its parent has ended, which costs one more read per poll, and a run
/// with a deadline when the clock passes it (no read).
fn spawn_cancel_watch(
    inner: Arc<Inner>,
    run: RunId,
    watched: Watched,
    token: CancelToken,
) -> AbortOnDrop {
    let every = inner.cfg.cancel_poll_interval.max(Duration::from_millis(1));
    AbortOnDrop(tokio::spawn(async move {
        let mut parent_read: Option<tokio::time::Instant> = None;
        loop {
            tokio::time::sleep(every).await;
            if let Some(deadline) = watched.deadline
                && inner.clock.now() >= deadline
            {
                cancel_for_deadline(&inner, run, watched.limit_secs).await;
                token.cancel();
                return;
            }
            match inner.store.run_header(run).await {
                Ok(Some(header)) if header.status.is_open() => {
                    let due = parent_read.is_none_or(|at| at.elapsed() >= PARENT_CHECK_EVERY);
                    if let Some(parent) = watched.parent
                        && due
                        && {
                            parent_read = Some(tokio::time::Instant::now());
                            parent_has_ended(&inner, parent).await
                        }
                    {
                        cancel_for_parent(&inner, run).await;
                        token.cancel();
                        return;
                    }
                }
                Ok(_) => {
                    tracing::debug!(%run, "run finished elsewhere while stepping; signalling cancellation");
                    token.cancel();
                    return;
                }
                Err(e) => tracing::debug!(%run, error = %e, "cancel watch could not read the run"),
            }
        }
    }))
}

/// One claimed run: step it, commit, release.
#[tracing::instrument(level = "debug", skip_all, fields(run = %lease.run.id, agent = %lease.run.agent))]
async fn advance(inner: Arc<Inner>, lease: Lease, guard: InFlightGuard) {
    let run = lease.run.id;
    // The first claim of a run is news to whoever watches it: the lease now says a worker is on
    // it, so its view reads `claimed` (see `RunView::claimed`) and an A2A task turns `working`,
    // though nothing is committed until the whole turn is. A subscriber re-reads the run on any
    // status event, so this prompts the read; the poll finds it anyway. Later claims (version
    // above 1) follow a commit, and the run was already `working`.
    if lease.run.status == RunStatus::Runnable && lease.run.version <= 1 {
        inner
            .emit_status(
                run,
                &lease.run.agent,
                RunStatus::Runnable,
                Some(CLAIMED.into()),
            )
            .await;
    }
    let renewer = inner
        .cfg
        .lease_renewal
        .then(|| spawn_renewer(inner.clone(), run));
    let cancel = CancelToken::new();
    let tracked = inner.track(run, cancel.clone());
    // Decoded and joined once, here, and handed to `transition`: a v5 run's head and the rows it
    // names are read once, and a v4 run is its whole state.
    let claimed = inner.claimed(&lease.run).await;
    // A run that waited for a person and is stepped again has the rest of its time from now.
    let restored = (lease.run.status == RunStatus::Runnable)
        .then(|| claimed.as_ref().ok().and_then(|c| c.env.remaining_secs))
        .flatten()
        .map(|secs| {
            inner.clock.now() + jiff::SignedDuration::from_secs(secs.try_into().unwrap_or(i64::MAX))
        });
    let watched = Watched {
        parent: lease.run.parent_id,
        deadline: claimed
            .as_ref()
            .ok()
            .and_then(|c| c.env.deadline)
            .or(restored),
        limit_secs: claimed.as_ref().ok().and_then(|c| c.env.limit_secs),
    };
    let watch = spawn_cancel_watch(inner.clone(), run, watched, cancel.clone());
    let release = transition(&inner, lease.run, claimed, cancel).await;
    drop(watch);
    drop(tracked);
    drop(renewer);
    // Release while the run is still in flight here. Nothing claims it until the guard is
    // dropped, so no claim of ours can have taken a new lease that this release, which matches
    // on the worker and not on the claim, would then clear.
    if release && let Err(e) = inner.store.release_lease(run, &inner.cfg.worker_id).await {
        tracing::warn!(%run, error = %e, "releasing the lease failed; it will expire");
    }
    drop(guard);
}

/// What to commit after a transition.
struct Next {
    status: RunStatus,
    wake_at: Option<Timestamp>,
    /// The agent's state to store.
    agent: Value,
    output: Value,
    error: Option<String>,
    attempt: u32,
    /// The retries recorded per key (ADR 0061); `None` after a transition that succeeds.
    retry: Option<RetryRecord>,
    seq: u64,
    /// How many inbox messages this transition consumed (a prefix).
    consumed: usize,
    artifacts: Vec<Artifact>,
    /// Set on a scheduled retry; shown in the status event.
    retry_detail: Option<String>,
    /// The agent asked that a `Done` not end the run past a message that arrived while it stepped
    /// ([`Ctx::reopen_on_arrival`]).
    reopen: bool,
    /// The ids of the messages the agent says the run has read ([`Ctx::read_already`]).
    read: Vec<String>,
    /// `(head, rows)` when the agent splits the state this transition commits; `None` writes the
    /// whole state and no head.
    split: Option<(Value, Vec<Value>)>,
    /// `Some(new)` when this commit starts a body generation (a first split or a compaction);
    /// `None` appends to the current one. Filled in `transition`, after the split is checked.
    generation: Option<Uuid>,
    /// The split was refused (it did not reconstruct the state, or a file's bytes were not stored).
    /// The commit writes the whole state and **no head**, even for a v5 run: a head must never
    /// carry the transcript, so a run whose split cannot be written demotes to v4 and the next step
    /// tries again.
    split_refused: bool,
    /// The merged artifacts the split's upload pass saw, in artifact order: `Head::of` pairs its
    /// file references with **this** list, so a retried envelope cannot shift them.
    head_artifacts: Option<Vec<Artifact>>,
    /// The digests the shadow's upload pass stored (the union the commit references).
    shadow_blobs: Vec<BlobId>,
    /// The digest and size of every file artifact in the head's upload pass, in artifact order, so
    /// `Head::of` does not re-hash the bytes.
    shadow_files: Vec<(BlobId, u64)>,
    /// The deadline and what is left of the time limit to store (set once, when the run is made, or
    /// restored from what was left after a wait for a person: never recomputed from the commit).
    deadline: Option<Timestamp>,
    remaining_secs: Option<u64>,
}

enum Plan {
    Commit(Box<Next>),
    /// Infrastructure trouble: commit nothing and keep the lease, so the run
    /// is retried when it expires instead of being hammered.
    Leave,
}

/// The span of one run's transition ([ANR-1345]): the state the run is in, from a **closed set**
/// ([`RunStatus::as_str`]), and nothing else.
///
/// A run id, a conversation id, an agent's name, a prompt or a tool argument is **never** a field.
/// A collector joins on ids across everything it receives, so an id in a span is a correlation
/// between runs that nothing here needs, and a run's content is not telemetry (pantry
/// `otel-conventions`). A child run's transition is a child span, so a trace shows the tree.
///
/// Nothing is exported yet: this is the substrate the OTel layer of ANR-1350 exports. A process
/// with no subscriber that renders spans is unaffected.
///
/// [ANR-1345]: https://github.com/vymalo/another-adam-rs/issues
fn run_span(status: RunStatus, child: bool) -> tracing::Span {
    if child {
        tracing::info_span!("adam.child", state = status.as_str())
    } else {
        tracing::info_span!("adam.run", state = status.as_str())
    }
}

/// The transition of one run, in its [`run_span`].
async fn transition(
    inner: &Arc<Inner>,
    rec: RunRecord,
    claimed: Result<Claimed, RuntimeError>,
    cancel: CancelToken,
) -> bool {
    let span = run_span(rec.status, rec.parent_id.is_some());
    transition_inner(inner, rec, claimed, cancel)
        .instrument(span)
        .await
}

/// Returns whether the lease should be released.
///
/// `claimed` is [`RunRecord::state`] of `rec`, decoded and joined by [`advance`]: a transition
/// reads the state once, and the failure it carries (an unreadable or unjoinable state) is
/// committed here.
async fn transition_inner(
    inner: &Arc<Inner>,
    rec: RunRecord,
    claimed: Result<Claimed, RuntimeError>,
    cancel: CancelToken,
) -> bool {
    let run = rec.id;
    // A claim may hand back a snapshot read after the lease was taken (the MongoDB store sets the
    // lease, then reads the run), and a cancel does not touch the lease: the run can be terminal
    // already. Its version is the cancel's, so stepping it would commit over the cancel and reopen
    // the run.
    if rec.status.is_terminal() {
        tracing::debug!("the claimed run is finished already; not stepping it");
        return true;
    }
    // A run that works for a parent that has ended does not step: it is cancelled here, before it
    // can do anything (one read of the parent per transition of a child).
    if let Some(parent) = rec.parent_id
        && parent_has_ended(inner, parent).await
    {
        cancel_for_parent(inner, run).await;
        return true;
    }
    let (mut env, base_head, base_rows) = match claimed {
        Ok(claimed) => (claimed.env, claimed.head, claimed.rows),
        // The keys, not the run, are the trouble: a key set that cannot be read now says nothing of
        // the run's state, so the run is left as it is (lease kept, found again when it expires)
        // and never failed for it. A forged head is `Corrupt` and takes the path below.
        Err(e @ RuntimeError::TurnKeys(_)) => {
            tracing::warn!(run = %run, error = %report(&e), "the turn keys could not check the run's head; leaving the run for its next claim");
            return false;
        }
        Err(e) => {
            tracing::error!(run = %run, error = %e, "failing run with unreadable state");
            // The document is unreadable, so it is stored as it is — except a head, which stays
            // the head and keeps the tombstone for its state: it was readable, its body was not.
            // A store that keeps no bodies gets the head as its state and no head.
            let update = match Stored::decode(run, &rec.state) {
                Ok(Stored::Head(_)) if inner.store.keeps_bodies() => {
                    RunUpdate::new(RunStatus::Failed, Value::Null).head(rec.state)
                }
                _ => RunUpdate::new(RunStatus::Failed, rec.state),
            };
            if let Ok(committed) = inner.store.commit_run(run, rec.version, update).await {
                inner
                    .emit_status(run, &rec.agent, RunStatus::Failed, Some(e.to_string()))
                    .await;
                // The report the parent gets is the one `ChildStatus::from_record` would build
                // from a record whose state cannot be decoded.
                let unreadable = format!("the run's state is unreadable: {e}");
                inner
                    .notify_parent(&committed, &Value::Null, Some(&unreadable))
                    .await;
            }
            return true;
        }
    };
    let now = inner.clock.now();
    if let Some(deadline) = env.deadline
        && now >= deadline
    {
        cancel_for_deadline(inner, run, env.limit_secs).await;
        return true;
    }
    // A watched run (it works for a parent or has a deadline) that is parked carries a wake the
    // runtime set, no later than a minute away. When that wake, and not the agent's own, is what
    // claimed it, the parent and the deadline have just been looked at: the run parks again, and the
    // agent is not stepped, nothing is journaled and no model is called.
    if rec.status == RunStatus::Parked
        && env.hb
        && env.inbox.is_empty()
        && !env.agent_wake.is_some_and(|wake| wake <= now)
    {
        // `rec` is not read again on this path.
        park_again(inner, rec, &env, now).await;
        return true;
    }
    // A run that waited for a person gets the rest of its time again from now.
    let (deadline, remaining_secs) = match (env.deadline, env.remaining_secs) {
        (None, Some(secs)) => (
            Some(now + jiff::SignedDuration::from_secs(secs.try_into().unwrap_or(i64::MAX))),
            None,
        ),
        both => both,
    };
    let Some(agent) = inner
        .agents
        .get(&rec.agent)
        .and_then(|r| r.agent().cloned())
    else {
        tracing::error!(run = %run, agent = %rec.agent, "claimed a run of an unregistered agent");
        return true;
    };

    let mut ctx = Ctx::new(CtxParts {
        run,
        agent: rec.agent.clone(),
        conversation_id: rec.conversation_id.clone(),
        attempt: env.attempt,
        retries_left: crate::retry::retries_left(
            env.attempt,
            env.retry.as_ref(),
            &inner.cfg.retry,
            ErrorClass::Transient,
        ),
        seq: env.seq,
        inbox: env.inbox.clone(),
        store: inner.store.clone(),
        sink: inner.sink.clone(),
        clock: inner.clock.clone(),
        cancel,
        runtime: Runtime {
            inner: Arc::clone(inner),
        },
    });
    let stepped = AssertUnwindSafe(agent.step(&mut ctx, std::mem::take(&mut env.agent)))
        .catch_unwind()
        .await;
    let result = stepped.unwrap_or_else(|panic| {
        Err(AgentError::transient(format!(
            "agent panicked: {}",
            panic_message(&panic)
        )))
    });
    let outcome = ctx.into_outcome();

    // The step consumed the agent state. A failure commits the state the run already had, which is
    // the record's own `agent` member: read, and copied, only then. `agent` is required by
    // `Envelope` (it has no serde default), so a record that lacks it never decodes to here.
    let plan = plan(
        inner,
        run,
        &rec.agent,
        &env,
        rec.state.get("agent"),
        base_head.is_some(),
        outcome,
        result,
    );
    let Plan::Commit(mut next) = plan else {
        return false;
    };
    // The shadow's one check: a head must reconstruct the state it is written beside, or the run
    // stays v4. A split that does not join is refused here, before anything is written: the state
    // goes alone, the run keeps its format, and the log says why. The typed state is deserialized
    // once more (the same `S`), never re-parsed from the head.
    if let Some((head, rows)) = &next.split {
        match ErasedAgent::join(agent.as_ref(), head.clone(), rows.clone()) {
            Ok(joined) if joined == next.agent => {}
            Ok(_) => {
                tracing::error!(
                    run = %run,
                    "the split does not reconstruct the committed state; writing the state only \
                     (the run stays v4)"
                );
                next.split = None;
                next.split_refused = true;
            }
            Err(e) => {
                tracing::error!(
                    run = %run,
                    error = %report(&e),
                    "the split's head does not join; writing the state only (the run stays v4)"
                );
                next.split = None;
                next.split_refused = true;
            }
        }
    }
    // Where the rows go: a new generation for the first split or a compaction, an append
    // otherwise. A v5 run's previous transcript is the rows the head named, read with the claim; a
    // v4 run's is in the full state this step read. Neither costs a store read here.
    let generation = match &next.split {
        None => None,
        Some((_, rows)) => {
            let compaction = match &base_head {
                Some(_) => env.rev == 0 || !is_prefix(&base_rows, rows),
                None => {
                    let previous = rec.state.get("agent").cloned();
                    let (previous_rows, unreadable) = match previous {
                        Some(state) => match agent.transcript(state) {
                            Ok(rows) => (rows, false),
                            Err(e) => {
                                // A step that decoded this state decoded the same `A::State`, so
                                // this arm is a guard rather than a path: if it ever fires (a
                                // second state type for the same agent name), the warning says why
                                // and a new generation is the safe answer. It has no test because
                                // no public path reaches it.
                                tracing::warn!(
                                    run = %run,
                                    error = %report(&e),
                                    "the previous state's transcript does not decode; starting a new generation"
                                );
                                (None, true)
                            }
                        },
                        None => (None, false),
                    };
                    unreadable
                        || env.rev == 0
                        || previous_rows
                            .as_ref()
                            .is_some_and(|previous| !is_prefix(previous, rows))
                }
            };
            compaction.then(Uuid::new_v4)
        }
    };
    next.generation = generation;
    // The head names every file artifact of the state, so upload them all and refuse the head if
    // one's bytes are not stored (a pre-E artifact, a failed put): a head never names a digest that
    // is not there. A v5 run's head names its files by digest already, so only what this step adds
    // is uploaded — the bytes of a file it inherited are never read.
    if next.split.is_some() {
        let artifacts = merged_artifacts(&env, &next);
        let uploaded = match &base_head {
            Some(_) => crate::blob::upload_all(&inner.store, &next.artifacts).await,
            None => crate::blob::upload_all(&inner.store, &artifacts).await,
        };
        if uploaded.all_stored {
            // A v5 run's inherited files are named by digest only: one whose bytes are gone (an
            // S2a head that never stored them, or a purged blob) is left out here, before the
            // head is built, because the head cannot store it again and refusing would retry for
            // ever. The files that are there, the step's own included, stay.
            let inherited = base_head
                .as_ref()
                .map(HeadParts::file_refs)
                .unwrap_or_default();
            let mut inherited_ids = Vec::with_capacity(inherited.len());
            for (digest, _) in &inherited {
                inherited_ids.push(digest.clone());
            }
            let missing = match crate::blob::missing_blobs(inner.store.clone(), inherited_ids).await
            {
                Ok(missing) => missing,
                Err(e) => {
                    tracing::error!(
                        run = %run,
                        error = %report(&e),
                        "checking an inherited file's bytes failed; the run is left as it is and retried"
                    );
                    return false;
                }
            };
            let artifacts = if missing.is_empty() {
                artifacts
            } else {
                // The inherited artifacts are the first `env.artifacts.len()` entries of the
                // merged list (a file is appended, never replaced), and their file artifacts are
                // `inherited` in order: pair them by position, since a carried `Artifact` has no
                // digest, only empty bytes.
                let inherited_len = env.artifacts.len();
                let mut file_at = 0_usize;
                let mut kept = Vec::with_capacity(artifacts.len());
                for (at, artifact) in artifacts.into_iter().enumerate() {
                    if at < inherited_len && artifact.file.is_some() {
                        let gone = inherited
                            .get(file_at)
                            .is_some_and(|(digest, _)| missing.contains(digest));
                        file_at += 1;
                        if gone {
                            continue;
                        }
                    }
                    kept.push(artifact);
                }
                kept
            };
            // The head pairs its file references with this pass, so carry the list the pass saw:
            // a retry merges deliveries into the envelope, never artifacts. A v5 run's inherited
            // files are paired first, in artifact order, and the pass covers the rest.
            next.head_artifacts = Some(artifacts);
            let mut shadow_files = Vec::with_capacity(inherited.len() + uploaded.files.len());
            for (digest, size) in inherited {
                if !missing.contains(&digest) {
                    shadow_files.push((digest, size));
                }
            }
            shadow_files.extend(uploaded.files);
            next.shadow_files = shadow_files;
            next.shadow_blobs = uploaded.ids;
        } else {
            tracing::error!(
                run = %run,
                "a file artifact's bytes are not stored; writing the state only (the run stays v4)"
            );
            next.split = None;
            next.split_refused = true;
        }
    }
    // A demotion writes the whole state, and a v4 state must carry a file's **bytes**: the head the
    // run read names them by digest and size only, so re-read them. A missing blob refuses the
    // demotion (the run is left as it is and retried) rather than writing zero-byte files.
    if next.split_refused
        && let Some(base) = &base_head
    {
        let mut artifacts = merged_artifacts(&env, &next);
        match inline_inherited_files(&inner.store, base, &mut artifacts).await {
            Ok(true) => next.head_artifacts = Some(artifacts),
            Ok(false) => {
                tracing::error!(
                    run = %run,
                    "an inherited file's bytes are not stored; the run is left as it is and retried"
                );
                return false;
            }
            Err(e) => {
                tracing::error!(
                    run = %run,
                    error = %report(&e),
                    "reading an inherited file failed; the run is left as it is and retried"
                );
                return false;
            }
        }
    }
    // One writer per step attempt: the commit's CAS retries reuse it, so a retry of the same
    // attempt is idempotent and a lost CAS deletes exactly the rows this attempt inserted.
    let body_writer = Uuid::new_v4();
    next.deadline = deadline;
    next.remaining_secs = remaining_secs;
    // Parked with no timer: the run waits for a person (or a message), whose time is not the run's.
    if next.status == RunStatus::Parked
        && next.wake_at.is_none()
        && let Some(deadline) = next.deadline.take()
    {
        let left = (deadline - inner.clock.now()).as_secs().max(0);
        next.remaining_secs = Some(u64::try_from(left).unwrap_or(0));
    }
    match commit(
        inner,
        &rec,
        &env,
        base_head.as_ref(),
        &next,
        rec.parent_id.is_some(),
        body_writer,
    )
    .await
    {
        Ok(Some(committed)) => {
            announce(inner, &committed, &next).await;
            observe_commit(inner, &committed, &next).await;
            // After the commit, never before: the parent's timer is the fallback for a crash here.
            // The report is built from what was just written: the store does not echo the state.
            inner
                .notify_parent(&committed, &next.output, next.error.as_deref())
                .await;
            // A stepped park is the parent's business too: it reads what the child waits on and
            // shows it, and a message or a resume to the parent goes down to the child (ADR 0067
            // decision 5). A commit a message turned into a wake-up parked nothing, so it says
            // nothing; `park_again`, the runtime's own look, never comes through here.
            inner.notify_parent_parked(&committed).await;
            true
        }
        Ok(None) => {
            tracing::info!(
                run = %run,
                "commit rejected (run advanced or cancelled elsewhere); dropping result"
            );
            true
        }
        Err(e) => {
            tracing::error!(run = %run, error = %report(&e), "commit failed; the run will be retried when its lease expires");
            false
        }
    }
}

/// `message`, then its cause when it has one: the run's failure text is a boundary, so the chain
/// is flattened here.
fn with_cause(
    message: String,
    source: Option<&(dyn std::error::Error + Send + Sync + 'static)>,
) -> String {
    match source {
        Some(cause) => format!("{message}: {}", report(cause)),
        None => message,
    }
}

fn panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_owned())
}

/// Decide what the transition's result means for the run.
///
/// `base_agent` is the agent state the run had before the step: the step took `base.agent` by
/// move, so the paths that commit the state untouched read it from the record instead.
#[allow(clippy::too_many_arguments)] // one transition, described by its parts
fn plan(
    inner: &Inner,
    run: RunId,
    agent: &str,
    base: &Envelope,
    base_agent: Option<&Value>,
    v5: bool,
    out: CtxOutcome,
    result: Result<Stepped, AgentError>,
) -> Plan {
    let CtxOutcome {
        seq,
        consumed,
        reopen,
        read,
        artifacts,
    } = out;
    // Starting point for a transition that made progress. A v5 run's step must split: its state is
    // the head plus the body, and a step that writes no head would put the whole state in the head,
    // which the next join discards. Such a step is a **refused split**: the run demotes to the
    // whole state (a v4 run, whose `split` is `None`, is unaffected).
    let progressed = |status, wake_at, agent, output, error, split: Option<(Value, Vec<Value>)>| {
        let split_refused = v5 && split.is_none();
        Next {
            status,
            wake_at,
            agent,
            output,
            error,
            attempt: 0,
            retry: None,
            seq,
            consumed,
            artifacts: artifacts.clone(),
            retry_detail: None,
            reopen,
            read: read.clone(),
            split,
            generation: None,
            split_refused,
            head_artifacts: None,
            shadow_blobs: Vec::new(),
            shadow_files: Vec::new(),
            deadline: base.deadline,
            remaining_secs: base.remaining_secs,
        }
    };
    // Starting point for a failure that ends the run: state untouched.
    let failed = |error: String| Next {
        status: RunStatus::Failed,
        wake_at: None,
        agent: base_agent.cloned().unwrap_or(Value::Null),
        output: Value::Null,
        error: Some(error),
        attempt: base.attempt,
        retry: base.retry.clone(),
        seq,
        consumed: 0,
        artifacts: artifacts.clone(),
        retry_detail: None,
        reopen: false,
        read: Vec::new(),
        split: None,
        generation: None,
        split_refused: false,
        head_artifacts: None,
        shadow_blobs: Vec::new(),
        shadow_files: Vec::new(),
        deadline: base.deadline,
        remaining_secs: base.remaining_secs,
    };

    // A transient failure: schedule the retry, or give up when the key's attempts or the run's
    // ceiling are used up (ADR 0061). `hint` is a minimum wait from the error; `step` the step's
    // own policy, else the runtime's counts under `runtime`.
    let retrying = |msg: String,
                    hint: Option<Duration>,
                    class: ErrorClass,
                    step: Option<&StepRetry>|
     -> Next {
        let (key, policy) = match step {
            Some(step) => (step.key.as_str(), &step.policy),
            None => (RUNTIME_RETRY_KEY, &inner.cfg.retry),
        };
        // The one decision `Ctx::retries_left` reads too (`retry::decide`).
        let Decision {
            record,
            attempt,
            failures,
            policy,
            verdict,
        } = decide(base.retry.as_ref(), base.attempt, key, policy, class);
        let of_key = if key == RUNTIME_RETRY_KEY {
            String::new()
        } else {
            format!(" of `{key}`")
        };
        let gave_up = |error: String| Next {
            attempt,
            retry: Some(record.clone()),
            ..failed(error)
        };
        match verdict {
            Verdict::NotRetried => gave_up(format!(
                "not retried: the policy{of_key} does not retry {}: {msg}",
                class_name(class)
            )),
            Verdict::AttemptsSpent => {
                gave_up(format!("gave up after {failures} attempts{of_key}: {msg}"))
            }
            Verdict::Ceiling => gave_up(format!(
                "gave up after {attempt} consecutive failed transitions: {msg}"
            )),
            Verdict::Retry => {
                let delay = match hint {
                    Some(at_least) => policy.delay_with_hint(failures, at_least),
                    None => policy.backoff(failures),
                };
                let wake_at = inner.clock.now() + adam_core::time::span_of(delay);
                Next {
                    status: RunStatus::Runnable,
                    wake_at: Some(wake_at),
                    agent: base_agent.cloned().unwrap_or(Value::Null),
                    output: Value::Null,
                    error: None,
                    attempt,
                    retry: Some(record.clone()),
                    // Abandon this try's journal entries so the retry runs
                    // its steps afresh (see `Ctx::step`).
                    seq,
                    consumed: 0,
                    artifacts: Vec::new(),
                    retry_detail: Some(format!(
                        "attempt {failures} of {}{of_key} failed, retrying in {delay:?}: {msg}",
                        policy.max_attempts
                    )),
                    reopen: false,
                    read: Vec::new(),
                    split: None,
                    generation: None,
                    split_refused: false,
                    head_artifacts: None,
                    shadow_blobs: Vec::new(),
                    shadow_files: Vec::new(),
                    deadline: base.deadline,
                    remaining_secs: base.remaining_secs,
                }
            }
        }
    };

    let stepped = result.is_ok();
    let next = match result {
        Ok(Stepped {
            transition: Transition::Continue(s),
            split,
        }) => progressed(RunStatus::Runnable, None, s, Value::Null, None, split),
        Ok(Stepped {
            transition: Transition::Park { state, wake_at },
            split,
        }) => progressed(RunStatus::Parked, wake_at, state, Value::Null, None, split),
        Ok(Stepped {
            transition: Transition::Done { state, output },
            split,
        }) => progressed(RunStatus::Done, None, state, output, None, split),
        Ok(Stepped {
            transition: Transition::Fail { state, error },
            split,
        }) => progressed(
            RunStatus::Failed,
            None,
            state,
            Value::Null,
            Some(error),
            split,
        ),
        // A retry hint from the peer only lengthens the policy's backoff.
        Err(AgentError::Transient {
            message,
            retry_after,
            class,
            retry,
            source,
        }) => {
            // As `AgentError::class` reads it: a plain transient error with a hint is a rate limit.
            let class = match (class, retry_after) {
                (ErrorClass::Transient, Some(_)) => ErrorClass::RateLimited,
                (class, _) => class,
            };
            retrying(
                with_cause(message, source.as_deref()),
                retry_after,
                class,
                retry.as_deref(),
            )
        }
        Err(AgentError::Permanent { message, source }) => {
            failed(with_cause(message, source.as_deref()))
        }
        // A policy refused the input (ADR 0050, slice H1b): the run fails with the reason a person
        // reads, and nothing retries it.
        Err(AgentError::Rejected { message }) => failed(message),
        Err(AgentError::NonDeterminism { message, source }) => failed(format!(
            "non-deterministic replay: {}",
            with_cause(message, source.as_deref())
        )),
        Err(AgentError::Store(e)) => match e.class() {
            ErrorClass::Corrupt if matches!(e, StoreError::NonDeterminism { .. }) => {
                failed(format!("non-deterministic replay: {e}"))
            }
            // Data that can never be read, or a request the store can never accept: retrying
            // would loop for ever, so the run ends.
            ErrorClass::Corrupt | ErrorClass::Invalid => {
                tracing::error!(
                    run = %run,
                    error = %report(&e),
                    "the store cannot read or accept this run's data; failing the run"
                );
                failed(report(&e))
            }
            // Anything else (an unreachable store, a bug): leave the run to its lease.
            class => {
                tracing::error!(
                    run = %run,
                    error = %report(&e),
                    class = ?class,
                    alert = class.should_alert(),
                    "store error while stepping; leaving the run leased"
                );
                return Plan::Leave;
            }
        },
    };
    // A runtime with keys needs an agent that signs its turns: a step whose rows hold an unsigned
    // assistant or tool row ends the run here, with its state untouched, before anything is
    // signed or written (ADR 0068). Retrying would call the model again for the same refusal.
    if stepped && inner.sealer.is_some() {
        let transcript;
        let rows: &[Value] = match &next.split {
            Some((_, rows)) => rows,
            None => {
                transcript = inner
                    .transcript_of(run, agent, &next.agent)
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                &transcript
            }
        };
        if let Err(e) = Sealer::require_signed_turns(run, rows) {
            tracing::error!(run = %run, error = %report(&e), "the agent did not sign its turns under a runtime with turn keys; failing the run");
            return Plan::Commit(Box::new(failed(e.to_string())));
        }
    }
    Plan::Commit(Box::new(next))
}

fn span_of(d: Duration) -> jiff::SignedDuration {
    adam_core::time::span_of(d)
}

/// Park a watched run again without stepping it: its parent and deadline were just looked at. The
/// state is stored as it is; only the wake moves, to the earliest of the agent's own wake, the
/// deadline (plus a second when this wake came early) and the next look at the parent. A message that
/// lands meanwhile moves the version and this commit is dropped: the run is runnable and is claimed.
///
/// Takes the record by value: its state is moved into the update, not copied. A v5 run's head is
/// written back beside the tombstone, so the head stays current and the next step appends to the
/// same generation.
async fn park_again(inner: &Inner, rec: RunRecord, env: &Envelope, now: Timestamp) {
    let mut wake = now + span_of(CHILD_WAIT_POLL);
    if let Some(agent) = env.agent_wake {
        wake = wake.min(agent);
    }
    if let Some(deadline) = env.deadline {
        wake = wake.min(deadline + span_of(DEADLINE_SKEW));
    }
    let keeps_bodies = inner.store.keeps_bodies();
    let mut update = match Stored::decode(rec.id, &rec.state) {
        Ok(Stored::Head(head)) => match head.encode() {
            Ok(head) if keeps_bodies => RunUpdate::new(RunStatus::Parked, Value::Null).head(head),
            Ok(head) => RunUpdate::new(RunStatus::Parked, head),
            Err(e) => {
                tracing::warn!(run = %rec.id, error = %report(&e), "the head does not encode; parking a watched run again is skipped");
                return;
            }
        },
        _ => RunUpdate::new(RunStatus::Parked, rec.state),
    };
    update.wake_at = Some(wake);
    match inner.store.commit_run(rec.id, rec.version, update).await {
        Ok(_) | Err(StoreError::Conflict { .. } | StoreError::NotFound(_)) => {}
        Err(e) => {
            tracing::warn!(run = %rec.id, error = %report(&e), "parking a watched run again failed; it is looked at again when its lease expires");
        }
    }
}

/// The record to write, built on the freshest envelope.
///
/// `cur` is the claim's envelope on the first attempt, or a freshly decoded retry. A stepped
/// envelope is drained (`cur.agent` is `Null`), so nothing here may read that member. `base_head`
/// is the v5 head the same record carried, if any: a commit that does not re-split writes it back
/// (with this commit's members) and keeps its file references.
#[allow(clippy::too_many_arguments)] // one commit, described by its parts
fn build_update(
    next: &Next,
    cur: &Envelope,
    base_head: Option<&HeadParts>,
    carried: Option<&[Artifact]>,
    base_inbox_len: usize,
    base_rev: u64,
    parent: bool,
    now: Timestamp,
    body_writer: Uuid,
    keeps_bodies: bool,
) -> StoreResult<RunUpdate> {
    // Messages that arrived while stepping sit after the ones we started
    // with. A parked agent must not sleep through them.
    let arrived = cur.inbox.len() > base_inbox_len;
    let reopened = reopens(next, cur, base_inbox_len);
    let (status, wake_at) = if reopened || (next.status == RunStatus::Parked && arrived) {
        (RunStatus::Runnable, None)
    } else {
        (next.status, next.wake_at)
    };
    // A run that works for a parent or has a deadline wakes at the deadline and within the poll at
    // the latest, so that a parked one, which may wait for a message that never comes, is looked at
    // (its parent, its deadline) and not left for ever. The agent's own wake is kept in the envelope.
    let agent_wake = (status == RunStatus::Parked).then_some(wake_at).flatten();
    let heartbeat = status == RunStatus::Parked && (parent || next.deadline.is_some());
    let wake_at = match status {
        RunStatus::Parked if heartbeat => {
            let mut wake = wake_at.map_or(now + span_of(CHILD_WAIT_POLL), |w| {
                w.min(now + span_of(CHILD_WAIT_POLL))
            });
            if let Some(deadline) = next.deadline {
                wake = wake.min(deadline);
            }
            Some(wake)
        }
        RunStatus::Runnable => match (wake_at, next.deadline) {
            (Some(w), Some(deadline)) => Some(w.min(deadline)),
            (w, _) => w,
        },
        _ => wake_at,
    };
    // A commit that ran an upload pass (a split, or a demotion that inlined the inherited files)
    // carries its list; otherwise the artifacts are the run's plus this transition's.
    let artifacts = match &next.head_artifacts {
        Some(artifacts) => artifacts.clone(),
        None => merged_artifacts(cur, next),
    };
    // `next` and `cur` are borrowed for the whole retry loop, which builds this update again from
    // them after a lost CAS race, so the envelope owns copies of what it keeps. Moving them would
    // leave a retry with `null`.
    let env = Envelope {
        v: cur.v,
        agent: next.agent.clone(),
        inbox: cur.inbox.iter().skip(next.consumed).cloned().collect(),
        seq: next.seq,
        attempt: next.attempt,
        retry: next.retry.clone(),
        rev: base_rev + 1,
        output: if reopened {
            Value::Null
        } else {
            next.output.clone()
        },
        error: next.error.clone(),
        artifacts,
        deadline: next.deadline,
        limit_secs: cur.limit_secs,
        remaining_secs: next.remaining_secs,
        agent_wake,
        hb: heartbeat,
        signature: None,
    };
    let mut update = RunUpdate::new(status, Value::Null);
    update.wake_at = wake_at;
    // `keeps_bodies` is the gate: a store that keeps none is never handed a head or a row — the
    // whole state is written instead, and the run stays v4 (ADR 0055).
    match (&next.split, base_head) {
        // A step that splits: the head, the rows it names, and the tombstone instead of the whole
        // state (ADR 0055, the contract release).
        (Some((head, rows)), _) if keeps_bodies => {
            let len = u32::try_from(rows.len())
                .map_err(|_| StoreError::InvalidInput("a transcript is longer than u32".into()))?;
            let head = Head::of(
                &env,
                head.clone(),
                len,
                next.head_artifacts.as_deref().unwrap_or(&env.artifacts),
                &next.shadow_files,
            );
            update.head = Some(head.encode()?);
            let rows = rows
                .iter()
                .enumerate()
                .map(|(idx, message)| {
                    let idx = u32::try_from(idx).map_err(|_| {
                        StoreError::InvalidInput("a transcript has more rows than u32::MAX".into())
                    })?;
                    Ok(BodyRow {
                        idx,
                        message: message.clone(),
                    })
                })
                .collect::<StoreResult<Vec<_>>>()?;
            update.body = Some(BodyWrite {
                generation: next.generation,
                rows,
                writer: body_writer,
            });
            update.state = Value::Null;
        }
        // A v5 run whose transition did not split (a failure, a retry): the head it read, with
        // this commit's members, and the transcript set unchanged. The state stays the tombstone.
        // A refused split is the exception: the state is the whole new state, and a head carrying
        // it would put the transcript back in the head, so the run demotes to v4 instead.
        (None, Some(_)) if next.split_refused => update.state = env.encode()?,
        (None, Some(base)) if keeps_bodies => {
            update.head = Some(
                base.carried(&env, carried.unwrap_or(&next.artifacts))
                    .encode()?,
            );
            update.state = Value::Null;
        }
        // A v5 document on a store that keeps no bodies (a third-party store that persists a head
        // but has not opted in): the head stays the state, and no head or row is written.
        (None, Some(base)) => {
            update.state = base
                .carried(&env, carried.unwrap_or(&next.artifacts))
                .encode()?;
        }
        // A v4 run that did not split, or a split for a store that keeps no bodies: the whole
        // state, as it always was.
        (_, None) | (Some(_), _) => update.state = env.encode()?,
    }
    Ok(update)
}

/// Sign the document `update` writes as the run's state ([ADR 0068] decision 4): the v5 head, or
/// the v4 state of a run that is not split. With no turn keys the document is left as built, which
/// carries no signature.
///
/// A head whose transcript this commit writes is chained over the rows it writes (`next.split`).
/// One that carries its rows is signed again with the `{len, chain}` it read, and a head that had
/// none (an unsigned run, adopted by decision 11) stays unsigned until a commit that writes rows. A
/// v4 state is chained over the agent's own transcript, the one a reader takes from it.
///
/// A key that cannot sign is the commit's failure, with the keys' class: the run keeps its lease
/// and is tried again, and nothing is written unsigned by a runtime that has keys.
///
/// [ADR 0068]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0068-adam-signs-its-own-history.md
async fn seal_update(
    inner: &Inner,
    rec: &RunRecord,
    next: &Next,
    base: Option<&HeadParts>,
    update: &mut RunUpdate,
) -> StoreResult<()> {
    let Some(sealer) = &inner.sealer else {
        return Ok(());
    };
    let refuse = refuse_commit;
    let conversation = rec.conversation_id.as_deref();
    let doc = match update.head.as_mut() {
        Some(head) => head,
        None if update.state.is_object() => &mut update.state,
        None => return Ok(()),
    };
    let is_head = doc.get("v").and_then(Value::as_u64) == Some(u64::from(crate::head::VERSION));
    let (len, chain, seal) = if is_head {
        match &next.split {
            Some((_, rows)) if !next.split_refused => {
                // Before anything is signed: never a head the next decode would refuse.
                Sealer::require_signed_turns(rec.id, rows).map_err(refuse)?;
                let (seal, chain) = Seal::over(rec.id, conversation, rows).map_err(refuse)?;
                (seal.len, chain, Some(seal))
            }
            _ => {
                let seal = base.and_then(|base| base.seal.clone());
                match seal.as_ref().and_then(Seal::parts) {
                    Some((len, chain)) => (len, chain, seal),
                    // A signed head with no usable seal cannot be signed again without its rows,
                    // and is not signed over: it is the commit's failure, as `reseal`'s strict
                    // path is a delivery's.
                    None if rec.state.get("signature").is_some() => {
                        return Err(refuse(unusable_seal(rec.id)));
                    }
                    // An unsigned head (adopted, decision 11) stays unsigned.
                    None => return Ok(()),
                }
            }
        }
    } else {
        let rows = inner
            .transcript_of(rec.id, &rec.agent, &next.agent)
            .map_err(refuse)?
            .ok_or_else(|| RuntimeError::Corrupt {
                run: rec.id,
                reason: "the agent that stepped the run is not registered".into(),
                source: None,
            })
            .map_err(refuse)?;
        if let Err(e) = Sealer::require_signed_turns(rec.id, &rows) {
            // A run that ends over a history that was never signed (an adopted run, decision 11)
            // is written as it was read, unsigned, so it can still be failed. Anything else
            // would be signed over unsigned turns: refused.
            if next.status == RunStatus::Failed && rec.state.get("signature").is_none() {
                return Ok(());
            }
            return Err(refuse(e));
        }
        let (len, chain) = chain_over(rec.id, conversation, &rows).map_err(refuse)?;
        (len, chain, None)
    };
    let signature = sealer
        .sign(rec.id, conversation, len, &chain, doc)
        .await
        .map_err(|e| refuse(e.into()))?;
    attach(doc, signature, seal.as_ref());
    Ok(())
}

/// The error of a head that is signed and carries no usable `seal`.
fn unusable_seal(run: RunId) -> RuntimeError {
    RuntimeError::Corrupt {
        run,
        reason: "the run's head is refused (ADR 0068): it is signed and carries no usable seal"
            .into(),
        source: None,
    }
}

/// A runtime error of the signing path as the commit's: the keys' own class for a key that
/// cannot be reached or refuses, `Corrupt` for a document that does not verify, and invalid input
/// otherwise.
fn refuse_commit(e: RuntimeError) -> StoreError {
    match e {
        RuntimeError::TurnKeys(e) => StoreError::Backend {
            class: e.class(),
            source: Box::new(e),
        },
        RuntimeError::Corrupt { .. } => StoreError::Corrupt(e.to_string()),
        other => StoreError::InvalidInput(other.to_string()),
    }
}

/// Check the record a commit reloaded after losing its CAS to a delivery, before the retry signs
/// over what it takes from it (the inbox, the artifacts): a signature is never put over a
/// document that did not verify. A signed head is checked against its own `seal`, a signed v4
/// state against the agent's transcript; an unsigned one is left unsigned, as a first commit would
/// find it.
async fn verify_reloaded(
    inner: &Inner,
    rec: &RunRecord,
    env: &Envelope,
    head: Option<&HeadParts>,
) -> StoreResult<()> {
    let Some(sealer) = &inner.sealer else {
        return Ok(());
    };
    if rec.state.get("signature").is_none() {
        return Ok(());
    }
    let conversation = rec.conversation_id.as_deref();
    let checked = match head {
        Some(parts) => {
            sealer
                .check_sealed(rec.id, conversation, parts.seal.as_ref(), &rec.state)
                .await
        }
        None => match inner.transcript_of(rec.id, &rec.agent, &env.agent) {
            Ok(Some(rows)) => {
                sealer
                    .check_read(rec.id, conversation, &rows, &rec.state)
                    .await
            }
            Ok(None) => Ok(()),
            Err(e) => Err(e),
        },
    };
    checked.map_err(refuse_commit)
}

/// Inline the bytes of every file a v5 head names into `artifacts`, so a demoted v4 state carries
/// them: a head names a file by digest and size, and a v4 state must carry the bytes.
///
/// The inherited files come first, in the head's artifact order (a step's own files are appended
/// after them), so the references pair positionally. `false` when a blob is missing or its bytes
/// are not the digest the head names: the demotion cannot be written, and the run is left as it is
/// (it is retried when its lease expires).
async fn inline_inherited_files(
    store: &DynStore,
    base: &HeadParts,
    artifacts: &mut [Artifact],
) -> StoreResult<bool> {
    let mut refs = base.file_refs().into_iter();
    for artifact in artifacts.iter_mut() {
        let Some(file) = artifact.file.as_mut() else {
            continue;
        };
        let Some((digest, _)) = refs.next() else {
            // The rest are this step's own files, whose bytes it already carries.
            break;
        };
        match store.get_blob(&digest).await? {
            Some(bytes) if blob_id(&bytes) == digest => file.bytes = bytes,
            _ => return Ok(false),
        }
    }
    // The pairing is exact: every file the head names has a carried artifact, in order.
    debug_assert!(refs.next().is_none(), "a head file has no carried artifact");
    Ok(true)
}

/// The artifacts of the state this commit writes: the ones the run already had, plus this
/// transition's, merged by [`add_artifact`].
fn merged_artifacts(cur: &Envelope, next: &Next) -> Vec<Artifact> {
    let mut artifacts = cur.artifacts.clone();
    for artifact in &next.artifacts {
        add_artifact(&mut artifacts, artifact);
    }
    artifacts
}

/// Whether `previous` is a prefix of `rows`: an append (the store writes only the rows after the
/// head's `len`), as opposed to a compaction (a new generation with the whole transcript).
fn is_prefix(previous: &[Value], rows: &[Value]) -> bool {
    previous.len() <= rows.len() && rows[..previous.len()] == previous[..]
}

/// `artifact` added to the run's `artifacts`: after the others, unless it has an id
/// ([`Artifact::id`]) that one of them has, which it replaces where it stands (so a position is
/// kept). A file artifact is never replaced and never replaces anything: files are content-addressed
/// and have no id, so an id on one is dropped and both are kept. Journaled artifacts with no id never
/// match, so a run written before ids existed reads and grows as it did.
fn add_artifact(artifacts: &mut Vec<Artifact>, artifact: &Artifact) {
    let mut artifact = artifact.clone();
    if artifact.file.is_some() {
        artifact.id = None;
    }
    if let Some(id) = &artifact.id
        && let Some(held) = artifacts
            .iter_mut()
            .find(|held| held.file.is_none() && held.id.as_deref() == Some(id.as_str()))
    {
        *held = artifact;
        return;
    }
    artifacts.push(artifact);
}

/// Whether a `Done` is committed as a step that goes on: the agent asked for it
/// ([`Ctx::reopen_on_arrival`]) and a message other than a child's notice, or a copy of one the run
/// has read ([`Ctx::read_already`]), arrived while it stepped.
fn reopens(next: &Next, cur: &Envelope, base_inbox_len: usize) -> bool {
    next.reopen
        && next.status == RunStatus::Done
        && news_since(&cur.inbox, base_inbox_len, &next.read) > 0
}

/// Commit with CAS. A conflict caused only by messages delivered while we
/// were stepping is merged (the messages are kept); any other conflict means
/// someone else advanced or cancelled the run, and the result is dropped
/// (`Ok(None)`).
async fn commit(
    inner: &Inner,
    claimed: &RunRecord,
    base: &Envelope,
    base_head: Option<&HeadParts>,
    next: &Next,
    parent: bool,
    body_writer: Uuid,
) -> StoreResult<Option<RunRecord>> {
    let run = claimed.id;
    let keeps_bodies = inner.store.keeps_bodies();
    // On the first attempt the update is built from the borrowed record and envelope: nothing is
    // copied. A retry owns the record it just loaded, and builds from that.
    let mut retried: Option<(RunRecord, Envelope, Option<HeadParts>)> = None;
    for attempt in 0..MAX_COMMIT_RETRIES {
        if attempt > 0 {
            let Some(rec) = inner.store.load_run(run).await? else {
                return Ok(None);
            };
            let (env, head) = match Stored::decode(run, &rec.state) {
                Ok(Stored::Full(env)) => (env, None),
                Ok(Stored::Head(head)) => {
                    let (env, parts) = head.into_parts(Value::Null);
                    (env, Some(parts))
                }
                Err(_) => return Ok(None),
            };
            // `deliver` never bumps `rev` and never changes a runnable run's
            // status; everything else that commits does.
            if rec.status != RunStatus::Runnable
                || env.rev != base.rev
                || env.inbox.len() < base.inbox.len()
            {
                return Ok(None);
            }
            verify_reloaded(inner, &rec, &env, head.as_ref()).await?;
            retried = Some((rec, env, head));
        }
        let (cur_rec, cur_env, cur_head) = match &retried {
            Some((rec, env, head)) => (rec, env, head.as_ref()),
            None => (claimed, base, base_head),
        };
        // A carried head must not keep naming an inherited file whose bytes are gone (an S2a
        // expand-release head can; its full state carried the bytes). The head is built below, so
        // check here; a store error leaves the run for its next claim. A demotion is not a carried
        // head: it writes the whole state, and `inline_inherited_files` already refused if an
        // inherited file's bytes were missing.
        let pruned;
        let cur_head = match cur_head {
            Some(base) if next.split.is_none() && !next.split_refused => {
                pruned = base.without_missing_files(inner.store.clone()).await?;
                Some(&pruned)
            }
            other => other,
        };
        // The bytes of every file artifact this commit names go to the store before the commit
        // that names them (ADR 0055). A split's own pass (`upload_all`, in `transition`) already
        // stored what was missing and refused the head if one was not. A commit that **carries a
        // head** gates its files the same way: the head names only the digests whose bytes are
        // stored, and a file whose put fails is left out of the head and its references (the state
        // is the tombstone, so the head is the run's only copy). A v4 commit keeps the fail-open
        // upload: its state carries the bytes.
        let (mut blobs, carried) = if next.split.is_some() {
            (next.shadow_blobs.clone(), None)
        } else if cur_head.is_some() {
            let carried = crate::blob::upload_carried(&inner.store, &next.artifacts).await;
            (carried.ids.clone(), Some(carried.artifacts))
        } else {
            let mut blobs = Vec::new();
            if cur_env.rev == 0 {
                blobs = crate::blob::upload_files(&inner.store, &cur_env.artifacts).await;
            }
            blobs.extend(crate::blob::upload_files(&inner.store, &next.artifacts).await);
            (blobs, None)
        };
        blobs.sort();
        blobs.dedup();
        let mut update = build_update(
            next,
            cur_env,
            cur_head,
            carried.as_deref(),
            base.inbox.len(),
            base.rev,
            parent,
            inner.clock.now(),
            body_writer,
            keeps_bodies,
        )?;
        seal_update(inner, cur_rec, next, cur_head, &mut update).await?;
        update.blobs = blobs;
        match inner.store.commit_run(run, cur_rec.version, update).await {
            Ok(rec) => {
                // ANR-1449: one committed transition, under the state it committed. The meter is the
                // process's, which is a no-op when `adam_otel::init` found no endpoint (ADR 0075).
                crate::metrics::record_run_status(
                    &opentelemetry::global::meter(crate::metrics::SCOPE),
                    rec.status,
                );
                if reopens(next, cur_env, base.inbox.len()) {
                    tracing::info!(%run, "a message arrived while the run finished: the run goes on instead");
                }
                return Ok(Some(rec));
            }
            Err(StoreError::Conflict { .. }) => {}
            Err(StoreError::NotFound(_)) => return Ok(None),
            Err(e) => return Err(e),
        }
    }
    tracing::warn!(%run, "gave up merging a commit after repeated conflicts");
    Ok(None)
}

/// Offers the events that follow a commit to the observers (ADR 0050, H4): one `run.parked`,
/// `run.ended` or `run.error` for the transition's outcome, then one `artifact.kept` per artifact
/// it kept. Keys are `<run>/commit:<version>/<event>/<index>`: a version is committed once, so a key
/// is never reused. Best effort and after the commit: nothing here changes the run.
async fn observe_commit(inner: &Inner, committed: &RunRecord, next: &Next) {
    use adam_plugin::{EventHeader, EventKind, HookEvent};
    let Some(set) = inner.observers.clone() else {
        return;
    };
    let kind = match (committed.status, next.status) {
        (RunStatus::Parked, _) => Some(EventKind::RunParked),
        (RunStatus::Done | RunStatus::Failed, _) => Some(EventKind::RunEnded),
        (RunStatus::Runnable, _) if next.retry_detail.is_some() => Some(EventKind::RunError),
        _ => None,
    };
    if kind.is_none() && next.artifacts.is_empty() {
        return;
    }
    let mut root = committed.id;
    let mut parent = committed.parent_id;
    for _ in 0..16 {
        let Some(p) = parent else { break };
        root = p;
        parent = match inner.store.run_header(p).await {
            Ok(Some(h)) => h.parent_id,
            _ => None,
        };
    }
    let at = inner.clock.now().to_string();
    let header = |name: &str, index: usize| EventHeader {
        key: format!(
            "{}/commit:{}/{name}/{index}",
            committed.id, committed.version
        ),
        run: committed.id.to_string(),
        root_run: root.to_string(),
        agent: committed.agent.clone(),
        at: at.clone(),
    };
    let keep = |text: &str| text.to_owned();
    let mut events = Vec::new();
    if let Some(kind) = kind {
        let mut event = HookEvent::new(kind, header(kind.name(), 0))
            .plain("version", committed.version)
            .plain("status", format!("{:?}", committed.status).to_lowercase());
        if let Some(detail) = next.retry_detail.as_deref() {
            event = event
                .text("detail", detail, &keep)
                .plain("attempt", next.attempt);
        }
        if let Some(error) = next.error.as_deref() {
            event = event.text("error", error, &keep);
        }
        events.push(event.bounded());
    }
    for (i, artifact) in next.artifacts.iter().enumerate() {
        let mut event = HookEvent::new(EventKind::ArtifactKept, header("artifact.kept", i))
            .text("name", &artifact.name, &keep)
            .plain("file", artifact.file.is_some());
        if let Some(mime) = artifact.mime_type.as_deref() {
            event = event.text("mime_type", mime, &keep);
        }
        events.push(event.bounded());
    }
    let run = committed.id;
    match tokio::task::spawn_blocking(move || set.observe(&events)).await {
        Ok(report) if report.failed > 0 || report.dropped > 0 => {
            tracing::warn!(%run, failed = report.failed, dropped = report.dropped, "an observer failed or was too slow");
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(%run, error = %e, "observers did not finish"),
    }
}

/// Emit the status event for a committed transition.
async fn announce(inner: &Inner, committed: &RunRecord, next: &Next) {
    let (status, detail) = match committed.status {
        RunStatus::Parked => (RunStatus::Parked, None),
        RunStatus::Done => (RunStatus::Done, None),
        RunStatus::Failed => (RunStatus::Failed, next.error.clone()),
        RunStatus::Runnable => match (&next.retry_detail, next.status) {
            (Some(detail), _) => (RunStatus::Runnable, Some(detail.clone())),
            (None, RunStatus::Parked) => (
                RunStatus::Runnable,
                Some("woken by inbound message".to_owned()),
            ),
            // A plain `Continue`: not worth an event per transition.
            (None, _) => return,
        },
    };
    inner
        .emit_status(committed.id, &committed.agent, status, detail)
        .await;
}

#[cfg(test)]
mod artifact_tests {
    use serde_json::json;

    use super::*;

    fn made(name: &str, id: Option<&str>, data: i32) -> Artifact {
        let mut artifact = Artifact::new(name, None, json!(data));
        artifact.id = id.map(str::to_owned);
        artifact
    }

    fn names(artifacts: &[Artifact]) -> Vec<(String, i64)> {
        artifacts
            .iter()
            .map(|a| (a.name.clone(), a.data.as_i64().unwrap_or(-1)))
            .collect()
    }

    #[test]
    fn a_replaced_artifact_keeps_its_position() {
        let mut held = vec![
            made("x", None, 1),
            made("plan", Some("p"), 1),
            made("y", None, 1),
        ];
        add_artifact(&mut held, &made("plan", Some("p"), 2));
        assert_eq!(
            names(&held),
            [("x".into(), 1), ("plan".into(), 2), ("y".into(), 1)]
        );
    }

    #[test]
    fn two_of_one_id_in_one_transition_are_one_entry_with_the_second_content() {
        let mut held = vec![made("x", None, 1)];
        for artifact in [made("plan", Some("p"), 1), made("plan", Some("p"), 2)] {
            add_artifact(&mut held, &artifact);
        }
        assert_eq!(names(&held), [("x".into(), 1), ("plan".into(), 2)]);
    }

    #[test]
    fn an_artifact_without_an_id_is_always_added() {
        let mut held = vec![];
        add_artifact(&mut held, &made("a", None, 1));
        add_artifact(&mut held, &made("a", None, 1));
        assert_eq!(held.len(), 2);
    }

    #[test]
    fn a_file_never_replaces_and_is_never_replaced() {
        let mut held = vec![made("plan", Some("p"), 1)];
        let mut file = Artifact::file("logo", "image/png", "l.png", vec![1, 2, 3]).unwrap();
        file.id = Some("p".into());
        add_artifact(&mut held, &file);
        assert_eq!(held.len(), 2, "the plan stays and the file is kept");
        assert_eq!(held[0].data, json!(1));
        assert_eq!(held[1].id, None, "an id on a file is dropped");
        // with_id leaves a file without one.
        assert_eq!(
            Artifact::file("logo", "image/png", "l.png", vec![1])
                .unwrap()
                .with_id("p")
                .id,
            None
        );
        // A plan with the same id still replaces the plan, and not the file.
        add_artifact(&mut held, &made("plan", Some("p"), 2));
        assert_eq!(held.len(), 2);
        assert_eq!(held[0].data, json!(2));
    }
}

#[cfg(test)]
mod claim_tests {
    use std::sync::atomic::{AtomicU32, Ordering::SeqCst};

    use adam_core::{DynStore, MemoryStore};
    use async_trait::async_trait;
    use serde_json::json;

    use super::*;
    use crate::agent::{Agent, Inbound};

    /// Parks at every step, and counts them.
    struct Counting(Arc<AtomicU32>);

    #[async_trait]
    impl Agent for Counting {
        type State = Value;

        fn name(&self) -> &str {
            "counting"
        }

        fn init(&self, input: Inbound) -> Result<Value, AgentError> {
            Ok(input.payload)
        }

        async fn step(
            &self,
            _ctx: &mut Ctx,
            state: Value,
        ) -> Result<Transition<Value>, AgentError> {
            self.0.fetch_add(1, SeqCst);
            Ok(Transition::Park {
                state,
                wake_at: None,
            })
        }
    }

    /// The interleaving of a claim that reads the run after taking the lease: the cancel commits
    /// between the two, and the worker gets the cancelled run with the cancel's version. It must
    /// not step it, or its commit would reopen the run (and a child would never see its parent end).
    #[tokio::test]
    async fn a_claimed_run_that_is_cancelled_already_is_not_stepped() {
        let steps = Arc::new(AtomicU32::new(0));
        let store: DynStore = Arc::new(MemoryStore::new());
        let rt = Runtime::builder(store.clone())
            .agent(Counting(steps.clone()))
            .build();
        let run = rt
            .start("counting", Inbound::new("start", json!({})), None)
            .await
            .expect("start");
        rt.cancel(run, "stop").await.expect("cancel");
        let cancelled = store.load_run(run).await.expect("load").expect("run");
        assert_eq!(cancelled.status, RunStatus::Failed);

        let claimed = rt.inner.claimed(&cancelled).await;
        assert!(transition(&rt.inner, cancelled.clone(), claimed, CancelToken::new()).await);

        assert_eq!(steps.load(SeqCst), 0, "the agent was not stepped");
        let after = store.load_run(run).await.expect("load").expect("run");
        assert_eq!(after.status, RunStatus::Failed);
        assert_eq!(after.version, cancelled.version, "nothing was committed");
    }

    /// Parks at every step; splits its transcript, so the run is a signed v5 head.
    struct Splitting;

    #[async_trait]
    impl Agent for Splitting {
        type State = Value;

        fn name(&self) -> &str {
            "splitting"
        }

        fn init(&self, input: Inbound) -> Result<Value, AgentError> {
            Ok(json!({"messages": [input.payload], "n": 0}))
        }

        fn split(&self, state: &Value) -> Option<(Value, Vec<Value>)> {
            let mut head = state.clone();
            let rows = head["messages"].as_array().cloned().unwrap_or_default();
            head["messages"] = json!([]);
            Some((head, rows))
        }

        fn join(&self, mut head: Value, rows: Vec<Value>) -> Result<Value, AgentError> {
            head["messages"] = Value::Array(rows);
            Ok(head)
        }

        async fn step(
            &self,
            _ctx: &mut Ctx,
            state: Value,
        ) -> Result<Transition<Value>, AgentError> {
            Ok(Transition::Park {
                state,
                wake_at: None,
            })
        }
    }

    /// A signed, parked run of `agent` on a runtime with keys.
    async fn signed_parked(agent: impl Agent<State = Value>, name: &str) -> (Runtime, RunRecord) {
        let store: DynStore = Arc::new(MemoryStore::new());
        let rt = Runtime::builder(store.clone())
            .agent(agent)
            .turn_keys(<dyn adam_core::turn::TurnKeys>::ephemeral().expect("keys"))
            .poll_interval(std::time::Duration::from_millis(10))
            .build();
        let run = rt
            .start(name, Inbound::new("start", json!({"text": "hi"})), None)
            .await
            .expect("start");
        let (stop, rx) = tokio::sync::oneshot::channel::<()>();
        let worker = {
            let rt = rt.clone();
            tokio::spawn(async move {
                let _ = rt
                    .run_worker(async {
                        let _ = rx.await;
                    })
                    .await;
            })
        };
        loop {
            let rec = store.load_run(run).await.expect("load").expect("run");
            if rec.status == RunStatus::Parked && rec.state.get("signature").is_some() {
                let _ = stop.send(());
                worker.await.expect("worker");
                return (rt, rec);
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    fn bare_next() -> Next {
        Next {
            status: RunStatus::Parked,
            wake_at: None,
            agent: Value::Null,
            output: Value::Null,
            error: None,
            attempt: 0,
            retry: None,
            seq: 0,
            consumed: 0,
            artifacts: Vec::new(),
            retry_detail: None,
            reopen: false,
            read: Vec::new(),
            split: None,
            generation: None,
            split_refused: false,
            head_artifacts: None,
            shadow_blobs: Vec::new(),
            shadow_files: Vec::new(),
            deadline: None,
            remaining_secs: None,
        }
    }

    /// The CAS retry reloads the record a delivery changed and signs over what it takes from it:
    /// it must check that record first, v4 and v5 alike. A forged inbox (v4) or a forged member of
    /// a head (v5) fails the commit `Corrupt`; the intact record passes.
    #[tokio::test]
    async fn a_reloaded_record_that_does_not_verify_fails_the_commit() {
        let (rt, rec) = signed_parked(Counting(Arc::new(AtomicU32::new(0))), "counting").await;
        let Ok(Stored::Full(env)) = Stored::decode(rec.id, &rec.state) else {
            panic!("a v4 state");
        };
        verify_reloaded(&rt.inner, &rec, &env, None)
            .await
            .expect("the intact record verifies");
        let mut forged = rec.clone();
        forged.state["inbox"] =
            json!([{"kind": "message", "id": "x", "payload": {"text": "forged"}}]);
        let err = verify_reloaded(&rt.inner, &forged, &env, None)
            .await
            .expect_err("a forged inbox is refused");
        assert_eq!(err.class(), adam_error::ErrorClass::Corrupt, "{err}");

        let (rt, rec) = signed_parked(Splitting, "splitting").await;
        assert_eq!(rec.state["v"], 5);
        let Ok(Stored::Head(head)) = Stored::decode(rec.id, &rec.state) else {
            panic!("a head");
        };
        let (env, parts) = head.into_parts(Value::Null);
        verify_reloaded(&rt.inner, &rec, &env, Some(&parts))
            .await
            .expect("the intact head verifies");
        let mut forged = rec.clone();
        forged.state["error"] = json!("forged");
        let err = verify_reloaded(&rt.inner, &forged, &env, Some(&parts))
            .await
            .expect_err("a forged head is refused");
        assert_eq!(err.class(), adam_error::ErrorClass::Corrupt, "{err}");
    }

    /// A signed head whose seal is missing or unusable is not signed over: `seal_update` fails the
    /// commit `Corrupt`. An unsigned head stays unsigned, and an intact one is signed again.
    #[tokio::test]
    async fn a_signed_head_with_a_forged_seal_is_not_signed_over() {
        let (rt, rec) = signed_parked(Splitting, "splitting").await;
        let next = bare_next();
        let sign = |rec: RunRecord| {
            let inner = rt.inner.clone();
            let next = &next;
            async move {
                let Ok(Stored::Head(head)) = Stored::decode(rec.id, &rec.state) else {
                    panic!("a head");
                };
                let (_, parts) = head.into_parts(Value::Null);
                let mut update =
                    RunUpdate::new(RunStatus::Parked, Value::Null).head(rec.state.clone());
                let out = seal_update(&inner, &rec, next, Some(&parts), &mut update).await;
                (out, update)
            }
        };

        let (out, update) = sign(rec.clone()).await;
        out.expect("an intact head is signed again");
        assert!(update.head.expect("head")["signature"].is_string());

        let mut forged = rec.clone();
        forged.state["seal"] = json!({"len": 1, "chain": "not-a-chain"});
        let (out, _) = sign(forged).await;
        let err = out.expect_err("an unparsable seal");
        assert_eq!(err.class(), adam_error::ErrorClass::Corrupt, "{err}");

        let mut forged = rec.clone();
        forged.state.as_object_mut().unwrap().remove("seal");
        let (out, _) = sign(forged).await;
        let err = out.expect_err("a missing seal");
        assert_eq!(err.class(), adam_error::ErrorClass::Corrupt, "{err}");

        let mut unsigned = rec.clone();
        let doc = unsigned.state.as_object_mut().unwrap();
        doc.remove("seal");
        doc.remove("signature");
        let (out, update) = sign(unsigned).await;
        out.expect("an unsigned head is adopted");
        assert!(update.head.expect("head").get("signature").is_none());
    }
}

/// The span of a run's transition ([ANR-1345]): its state and nothing that identifies the run.
///
/// [ANR-1345]: https://github.com/vymalo/another-adam-rs/issues
#[cfg(test)]
mod run_span_tests {
    use super::run_span;
    use adam_core::RunStatus;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::fmt::MakeWriter;

    /// A writer the test reads back: what the subscriber rendered.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Captured {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// The span `run_span(status, child)` renders, as text.
    fn rendered(status: RunStatus, child: bool) -> String {
        let sink = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(sink.clone())
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
            .with_max_level(tracing::Level::INFO)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            run_span(status, child).in_scope(|| {});
        });
        let bytes = sink
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        String::from_utf8(bytes).expect("utf-8")
    }

    #[test]
    fn a_run_span_carries_its_state_and_nothing_that_identifies_the_run() {
        let text = rendered(RunStatus::Runnable, false);
        assert!(text.contains("adam.run"), "{text}");
        assert!(
            text.contains("state=\"runnable\"") || text.contains("state=runnable"),
            "{text}"
        );
        // The fields a collector could join on, and the content a run holds, are never here.
        for forbidden in ["run=", "run_id", "conversation", "prompt", "argument"] {
            assert!(!text.contains(forbidden), "{forbidden} in {text}");
        }
    }

    #[test]
    fn a_childs_transition_is_a_child_span() {
        let text = rendered(RunStatus::Parked, true);
        assert!(text.contains("adam.child"), "{text}");
        assert!(text.contains("parked"), "{text}");
    }
}
