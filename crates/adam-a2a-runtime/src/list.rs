//! `ListTasks` over the run store: a caller's tasks, newest update first, by keyset cursor.
//!
//! The store does the scoping, the status narrowing it can express and the paging
//! ([`Store::list_runs`](adam_core::Store::list_runs): one indexed read per page). What it cannot
//! express is the A2A state, which is derived from a run's status **and** its error text, its
//! lease and its version (see [`task_state`](crate::task_state)): `failed` and `canceled` are both
//! a failed run, `submitted` and `working` both a runnable one. So the store narrows by the run
//! statuses the state can come from, and each candidate is read as a task and kept only if its
//! state is the one asked for.
//!
//! `total_size` comes from [`Store::count_runs`](adam_core::Store::count_runs) over those statuses,
//! so it is **exact** with no `status` filter, for `completed` (one status, one state) and for the
//! states this backend never produces (`0`), and an **upper bound** for `failed`, `canceled`,
//! `input-required`, `working` and `submitted`, whose statuses also hold runs in another state.
//! An exact count would read every run. The tasks of a page are always exactly the ones asked for.
//!
//! The scan is bounded ([`MAX_SCAN`]): a page that cannot be filled within it is returned short
//! with a token that continues from where the scan stopped.

use a2a::{Task, TaskState};
use adam_a2a::{BackendError, Caller, PageToken, TaskPage, TaskQuery};
use adam_core::{ConversationScope, RunId, RunQuery, RunStatus};
use adam_runtime::RuntimeError;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::backend::{RuntimeTaskBackend, map_err};
use crate::convert::{decode_conversation, encode_conversation, task_from_view};

/// The most runs one page reads while looking for matches that need their state read. A filter
/// that matches rarely returns a short page and a token instead of reading a whole history.
pub const MAX_SCAN: usize = 500;

/// The run statuses a task in `state` can come from, and whether that is exact (every run in
/// those statuses is in that state). `None` statuses: the state is never produced here.
fn narrowing(state: Option<&TaskState>) -> (Option<Vec<RunStatus>>, bool) {
    match state {
        None => (None, true),
        Some(TaskState::Completed) => (Some(vec![RunStatus::Done]), true),
        Some(TaskState::Failed | TaskState::Canceled) => (Some(vec![RunStatus::Failed]), false),
        Some(TaskState::InputRequired) => (Some(vec![RunStatus::Parked]), false),
        Some(TaskState::Working) => (Some(vec![RunStatus::Runnable, RunStatus::Parked]), false),
        Some(TaskState::Submitted) => (Some(vec![RunStatus::Runnable]), false),
        // Never produced by this backend (`rejected`, `auth-required`) or no state at all.
        Some(TaskState::Rejected | TaskState::AuthRequired | TaskState::Unspecified) => {
            (Some(Vec::new()), true)
        }
    }
}

impl RuntimeTaskBackend {
    pub(crate) async fn list_tasks(
        &self,
        caller: &Caller,
        query: &TaskQuery,
    ) -> Result<TaskPage, BackendError> {
        let after = query
            .page_token
            .as_deref()
            .map(|token| PageToken::decode(token, caller, query))
            .transpose()?
            .map(|t| {
                Uuid::parse_str(&t.id)
                    .map(|id| (t.updated_at, RunId(id)))
                    .map_err(|_| BackendError::InvalidParams("invalid page token".to_owned()))
            })
            .transpose()?;

        // Only the caller's conversations, before anything else: `<subject>:` is a prefix no other
        // subject's conversations share (a `:` in a subject is escaped).
        let scope = match &query.context_id {
            Some(context) => {
                ConversationScope::Exact(encode_conversation(&caller.subject, context))
            }
            None => ConversationScope::Prefix(encode_conversation(&caller.subject, "")),
        };
        let (statuses, exact) = narrowing(query.status.as_ref());
        let mut runs = RunQuery::new(&self.agent, scope, 0);
        runs.statuses = statuses;
        runs.updated_since = query.status_timestamp_after;
        if runs.statuses.as_ref().is_some_and(Vec::is_empty) {
            return Ok(TaskPage::default());
        }
        let store = self.runtime.store();
        let total = store
            .count_runs(&runs)
            .await
            .map_err(|e| map_err(e.into()))?;

        let want = query.page_size + 1;
        let batch = want.clamp(50, 200);
        let mut found: Vec<(Task, DateTime<Utc>, RunId)> = Vec::new();
        let mut cursor = after;
        let mut scanned = 0usize;
        let mut exhausted = false;
        'scan: loop {
            runs.after = cursor;
            runs.limit = batch;
            let page = store
                .list_runs(&runs)
                .await
                .map_err(|e| map_err(e.into()))?;
            let n = page.len();
            for rec in page {
                cursor = Some((rec.updated_at, rec.id));
                scanned += 1;
                let Some((_, context)) =
                    rec.conversation_id.as_deref().and_then(decode_conversation)
                else {
                    continue;
                };
                let view = match self.runtime.view(rec.id).await {
                    Ok(Some(view)) => view,
                    Ok(None) => continue,
                    Err(RuntimeError::Corrupt { .. }) => {
                        tracing::warn!(run = %rec.id, "a task of the caller has an unreadable state; it is not listed");
                        continue;
                    }
                    Err(e) => return Err(map_err(e)),
                };
                let task = task_from_view(&view, &context, &self.prompt);
                if !exact
                    && query
                        .status
                        .as_ref()
                        .is_some_and(|s| &task.status.state != s)
                {
                    if scanned >= MAX_SCAN {
                        break 'scan;
                    }
                    continue;
                }
                found.push((task, rec.updated_at, rec.id));
                if found.len() == want || scanned >= MAX_SCAN {
                    break 'scan;
                }
            }
            if n < batch {
                exhausted = true;
                break;
            }
        }

        let more = found.len() > query.page_size || (!exhausted && found.len() < want);
        found.truncate(query.page_size);
        // The next page starts after the last task returned; a page cut short by the scan bound
        // continues after the last run looked at.
        let position = if found.len() == query.page_size && more {
            found.last().map(|(_, at, id)| (*at, *id))
        } else {
            cursor
        };
        let next = more
            .then_some(position)
            .flatten()
            .map(|(at, id)| PageToken::encode(caller, query, at, &id.to_string()));
        Ok(TaskPage::new(
            found.into_iter().map(|(task, _, _)| task).collect(),
            next,
            usize::try_from(total).unwrap_or(usize::MAX),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_statuses_a_state_can_come_from() {
        assert_eq!(narrowing(None), (None, true));
        assert_eq!(
            narrowing(Some(&TaskState::Completed)),
            (Some(vec![RunStatus::Done]), true)
        );
        for state in [TaskState::Failed, TaskState::Canceled] {
            assert_eq!(
                narrowing(Some(&state)),
                (Some(vec![RunStatus::Failed]), false)
            );
        }
        assert_eq!(
            narrowing(Some(&TaskState::Submitted)),
            (Some(vec![RunStatus::Runnable]), false)
        );
        assert_eq!(
            narrowing(Some(&TaskState::Working)),
            (Some(vec![RunStatus::Runnable, RunStatus::Parked]), false)
        );
        assert_eq!(
            narrowing(Some(&TaskState::InputRequired)),
            (Some(vec![RunStatus::Parked]), false)
        );
        for state in [
            TaskState::Rejected,
            TaskState::AuthRequired,
            TaskState::Unspecified,
        ] {
            assert_eq!(
                narrowing(Some(&state)),
                (Some(Vec::new()), true),
                "never produced"
            );
        }
    }
}
