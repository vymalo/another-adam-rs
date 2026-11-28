//! Where runs come from: the [`RunSource`] port. A source lists the runs of one agent a page at a
//! time and reads each one into a [`SourceRun`]; what it lists and cannot yield it counts
//! ([`Skipped`]). [`RuntimeSource`](crate::RuntimeSource) is the adapter over any `Store`.

use adam_core::store::RunStatus;
use adam_core::time::Timestamp;
use adam_error::{BoxError, Classify, ErrorClass};
use adam_llm_agent::Conversation;
use adam_runtime::RuntimeError;
use async_trait::async_trait;

use crate::{Filter, RunMeta, TurnRequest};

/// Where an export reads runs from: a store, a dump, another deployment's archive.
///
/// The exporter asks for the runs of each of [`agents`](Self::agents) by pages
/// ([`page`](Self::page)), passing back the [`Cursor`] of the page before, until a page has no
/// `next`. The suite `source_conformance!` (`adam-export-testkit`) checks an implementation.
///
/// The contract:
///
/// * the pages of one query are **disjoint and cover every run once**, in an order the source
///   keeps from one page to the next (a `Store` lists newest first);
/// * a page **lists** at most [`SourceQuery::limit`] runs, the yielded and the skipped together;
/// * a run is **yielded** only when it is of the query's agent, admitted by its statuses and
///   times ([`SourceQuery::admits`]), and its state is a conversation with messages; every other
///   run the page lists is **counted** in [`Skipped`], never yielded. A source may leave out of
///   its listing what its backend filters itself (then it is neither yielded nor counted);
/// * a [`Cursor`] is read only by the source that made it, and the same cursor answers the same
///   page while the runs do not change;
/// * an error stops the export: a run the source cannot read is [`Skipped::unreadable`], not an
///   error.
#[async_trait]
pub trait RunSource: Send + Sync {
    /// The operator's name for this source, written as each record's `source` (`prod`,
    /// `eu-archive`): a name the operator chose, never a host, a URL or a credential. `None` for
    /// an unnamed source, which the exporter allows only when it reads one source.
    fn label(&self) -> Option<&str>;

    /// The agents whose runs this source exports, in the order they are exported.
    fn agents(&self) -> &[String];

    /// The page of `query` that starts after `after` (the first page when `None`).
    ///
    /// # Errors
    ///
    /// [`SourceError`]: the backend refused the listing, or `after` is not a cursor of this source.
    async fn page(
        &self,
        query: &SourceQuery,
        after: Option<&Cursor>,
    ) -> Result<SourcePage, SourceError>;
}

/// Which runs of one agent a page lists: the part of a [`Filter`] that is decided before a run's
/// state is read.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct SourceQuery {
    /// The agent whose runs are listed.
    pub agent: String,
    /// Only runs in one of these statuses; empty for every status.
    pub statuses: Vec<RunStatus>,
    /// Only runs last updated at or after this instant.
    pub since: Option<Timestamp>,
    /// Only runs last updated before this instant.
    pub until: Option<Timestamp>,
    /// The most runs a page lists, the skipped ones included; at least 1.
    pub limit: usize,
}

impl SourceQuery {
    /// Every run of `agent`, up to `limit` a page (raised to 1).
    pub fn new(agent: impl Into<String>, limit: usize) -> Self {
        Self {
            agent: agent.into(),
            statuses: Vec::new(),
            since: None,
            until: None,
            limit: limit.max(1),
        }
    }

    /// The runs of `agent` that `filter` admits before their state is read
    /// ([`Filter::admits`]), up to `limit` a page.
    pub fn of(agent: impl Into<String>, filter: &Filter, limit: usize) -> Self {
        Self {
            statuses: filter.statuses.clone(),
            since: filter.since,
            until: filter.until,
            ..Self::new(agent, limit)
        }
    }

    /// Only runs in `statuses` (empty for every status).
    #[must_use]
    pub fn statuses(mut self, statuses: impl IntoIterator<Item = RunStatus>) -> Self {
        self.statuses = statuses.into_iter().collect();
        self
    }

    /// Only runs last updated at or after `since`.
    #[must_use]
    pub fn since(mut self, since: Option<Timestamp>) -> Self {
        self.since = since;
        self
    }

    /// Only runs last updated before `until`.
    #[must_use]
    pub fn until(mut self, until: Option<Timestamp>) -> Self {
        self.until = until;
        self
    }

    /// Whether a run of this agent with `status`, last updated at `updated_at`, is in the query.
    pub fn admits(&self, status: RunStatus, updated_at: Timestamp) -> bool {
        crate::filter::within(&self.statuses, self.since, self.until, status, updated_at)
    }
}

/// A position in a source's listing: where the next page starts. Opaque: only the source that
/// made it reads it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Cursor(String);

impl Cursor {
    /// A cursor of `text`, for a source to make.
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// Its text, for the source that made it.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One page of a listing.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct SourcePage {
    /// The runs read, in the source's order.
    pub runs: Vec<SourceRun>,
    /// The runs listed and not yielded, by why.
    pub skipped: Skipped,
    /// Where the next page starts; `None` when this page is the last.
    pub next: Option<Cursor>,
}

impl SourcePage {
    /// A page of `runs` and `skipped`, followed by `next`.
    pub fn new(runs: Vec<SourceRun>, skipped: Skipped, next: Option<Cursor>) -> Self {
        Self {
            runs,
            skipped,
            next,
        }
    }

    /// The runs the page listed: yielded and skipped.
    pub fn listed(&self) -> u64 {
        u64::try_from(self.runs.len()).unwrap_or(u64::MAX) + self.skipped.total()
    }
}

/// One run a source read: what the conversion and the samples need (`record_with`,
/// `samples_of`).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct SourceRun {
    /// Its bookkeeping, without its state.
    pub meta: RunMeta,
    /// Its state, a conversation with messages.
    pub conversation: Conversation,
    /// The prompts and tools its model calls were sent, as the run recorded them, oldest first:
    /// the last entry is the last call's. Empty when it recorded none, or a blob is gone.
    pub requests: Vec<TurnRequest>,
}

impl SourceRun {
    /// The run `meta` whose state is `conversation`, with its recorded `requests`.
    pub fn new(meta: RunMeta, conversation: Conversation, requests: Vec<TurnRequest>) -> Self {
        Self {
            meta,
            conversation,
            requests,
        }
    }
}

/// The runs a page listed and did not yield, by why.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Skipped {
    /// Listed, but outside the query: a part of it the backend could not filter (a `Store` lists
    /// by `since` and not by `until`). The exporter counts them as filtered.
    pub outside_query: u64,
    /// Read, and their state is not a conversation with messages (another kind of agent, or a
    /// run that never started).
    pub not_a_conversation: u64,
    /// Their state could not be read; the source logs each one with its id.
    pub unreadable: u64,
}

impl Skipped {
    /// Every run counted.
    pub fn total(&self) -> u64 {
        self.outside_query + self.not_a_conversation + self.unreadable
    }
}

/// Why a source could not answer a page.
///
/// No backend type appears here: an adapter boxes its backend's error as the `source` of
/// [`Backend`](Self::Backend) after choosing its class, the way a `Store` does.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SourceError {
    /// The cursor is not one this source made.
    #[error("the cursor {0:?} is not one this source made")]
    Cursor(String),
    /// The backend refused the listing. The adapter that boxed `source` chose `class`.
    #[error("the source's backend failed")]
    Backend {
        /// What to do about it.
        class: ErrorClass,
        /// The backend's own error.
        #[source]
        source: BoxError,
    },
}

impl SourceError {
    /// The backend's `source`, of class `class`.
    pub fn backend(class: ErrorClass, source: impl Into<BoxError>) -> Self {
        Self::Backend {
            class,
            source: source.into(),
        }
    }
}

/// A runtime's error keeps its class (a store's passes through it).
impl From<RuntimeError> for SourceError {
    fn from(e: RuntimeError) -> Self {
        Self::backend(e.class(), e)
    }
}

impl Classify for SourceError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::Cursor(_) => ErrorClass::Invalid,
            Self::Backend { class, .. } => *class,
        }
    }
}
