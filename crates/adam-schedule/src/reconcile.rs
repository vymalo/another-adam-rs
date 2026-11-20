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

    let latest = latest_by_conversation(runtime, agent).await?;
    apply(runtime, agent, &expected, latest).await
}

/// The rules of [`reconcile`] over a listing read before: a listing gone stale (another process
/// started a scheduler since) is what the `ConversationBusy` branch mends.
async fn apply(
    runtime: &Runtime,
    agent: &str,
    expected: &BTreeMap<String, (&Schedule, ScheduleSpec)>,
    mut latest: BTreeMap<String, RunRecord>,
) -> Result<Reconciled, ReconcileError> {
    let mut done = Reconciled::default();

    for (conversation, rec) in &latest {
        if !expected.contains_key(conversation) && rec.status.is_open() {
            runtime.cancel(rec.id, REMOVED_REASON).await?;
            done.cancelled.push(name_of(conversation, agent));
        }
    }

    for (conversation, (schedule, spec)) in expected {
        for attempt in 0..=BUSY_RETRIES {
            if let Some(rec) = latest.get(conversation) {
                let hash = hash_of(rec);
                let same = hash.as_deref() == Some(spec.hash());
                // A finished run whose state cannot be read is kept, not started again: a fired
                // one-shot would fire twice.
                if (same && (rec.status.is_open() || rec.status == RunStatus::Done))
                    || (hash.is_none() && rec.status == RunStatus::Done)
                {
                    done.kept.push(schedule.name.clone());
                    break;
                }
                if rec.status.is_open() {
                    runtime.cancel(rec.id, CHANGED_REASON).await?;
                    done.cancelled.push(schedule.name.clone());
                }
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

/// The spec hash a scheduler run's state carries, `None` when it has none: only the field is read,
/// so a later release that adds fields to the state does not make every run unreadable.
fn hash_of(rec: &RunRecord) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Hash {
        hash: String,
    }
    let view = view_from_record(rec.clone(), false).ok()?;
    serde_json::from_value::<Hash>(view.state)
        .ok()
        .map(|h| h.hash)
}

/// The schedule name of a scheduler conversation of `agent`, for reporting.
fn name_of(conversation: &str, agent: &str) -> String {
    let prefix = format!("{SCHEDULE_CONVERSATION_PREFIX}{agent}/");
    conversation
        .strip_prefix(&prefix)
        .and_then(crate::ids::unescape)
        .unwrap_or_else(|| conversation.to_owned())
}

/// The scheduler run that decides for each conversation of `agent`: its open one (the store keeps
/// at most one), else the one created last.
async fn latest_by_conversation(
    runtime: &Runtime,
    agent: &str,
) -> Result<BTreeMap<String, RunRecord>, RuntimeError> {
    let prefix = format!("{SCHEDULE_CONVERSATION_PREFIX}{agent}/");
    let mut out: BTreeMap<String, RunRecord> = BTreeMap::new();
    for rec in list(runtime, ConversationScope::Prefix(prefix)).await? {
        let Some(conversation) = rec.conversation_id.clone() else {
            continue;
        };
        let replace = out
            .get(&conversation)
            .is_none_or(|seen| decides_over(&rec, seen));
        if replace {
            out.insert(conversation, rec);
        }
    }
    Ok(out)
}

/// The scheduler run that decides for one conversation.
async fn latest_in(
    runtime: &Runtime,
    conversation: &str,
) -> Result<Option<RunRecord>, RuntimeError> {
    let mut latest: Option<RunRecord> = None;
    for rec in list(runtime, ConversationScope::Exact(conversation.to_owned())).await? {
        if latest.as_ref().is_none_or(|seen| decides_over(&rec, seen)) {
            latest = Some(rec);
        }
    }
    Ok(latest)
}

/// Whether `rec` decides over `seen`: an open run over a finished one, else the one created last.
fn decides_over(rec: &RunRecord, seen: &RunRecord) -> bool {
    match (rec.status.is_open(), seen.status.is_open()) {
        (true, false) => true,
        (false, true) => false,
        _ => (rec.created_at, rec.id) > (seen.created_at, seen.id),
    }
}

/// Every scheduler run in `scope`, all pages.
async fn list(runtime: &Runtime, scope: ConversationScope) -> Result<Vec<RunRecord>, RuntimeError> {
    let mut query = RunQuery::new(SCHEDULER_AGENT, scope, PAGE);
    let mut out = Vec::new();
    loop {
        let page = runtime.store().list_runs(&query).await?;
        let last = page.last().map(|r| (r.updated_at, r.id));
        let full = page.len() == PAGE;
        out.extend(page);
        match (full, last) {
            (true, Some(last)) => query.after = Some(last),
            _ => break,
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use std::path::PathBuf;
    use std::sync::Arc;

    use adam_core::{DynStore, MemoryStore};

    use super::*;
    use crate::SchedulerAgent;

    fn record(status: RunStatus, created_at: &str) -> RunRecord {
        let at = adam_core::time::Timestamp(created_at.parse().unwrap());
        RunRecord {
            id: RunId::new(),
            agent: SCHEDULER_AGENT.into(),
            conversation_id: Some("adam.schedule:coder/daily".into()),
            parent_id: None,
            status,
            state: serde_json::Value::Null,
            wake_at: None,
            version: 1,
            created_at: at,
            updated_at: at,
        }
    }

    /// The open run decides even when a finished one looks newer: processes' clocks differ.
    #[test]
    fn the_open_run_decides_over_a_finished_one() {
        let open = record(RunStatus::Parked, "2026-11-20T10:00:00Z");
        let done = record(RunStatus::Done, "2026-11-20T11:00:00Z");
        let failed = record(RunStatus::Failed, "2026-11-20T12:00:00Z");
        assert!(decides_over(&open, &done) && decides_over(&open, &failed));
        assert!(!decides_over(&done, &open) && !decides_over(&failed, &open));
        // Between finished runs, the one created last.
        assert!(decides_over(&failed, &done) && !decides_over(&done, &failed));
    }

    /// Another process starts the scheduler after this one listed: the start is refused with
    /// `ConversationBusy`, the conversation is read again, and the other's run is kept.
    #[tokio::test]
    async fn a_start_refused_as_busy_reads_again_and_keeps_the_other() {
        let store: DynStore = Arc::new(MemoryStore::new());
        let rt = Runtime::builder(store).agent(SchedulerAgent).build();
        let schedule = Schedule {
            name: "daily".into(),
            cron: "0 9 * * *".into(),
            at: None,
            after: None,
            timezone: "UTC".into(),
            agent: "coder".into(),
            prompt: "Go.".into(),
            path: PathBuf::new(),
        };
        let stale = latest_by_conversation(&rt, "coder").await.unwrap();
        assert!(stale.is_empty());
        let other = reconcile(&rt, "coder", std::slice::from_ref(&schedule))
            .await
            .unwrap();
        assert_eq!(other.started, ["daily"]);

        let spec = ScheduleSpec::from_schedule(&schedule).unwrap();
        let mut expected = BTreeMap::new();
        expected.insert(
            schedule_conversation_id("coder", "daily"),
            (&schedule, spec),
        );
        let done = apply(&rt, "coder", &expected, stale).await.unwrap();
        assert_eq!(
            done,
            Reconciled {
                kept: vec!["daily".to_owned()],
                ..Reconciled::default()
            }
        );
        let open = list(
            &rt,
            ConversationScope::Prefix("adam.schedule:coder/".into()),
        )
        .await
        .unwrap();
        assert_eq!(open.len(), 1);
        assert!(open[0].status.is_open());
    }
}
