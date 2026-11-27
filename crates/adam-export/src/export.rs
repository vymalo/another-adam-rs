//! The driver: list an agent's runs, join each one, write the records that pass.

use std::io::Write;

use adam_core::store::{ConversationScope, RunQuery, RunRecord};
use adam_error::{Classify, ErrorClass};
use adam_llm_agent::Conversation;
use adam_runtime::{Runtime, RuntimeError, view_from_record};

use crate::{ExportOptions, Filter, Record, RunMeta, record_of};

/// Runs read per listing.
const PAGE: usize = 100;

/// What an export did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Summary {
    /// Runs listed.
    pub seen: u64,
    /// Records written.
    pub written: u64,
    /// Runs the filter refused.
    pub filtered: u64,
    /// Runs whose state is not a conversation with messages (another kind of agent, or a run
    /// that never started).
    pub not_a_conversation: u64,
    /// Runs whose state could not be read; each one is logged with its id.
    pub unreadable: u64,
}

impl Summary {
    fn add(&mut self, other: Summary) {
        self.seen += other.seen;
        self.written += other.written;
        self.filtered += other.filtered;
        self.not_a_conversation += other.not_a_conversation;
        self.unreadable += other.unreadable;
    }
}

/// Why an export stopped.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    /// The store refused a listing.
    #[error("listing the runs failed")]
    Runtime(#[from] RuntimeError),
    /// The sink refused a record.
    #[error("writing a record failed")]
    Write(#[from] std::io::Error),
    /// A record did not serialize (it cannot: a record is plain data).
    #[error("serializing a record failed")]
    Json(#[from] serde_json::Error),
}

impl Classify for ExportError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::Runtime(e) => e.class(),
            Self::Write(_) => ErrorClass::Transient,
            Self::Json(_) => ErrorClass::Internal,
        }
    }
}

/// Write `record` as one line of JSON.
///
/// # Errors
///
/// The sink's.
pub fn write_record(out: &mut impl Write, record: &Record) -> Result<(), ExportError> {
    serde_json::to_writer(&mut *out, record)?;
    out.write_all(b"\n")?;
    Ok(())
}

/// Export the runs of each of `agents` that have a conversation, newest first, as JSON Lines to
/// `out`.
///
/// The runs are listed with [`Store::list_runs`](adam_core::Store::list_runs) and joined with
/// [`Runtime::state`], so each agent must be registered on `runtime` (its `join` reads a v5
/// run's transcript). A run whose state cannot be read is counted and logged, and the export
/// goes on; a listing or a write that fails stops it.
///
/// # Errors
///
/// [`ExportError`].
pub async fn export_runs(
    runtime: &Runtime,
    agents: &[String],
    filter: &Filter,
    options: &ExportOptions,
    out: &mut impl Write,
) -> Result<Summary, ExportError> {
    let mut summary = Summary::default();
    for agent in agents {
        summary.add(export_agent(runtime, agent, filter, options, out).await?);
    }
    out.flush()?;
    Ok(summary)
}

async fn export_agent(
    runtime: &Runtime,
    agent: &str,
    filter: &Filter,
    options: &ExportOptions,
    out: &mut impl Write,
) -> Result<Summary, ExportError> {
    let mut summary = Summary::default();
    let mut query = RunQuery::new(agent, ConversationScope::Prefix(String::new()), PAGE);
    query.statuses = (!filter.statuses.is_empty()).then(|| filter.statuses.clone());
    query.updated_since = filter.since;
    loop {
        let page = runtime
            .store()
            .list_runs(&query)
            .await
            .map_err(RuntimeError::from)?;
        let Some(last) = page.last() else {
            break;
        };
        query.after = Some((last.updated_at, last.id));
        let full = page.len() == PAGE;
        for record in page {
            summary.seen += 1;
            if !filter.admits(record.status, record.updated_at) {
                summary.filtered += 1;
                continue;
            }
            match read(runtime, record).await {
                Ok(Some((meta, conversation))) => {
                    let exported = record_of(&meta, &conversation, options);
                    if filter.accepts(&exported) {
                        write_record(out, &exported)?;
                        summary.written += 1;
                    } else {
                        summary.filtered += 1;
                    }
                }
                Ok(None) => summary.not_a_conversation += 1,
                Err((run, e)) => {
                    tracing::warn!(%run, error = %adam_error::report(&e), "a run's state could not be read; it is not exported");
                    summary.unreadable += 1;
                }
            }
        }
        if !full {
            break;
        }
    }
    Ok(summary)
}

/// The bookkeeping and the conversation of `record`, or `None` when its state is not a
/// conversation with messages.
async fn read(
    runtime: &Runtime,
    record: RunRecord,
) -> Result<Option<(RunMeta, Conversation)>, (adam_core::RunId, RuntimeError)> {
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
    Ok(Some((RunMeta::of(&record, error), conversation)))
}
