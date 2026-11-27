//! The [`RunSource`] over a [`Runtime`]: any `Store` adapter (PostgreSQL, MongoDB, SQLite,
//! memory) through the runtime that joins its runs.

use adam_core::RunId;
use adam_core::store::{ConversationScope, RunQuery, RunRecord};
use adam_core::time::Timestamp;
use adam_error::ErrorClass;
use adam_llm_agent::Conversation;
use adam_runtime::{Runtime, RuntimeError, view_from_record};
use async_trait::async_trait;

use crate::{
    Cursor, RequestShape, RunMeta, RunSource, Skipped, SourceError, SourcePage, SourceQuery,
    SourceRun, TurnRequest,
};

/// The runs of a [`Runtime`]'s store, each joined with [`Runtime::state`].
///
/// A page is one [`Store::list_runs`](adam_core::Store::list_runs), newest first, with the query's
/// statuses and `since` pushed into it; `until` is checked here and counted in
/// [`Skipped::outside_query`]. **Only runs that have a conversation id are listed**
/// (`ConversationScope::Prefix("")`). Each agent must be registered on the runtime: its `join`
/// reads a v5 run's transcript. A state that does not decode as a `Conversation` with messages is
/// [`Skipped::not_a_conversation`]; one that cannot be read is logged with its id and
/// [`Skipped::unreadable`].
#[derive(Clone)]
pub struct RuntimeSource {
    runtime: Runtime,
    agents: Vec<String>,
    label: Option<String>,
}

impl RuntimeSource {
    /// Every agent registered on `runtime` ([`Runtime::agent_names`]), unnamed.
    pub fn new(runtime: Runtime) -> Self {
        let agents = runtime.agent_names();
        Self {
            runtime,
            agents,
            label: None,
        }
    }

    /// Export only `agents`, in this order.
    #[must_use]
    pub fn only(mut self, agents: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.agents = agents.into_iter().map(Into::into).collect();
        self
    }

    /// Name the source `label` (see [`RunSource::label`]).
    #[must_use]
    pub fn named(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }
}

impl std::fmt::Debug for RuntimeSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeSource")
            .field("agents", &self.agents)
            .field("label", &self.label)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl RunSource for RuntimeSource {
    fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }

    fn agents(&self) -> &[String] {
        &self.agents
    }

    async fn page(
        &self,
        query: &SourceQuery,
        after: Option<&Cursor>,
    ) -> Result<SourcePage, SourceError> {
        let mut listing = RunQuery::new(
            query.agent.as_str(),
            ConversationScope::Prefix(String::new()),
            query.limit.max(1),
        );
        listing.statuses = (!query.statuses.is_empty()).then(|| query.statuses.clone());
        listing.updated_since = query.since;
        listing.after = after.map(position_of).transpose()?;
        let records = self
            .runtime
            .store()
            .list_runs(&listing)
            .await
            .map_err(RuntimeError::from)?;
        let next = match records.last() {
            Some(last) if records.len() == listing.limit => Some(cursor_of(last)?),
            _ => None,
        };
        let mut runs = Vec::with_capacity(records.len());
        let mut skipped = Skipped::default();
        for record in records {
            if !query.admits(record.status, record.updated_at) {
                skipped.outside_query += 1;
                continue;
            }
            match read(&self.runtime, record).await {
                Ok(Some(run)) => runs.push(run),
                Ok(None) => skipped.not_a_conversation += 1,
                Err((run, e)) => {
                    tracing::warn!(%run, error = %adam_error::report(&e), "a run's state could not be read; it is not exported");
                    skipped.unreadable += 1;
                }
            }
        }
        Ok(SourcePage::new(runs, skipped, next))
    }
}

/// The cursor after `record`: its place in the listing's order, `(updated_at, id)`, as JSON.
fn cursor_of(record: &RunRecord) -> Result<Cursor, SourceError> {
    serde_json::to_string(&(record.updated_at, record.id))
        .map(Cursor::new)
        .map_err(|e| SourceError::backend(ErrorClass::Internal, e))
}

/// The listing position `cursor` names.
fn position_of(cursor: &Cursor) -> Result<(Timestamp, RunId), SourceError> {
    serde_json::from_str(cursor.as_str()).map_err(|_| SourceError::Cursor(cursor.as_str().into()))
}

/// The run `record` read whole, or `None` when its state is not a conversation with messages.
async fn read(
    runtime: &Runtime,
    record: RunRecord,
) -> Result<Option<SourceRun>, (RunId, RuntimeError)> {
    let run = record.id;
    let state = runtime.state(run).await.map_err(|e| (run, e))?;
    let Some(state) = state else {
        return Ok(None);
    };
    // Every field of a `Conversation` has a default, so any object decodes: a state with no
    // messages is not one.
    let conversation = match serde_json::from_value::<Conversation>(state) {
        Ok(conversation) if !conversation.messages.is_empty() => conversation,
        _ => return Ok(None),
    };
    let error = view_from_record(record.clone(), false)
        .map_err(|e| (run, e))?
        .error;
    let requests = recorded_requests(runtime, run, &conversation).await;
    Ok(Some(SourceRun::new(
        RunMeta::of(&record, error),
        conversation,
        requests,
    )))
}

/// The requests `conversation` recorded, read from the store: its distinct requests
/// (`Conversation::requests`), or, for a run recorded before the list existed, its last call's
/// (`Conversation::request`). All or none: a request whose bytes were collected, or that does not
/// read, would stand for the calls of another, so then none is used, and the records fall back to
/// the current shape or to none and say so in `tools_source`.
async fn recorded_requests(
    runtime: &Runtime,
    run: RunId,
    conversation: &Conversation,
) -> Vec<TurnRequest> {
    let entries: Vec<_> = if conversation.requests.is_empty() {
        conversation.request.iter().collect()
    } else {
        conversation.requests.iter().collect()
    };
    let mut recorded = Vec::with_capacity(entries.len());
    for entry in entries {
        match entry.load(runtime.store().as_ref()).await {
            Ok(Some(content)) => recorded.push(TurnRequest {
                turn: entry.turn,
                shape: RequestShape {
                    system: content.system,
                    tools: content.tools,
                },
            }),
            Ok(None) => return Vec::new(),
            Err(e) => {
                tracing::warn!(%run, error = %adam_error::report(&e), "a run's recorded request could not be read; its records have no recorded tools");
                return Vec::new();
            }
        }
    }
    recorded
}
