//! Reconcile a runtime's scheduler runs with an agent's schedules (ADR 0060, decision 5).

use std::collections::BTreeMap;

use adam_agent_fs::Schedule;
use adam_core::{ConversationScope, RunId, RunQuery, RunRecord, RunStatus};
use adam_runtime::{
    Inbound, Runtime, RuntimeError, SCHEDULE_CONVERSATION_PREFIX, view_from_record,
};

use crate::agent::{SCHEDULER_AGENT, SCHEDULER_START_KIND, SchedulerState};
use crate::{ScheduleError, ScheduleSpec, schedule_conversation_id};

/// The reason a scheduler run is cancelled with when its schedule is gone.
pub const REMOVED_REASON: &str = "schedule removed";
/// The reason a scheduler run is cancelled with when its schedule changed.
pub const CHANGED_REASON: &str = "schedule changed";

/// How often a start refused because another process started one first is read again.
const BUSY_RETRIES: usize = 3;
const PAGE: usize = 100;

/// What [`reconcile`] did, by schedule name, each list sorted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reconciled {
    /// Schedules whose scheduler was started now.
    pub started: Vec<String>,
    /// Schedules whose scheduler was left as it is: open with the same spec, or a one-shot that
    /// fired.
    pub kept: Vec<String>,
    /// Scheduler runs cancelled: their schedule changed (and was started again) or is gone. A
    /// removed schedule is named here only.
    pub cancelled: Vec<String>,
}

/// Why [`reconcile`] failed.
#[derive(Debug, thiserror::Error)]
pub enum ReconcileError {
    /// A schedule the folder holds cannot be evaluated: nothing was changed.
    #[error(transparent)]
    Schedule(#[from] ScheduleError),
    /// The runtime or its store failed; what was done before stays done, and a second call
    /// finishes the work.
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
}

/// Make the scheduler runs of `agent` on `runtime` match `schedules`, the folder's schedules of
/// that agent (decision 5). For each schedule, the latest scheduler run of its conversation
/// decides:
///
/// * open with the same spec hash, or `Done` with it (a one-shot that fired): kept;
/// * open with another hash: cancelled (`schedule changed`), the cancel committed before anything
///   starts, then started again;
/// * none, failed (a cancel ends a run `Failed`), or finished with another hash: started, with a
///   new id and an anchor read from `runtime`'s clock.
///
/// An open scheduler of a conversation no schedule asks for is cancelled (`schedule removed`). A
/// start refused with `ConversationBusy` means another process started one first: the
/// conversation is read again and the rules applied once more.
///
/// The runtime must have [`SchedulerAgent`](crate::SchedulerAgent) registered.
///
/// # Errors
///
/// [`ReconcileError::Schedule`] before anything is changed, when a schedule does not evaluate;
/// [`ReconcileError::Runtime`] for the runtime's and the store's errors.
pub async fn reconcile(
    runtime: &Runtime,
    agent: &str,
    schedules: &[Schedule],
) -> Result<Reconciled, ReconcileError> {
    let mut expected = BTreeMap::new();
    for schedule in schedules.iter().filter(|s| s.agent == agent) {
        let spec = ScheduleSpec::from_schedule(schedule)?;
        expected.insert(
            schedule_conversation_id(agent, &schedule.name),
            (schedule, spec),
        );
    }

    let mut latest = latest_by_conversation(runtime, agent).await?;
    let mut done = Reconciled::default();

    for (conversation, rec) in &latest {
        if !expected.contains_key(conversation) && rec.status.is_open() {
            runtime.cancel(rec.id, REMOVED_REASON).await?;
            done.cancelled.push(name_of(conversation, agent));
        }
    }

    for (conversation, (schedule, spec)) in &expected {
        for attempt in 0..=BUSY_RETRIES {
            let current = latest.get(conversation).map(|rec| (rec, hash_of(rec)));
            match current {
                Some((rec, Some(hash)))
                    if hash == spec.hash()
                        && (rec.status.is_open() || rec.status == RunStatus::Done) =>
                {
                    done.kept.push(schedule.name.clone());
                    break;
                }
                Some((rec, _)) if rec.status.is_open() => {
                    runtime.cancel(rec.id, CHANGED_REASON).await?;
                    done.cancelled.push(schedule.name.clone());
                }
                _ => {}
            }
            let state = SchedulerState::new(schedule, spec, runtime.now());
            let payload = serde_json::to_value(&state).map_err(|e| {
                RuntimeError::Agent(adam_runtime::AgentError::permanent(format!(
                    "a scheduler's state does not serialize: {e}"
                )))
            })?;
            let started = runtime
                .start_with_id(
                    RunId::new(),
                    SCHEDULER_AGENT,
                    Inbound::new(SCHEDULER_START_KIND, payload),
                    Some(conversation),
                )
                .await;
            match started {
                Ok(_) => {
                    done.started.push(schedule.name.clone());
                    break;
                }
                Err(RuntimeError::ConversationBusy { .. }) if attempt < BUSY_RETRIES => {
                    // Another process started one first: read the conversation and decide again.
                    match latest_in(runtime, conversation).await? {
                        Some(rec) => latest.insert(conversation.clone(), rec),
                        None => latest.remove(conversation),
                    };
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    for list in [&mut done.started, &mut done.kept, &mut done.cancelled] {
        list.sort();
        list.dedup();
    }
    Ok(done)
}

/// The spec hash a scheduler run's state carries, `None` when the state is not a scheduler's.
fn hash_of(rec: &RunRecord) -> Option<String> {
    let view = view_from_record(rec.clone(), false).ok()?;
    let state: SchedulerState = serde_json::from_value(view.state).ok()?;
    Some(state.hash)
}

/// The schedule name of a scheduler conversation of `agent`, for reporting.
fn name_of(conversation: &str, agent: &str) -> String {
    let prefix = format!("{SCHEDULE_CONVERSATION_PREFIX}{agent}/");
    conversation
        .strip_prefix(&prefix)
        .and_then(crate::ids::unescape)
        .unwrap_or_else(|| conversation.to_owned())
}

/// The latest scheduler run of each conversation of `agent`: the one updated last.
async fn latest_by_conversation(
    runtime: &Runtime,
    agent: &str,
) -> Result<BTreeMap<String, RunRecord>, RuntimeError> {
    let prefix = format!("{SCHEDULE_CONVERSATION_PREFIX}{agent}/");
    let mut out: BTreeMap<String, RunRecord> = BTreeMap::new();
    let mut query = RunQuery::new(SCHEDULER_AGENT, ConversationScope::Prefix(prefix), PAGE);
    loop {
        let page = runtime.store().list_runs(&query).await?;
        let last = page.last().map(|r| (r.updated_at, r.id));
        let full = page.len() == PAGE;
        for rec in page {
            let Some(conversation) = rec.conversation_id.clone() else {
                continue;
            };
            // A firing's conversation is the root agent's, never the scheduler's.
            if conversation.contains('@') {
                continue;
            }
            match out.get(&conversation) {
                Some(seen) if (seen.updated_at, seen.id) >= (rec.updated_at, rec.id) => {}
                _ => {
                    out.insert(conversation, rec);
                }
            }
        }
        match (full, last) {
            (true, Some(last)) => query.after = Some(last),
            _ => break,
        }
    }
    Ok(out)
}

/// The latest scheduler run of one conversation.
async fn latest_in(
    runtime: &Runtime,
    conversation: &str,
) -> Result<Option<RunRecord>, RuntimeError> {
    let mut query = RunQuery::new(
        SCHEDULER_AGENT,
        ConversationScope::Exact(conversation.to_owned()),
        PAGE,
    );
    let mut latest: Option<RunRecord> = None;
    loop {
        let page = runtime.store().list_runs(&query).await?;
        let last = page.last().map(|r| (r.updated_at, r.id));
        let full = page.len() == PAGE;
        for rec in page {
            if latest
                .as_ref()
                .is_none_or(|seen| (seen.updated_at, seen.id) < (rec.updated_at, rec.id))
            {
                latest = Some(rec);
            }
        }
        match (full, last) {
            (true, Some(last)) => query.after = Some(last),
            _ => break,
        }
    }
    Ok(latest)
}
