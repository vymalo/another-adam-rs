//! The worker loop: claim due runs, advance them one transition, commit with
//! CAS, renew leases while stepping, release.

use std::collections::HashSet;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::stream::BoxStream;
use futures::{FutureExt, StreamExt};
use serde_json::Value;
use tokio::task::{JoinHandle, JoinSet};

use adam_core::{Lease, RunId, RunRecord, RunStatus, RunUpdate, StoreError, StoreResult};
use adam_error::{Classify, ErrorClass, report};

use crate::agent::{AgentError, Transition};
use crate::cancel::CancelToken;
use crate::ctx::{Ctx, CtxOutcome, CtxParts};
use crate::envelope::Envelope;
use crate::events::Artifact;
use crate::notify::{Delivery, Signal};
use crate::runtime::{Inner, MAX_COMMIT_RETRIES, Runtime, RuntimeError};

type InFlight = Arc<Mutex<HashSet<RunId>>>;

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

        // Graceful: let in-flight transitions finish, commit and release.
        while let Some(joined) = tasks.join_next().await {
            log_join(joined);
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
        match inner.store.load_run(run).await {
            Ok(Some(rec)) if rec.status.is_open() => {}
            Ok(_) => inner.fire_cancel(run),
            Err(e) => tracing::debug!(%run, error = %e, "resync could not read the run"),
        }
    }
}

/// Fires `token` once the run turns terminal (or vanishes) in the store, which
/// is how a cancel issued by another process reaches a step running here.
fn spawn_cancel_watch(inner: Arc<Inner>, run: RunId, token: CancelToken) -> AbortOnDrop {
    let every = inner.cfg.poll_interval.max(Duration::from_millis(1));
    AbortOnDrop(tokio::spawn(async move {
        loop {
            tokio::time::sleep(every).await;
            match inner.store.load_run(run).await {
                Ok(Some(rec)) if rec.status.is_open() => {}
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
#[tracing::instrument(skip_all, fields(run = %lease.run.id, agent = %lease.run.agent))]
async fn advance(inner: Arc<Inner>, lease: Lease, guard: InFlightGuard) {
    let run = lease.run.id;
    let renewer = inner
        .cfg
        .lease_renewal
        .then(|| spawn_renewer(inner.clone(), run));
    let cancel = CancelToken::new();
    let tracked = inner.track(run, cancel.clone());
    let watch = spawn_cancel_watch(inner.clone(), run, cancel.clone());
    let release = transition(&inner, lease.run, cancel).await;
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
    wake_at: Option<DateTime<Utc>>,
    /// The agent's state to store.
    agent: Value,
    output: Value,
    error: Option<String>,
    attempt: u32,
    seq: u64,
    /// How many inbox messages this transition consumed (a prefix).
    consumed: usize,
    artifacts: Vec<Artifact>,
    /// Set on a scheduled retry; shown in the status event.
    retry_detail: Option<String>,
}

enum Plan {
    Commit(Box<Next>),
    /// Infrastructure trouble: commit nothing and keep the lease, so the run
    /// is retried when it expires instead of being hammered.
    Leave,
}

/// Returns whether the lease should be released.
async fn transition(inner: &Arc<Inner>, rec: RunRecord, cancel: CancelToken) -> bool {
    let run = rec.id;
    let env = match Envelope::decode(run, &rec.state) {
        Ok(env) => env,
        Err(e) => {
            tracing::error!(error = %e, "failing run with unreadable state");
            let update = RunUpdate::new(RunStatus::Failed, rec.state.clone());
            if let Ok(committed) = inner.store.commit_run(run, rec.version, update).await {
                inner
                    .emit_status(run, &rec.agent, RunStatus::Failed, Some(e.to_string()))
                    .await;
                inner.notify_parent(&committed).await;
            }
            return true;
        }
    };
    let Some(agent) = inner
        .agents
        .get(&rec.agent)
        .and_then(|r| r.agent().cloned())
    else {
        tracing::error!(agent = %rec.agent, "claimed a run of an unregistered agent");
        return true;
    };

    let mut ctx = Ctx::new(CtxParts {
        run,
        agent: rec.agent.clone(),
        conversation_id: rec.conversation_id.clone(),
        attempt: env.attempt,
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
    let stepped = AssertUnwindSafe(agent.step(&mut ctx, env.agent.clone()))
        .catch_unwind()
        .await;
    let result = stepped.unwrap_or_else(|panic| {
        Err(AgentError::transient(format!(
            "agent panicked: {}",
            panic_message(&panic)
        )))
    });
    let outcome = ctx.into_outcome();

    let plan = plan(inner, &env, outcome, result);
    let Plan::Commit(next) = plan else {
        return false;
    };
    match commit(inner, &rec, &env, &next).await {
        Ok(Some(committed)) => {
            announce(inner, &committed, &next).await;
            // After the commit, never before: the parent's timer is the fallback for a crash here.
            inner.notify_parent(&committed).await;
            true
        }
        Ok(None) => {
            tracing::info!(
                "commit rejected (run advanced or cancelled elsewhere); dropping result"
            );
            true
        }
        Err(e) => {
            tracing::error!(error = %report(&e), "commit failed; the run will be retried when its lease expires");
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
fn plan(
    inner: &Inner,
    base: &Envelope,
    out: CtxOutcome,
    result: Result<Transition<Value>, AgentError>,
) -> Plan {
    let CtxOutcome {
        seq,
        consumed,
        artifacts,
    } = out;
    // Starting point for a transition that made progress.
    let progressed = |status, wake_at, agent, output, error| Next {
        status,
        wake_at,
        agent,
        output,
        error,
        attempt: 0,
        seq,
        consumed,
        artifacts: artifacts.clone(),
        retry_detail: None,
    };
    // Starting point for a failure that ends the run: state untouched.
    let failed = |error: String| Next {
        status: RunStatus::Failed,
        wake_at: None,
        agent: base.agent.clone(),
        output: Value::Null,
        error: Some(error),
        attempt: base.attempt,
        seq,
        consumed: 0,
        artifacts: artifacts.clone(),
        retry_detail: None,
    };

    // A transient failure: schedule the retry, or give up when the policy's
    // attempts are used up. `hint` is a minimum wait from the error.
    let retrying = |msg: String, hint: Option<Duration>| -> Next {
        let failures = base.attempt.saturating_add(1);
        let policy = &inner.cfg.retry;
        if failures >= policy.max_attempts {
            Next {
                attempt: failures,
                ..failed(format!("gave up after {failures} attempts: {msg}"))
            }
        } else {
            let delay = match hint {
                Some(at_least) => policy.delay_with_hint(failures, at_least),
                None => policy.backoff(failures),
            };
            let wake_at = inner
                .clock
                .now()
                .checked_add_signed(
                    chrono::Duration::from_std(delay).unwrap_or(chrono::Duration::MAX),
                )
                .unwrap_or(DateTime::<Utc>::MAX_UTC);
            Next {
                status: RunStatus::Runnable,
                wake_at: Some(wake_at),
                agent: base.agent.clone(),
                output: Value::Null,
                error: None,
                attempt: failures,
                // Abandon this try's journal entries so the retry runs
                // its steps afresh (see `Ctx::step`).
                seq,
                consumed: 0,
                artifacts: Vec::new(),
                retry_detail: Some(format!(
                    "attempt {failures} of {} failed, retrying in {delay:?}: {msg}",
                    policy.max_attempts
                )),
            }
        }
    };

    let next = match result {
        Ok(Transition::Continue(s)) => progressed(RunStatus::Runnable, None, s, Value::Null, None),
        Ok(Transition::Park { state, wake_at }) => {
            progressed(RunStatus::Parked, wake_at, state, Value::Null, None)
        }
        Ok(Transition::Done { state, output }) => {
            progressed(RunStatus::Done, None, state, output, None)
        }
        Ok(Transition::Fail { state, error }) => {
            progressed(RunStatus::Failed, None, state, Value::Null, Some(error))
        }
        // A retry hint from the peer only lengthens the policy's backoff.
        Err(AgentError::Transient {
            message,
            retry_after,
            source,
        }) => retrying(with_cause(message, source.as_deref()), retry_after),
        Err(AgentError::Permanent { message, source }) => {
            failed(with_cause(message, source.as_deref()))
        }
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
                    error = %report(&e),
                    "the store cannot read or accept this run's data; failing the run"
                );
                failed(report(&e))
            }
            // Anything else (an unreachable store, a bug): leave the run to its lease.
            class => {
                tracing::error!(
                    error = %report(&e),
                    class = ?class,
                    alert = class.should_alert(),
                    "store error while stepping; leaving the run leased"
                );
                return Plan::Leave;
            }
        },
    };
    Plan::Commit(Box::new(next))
}

/// The record to write, built on the freshest envelope.
fn build_update(
    next: &Next,
    cur: &Envelope,
    base_inbox_len: usize,
    base_rev: u64,
) -> StoreResult<RunUpdate> {
    // Messages that arrived while stepping sit after the ones we started
    // with. A parked agent must not sleep through them.
    let arrived = cur.inbox.len() > base_inbox_len;
    let (status, wake_at) = if next.status == RunStatus::Parked && arrived {
        (RunStatus::Runnable, None)
    } else {
        (next.status, next.wake_at)
    };
    let mut artifacts = cur.artifacts.clone();
    artifacts.extend(next.artifacts.iter().cloned());
    let env = Envelope {
        v: cur.v,
        agent: next.agent.clone(),
        inbox: cur.inbox.iter().skip(next.consumed).cloned().collect(),
        seq: next.seq,
        attempt: next.attempt,
        rev: base_rev + 1,
        output: next.output.clone(),
        error: next.error.clone(),
        artifacts,
    };
    let mut update = RunUpdate::new(status, env.encode()?);
    update.wake_at = wake_at;
    Ok(update)
}

/// Commit with CAS. A conflict caused only by messages delivered while we
/// were stepping is merged (the messages are kept); any other conflict means
/// someone else advanced or cancelled the run, and the result is dropped
/// (`Ok(None)`).
async fn commit(
    inner: &Inner,
    claimed: &RunRecord,
    base: &Envelope,
    next: &Next,
) -> StoreResult<Option<RunRecord>> {
    let run = claimed.id;
    let mut cur_rec = claimed.clone();
    let mut cur_env = base.clone();
    for attempt in 0..MAX_COMMIT_RETRIES {
        if attempt > 0 {
            let Some(rec) = inner.store.load_run(run).await? else {
                return Ok(None);
            };
            let env = match Envelope::decode(run, &rec.state) {
                Ok(env) => env,
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
            cur_rec = rec;
            cur_env = env;
        }
        let update = build_update(next, &cur_env, base.inbox.len(), base.rev)?;
        match inner.store.commit_run(run, cur_rec.version, update).await {
            Ok(rec) => return Ok(Some(rec)),
            Err(StoreError::Conflict { .. }) => {}
            Err(StoreError::NotFound(_)) => return Ok(None),
            Err(e) => return Err(e),
        }
    }
    tracing::warn!(%run, "gave up merging a commit after repeated conflicts");
    Ok(None)
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
