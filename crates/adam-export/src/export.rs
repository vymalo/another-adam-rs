//! The driver: list an agent's runs, join each one, write the records that pass.

use std::io::Write;

use adam_core::store::{ConversationScope, RunQuery, RunRecord};
use adam_error::{Classify, ErrorClass};
use adam_llm_agent::Conversation;
use adam_runtime::{Runtime, RuntimeError, view_from_record};

use crate::{
    ExportOptions, Filter, Record, RequestShape, RunMeta, Samples, TurnRequest, estimated_tokens,
    record_with, samples_of, turns_lost_to_compaction,
};

/// Runs read per listing.
const PAGE: usize = 100;

/// What an export did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Summary {
    /// Runs listed.
    pub seen: u64,
    /// Records written: runs, or samples with [`Samples::Turn`].
    pub written: u64,
    /// Runs the filter refused.
    pub filtered: u64,
    /// Records left out because their estimated tokens are over
    /// [`ExportOptions::max_tokens`]: runs, or samples with [`Samples::Turn`].
    pub over_budget: u64,
    /// With [`Samples::Turn`]: the turns of the runs written before their last compaction, which
    /// have no sample ([`turns_lost_to_compaction`]).
    pub turns_lost_to_compaction: u64,
    /// With [`Samples::Turn`]: the samples written with `exact: false`.
    pub inexact: u64,
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
        self.over_budget += other.over_budget;
        self.turns_lost_to_compaction += other.turns_lost_to_compaction;
        self.inexact += other.inexact;
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
/// `out`: one record per run, or one per assistant turn with [`ExportOptions::samples`]
/// ([`samples_of`]; the samples of a run oldest first). A record over
/// [`ExportOptions::max_tokens`] is left out and counted.
///
/// With [`Samples::Turn`] a compacted run is kept whatever [`Filter::include_compacted`] says:
/// its samples start at its last compaction, so they are what the model was sent.
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
    // Per turn, a compacted run is kept: its samples start at its last compaction.
    let turn_filter = Filter {
        include_compacted: true,
        ..filter.clone()
    };
    let filter_of_record = match options.samples {
        Samples::Run => filter,
        Samples::Turn => &turn_filter,
    };
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
                Ok(Some((meta, conversation, recorded))) => {
                    let exported = record_with(
                        &meta,
                        &conversation,
                        // The last call's, as `Conversation::request` names it: none when that
                        // call recorded nothing, never an earlier call's.
                        conversation
                            .request
                            .as_ref()
                            .and(recorded.last())
                            .map(|entry| &entry.shape),
                        options,
                    );
                    if !filter_of_record.accepts(&exported) {
                        summary.filtered += 1;
                        continue;
                    }
                    let records = match options.samples {
                        Samples::Run => vec![exported],
                        Samples::Turn => {
                            summary.turns_lost_to_compaction +=
                                u64::from(turns_lost_to_compaction(&conversation));
                            samples_of(&meta, &conversation, &recorded, options)
                        }
                    };
                    for record in records {
                        if options
                            .max_tokens
                            .is_some_and(|max| estimated_tokens(&record) > u64::from(max))
                        {
                            summary.over_budget += 1;
                            continue;
                        }
                        if record.sample.is_some_and(|sample| !sample.exact) {
                            summary.inexact += 1;
                        }
                        write_record(out, &record)?;
                        summary.written += 1;
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

/// What [`read`] answers: the run's bookkeeping, its conversation, and the prompts and tools its
/// model calls were sent, as the run recorded them (empty when it recorded none, or a blob is
/// gone); the last entry is the last call's.
type Read = (RunMeta, Conversation, Vec<TurnRequest>);

/// The bookkeeping, the conversation and the recorded request of `record`, or `None` when its
/// state is not a conversation with messages.
async fn read(
    runtime: &Runtime,
    record: RunRecord,
) -> Result<Option<Read>, (adam_core::RunId, RuntimeError)> {
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
    let recorded = recorded_requests(runtime, run, &conversation).await;
    Ok(Some((RunMeta::of(&record, error), conversation, recorded)))
}

/// The requests `conversation` recorded, read from the store: its distinct requests
/// (`Conversation::requests`), or, for a run recorded before the list existed, its last call's
/// (`Conversation::request`). All or none: a request whose bytes were collected, or that does not
/// read, would stand for the calls of another, so then none is used, and the records fall back to
/// the current shape or to none and say so in `tools_source`.
async fn recorded_requests(
    runtime: &Runtime,
    run: adam_core::RunId,
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
