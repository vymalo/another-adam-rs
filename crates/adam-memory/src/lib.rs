//! The L1 self-improvement memory: the problems an agent met, the attempts it made, and what it saw.
//!
//! [ADR 0081](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0081-an-agent-keeps-a-self-improvement-memory-near-it-and-what-holds-becomes-the-deployments-memory.md)
//! decision 2 to 5: L1 is every memory next to the agent, and its only adapter is a folder of plain
//! files the deployment gives it. An entry is a **problem** (a short root cause) with its
//! **attempts**: the remedy, the rationale, a prediction written before the outcome, the
//! observation stamped after it, and provenance. A failed attempt is kept.
//!
//! L1 is keyed by [`Scope`]: deployment, caller, agent and [`Kind`] (`agent`, or one repository).
//! No entry crosses a caller. Every text is scrubbed by the injected [`Scrubber`] before it is
//! written; a text that still holds [`REDACTION_MARKER`] is refused, not stored. Every list is
//! bounded by [`Limits`], and an agent-scoped entry may not name a repository, a path, a URL, a
//! host or an e-mail address.
//!
//! The port is [`SiMemory`]; [`FolderSiMemory`] is L1's only adapter and [`InProcessSiMemory`] is
//! the in-process reference. The conformance suite is `adam-memory-testkit`'s
//! [`si_memory_conformance!`](../adam-memory-testkit/README.md).
//!
//! ```
//! use adam_core::RunId;
//! use adam_memory::{
//!     AttemptDraft, AttemptId, InProcessSiMemory, Observation, Outcome, Provenance, Scope,
//!     Scrubber, Settings, SiMemory,
//! };
//!
//! let memory = InProcessSiMemory::new(Settings::new(Scrubber::new(|text| text.to_owned())));
//! let scope = Scope::agent("deployment", "caller", "adam-coder");
//! let draft = AttemptDraft::new(
//!     "the lockfile was stale",
//!     AttemptId::new("run-1/tool:call-1"),
//!     Provenance::new(RunId::new(), "conversation-1"),
//! )
//! .remedy("regenerate the lockfile")
//! .predicted("the build resolves its dependencies");
//! memory.record(&scope, draft).expect("the write is valid");
//! let problem = &memory.recall(&scope).expect("a readable folder")[0];
//! let attempt = &problem.attempts[0];
//! memory
//!     .observe(
//!         &scope,
//!         &problem.root_cause,
//!         &attempt.id,
//!         Observation::new(Outcome::Success, "the build passed"),
//!     )
//!     .expect("the attempt is there");
//! assert_eq!(memory.recall(&scope).expect("a readable folder")[0].attempts[0].observed.as_ref().expect("an observation").outcome, Outcome::Success);
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod error;
mod folder;
mod memory;
mod scope;
mod slug;
mod types;
mod validate;

pub use error::{MemoryError, MemoryResult};
pub use folder::FolderSiMemory;
pub use memory::InProcessSiMemory;
pub use scope::{Kind, Repository, Scope};
pub use types::{
    Attempt, AttemptDraft, AttemptId, Limits, Observation, Outcome, Problem, Provenance,
    REDACTION_MARKER, Scrubber, Settings,
};

use std::sync::Arc;

use adam_core::Timestamp;

/// The L1 self-improvement memory port.
///
/// An adapter keeps one L1: a folder the deployment gave the agent, or a process's memory for a
/// test. It never opens a network connection; a store reached over the network is L2 (ADR 0081
/// decision 2).
///
/// A new required method breaks every implementer, so say so in the pull request and the crate
/// README (CLAUDE.md rule 2).
pub trait SiMemory: Send + Sync + 'static {
    /// Open the problem named by `draft.root_cause`, or extend it with the draft's attempt.
    ///
    /// Every text is scrubbed first. A text that still holds [`REDACTION_MARKER`], a text over a
    /// bound, a new root cause in a full scope, or a further attempt on a full problem is refused
    /// and nothing is written. A second record whose attempt id the problem already holds is a
    /// no-op: a retried tool step writes one attempt.
    ///
    /// # Errors
    ///
    /// [`MemoryError`], whose class says whether to retry (a backend failure) or not (a refusal).
    fn record(&self, scope: &Scope, draft: AttemptDraft) -> MemoryResult<Problem>;

    /// Stamp what an observer saw for one attempt.
    ///
    /// `root_cause` is the problem's stored root cause, as [`record`](Self::record) returned it
    /// (the scrubbed text). The first observation is the record: a second one is ignored, so a
    /// replayed event does not move the outcome. The observation's text is scrubbed like every
    /// other.
    ///
    /// # Errors
    ///
    /// [`MemoryError::ProblemNotFound`], [`MemoryError::AttemptNotFound`], or a refusal.
    fn observe(
        &self,
        scope: &Scope,
        root_cause: &str,
        attempt: &AttemptId,
        observation: Observation,
    ) -> MemoryResult<Problem>;

    /// The scope's problems, the freshest first, and no other scope's.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Corrupt`] when a stored problem does not parse, or a backend failure.
    fn recall(&self, scope: &Scope) -> MemoryResult<Vec<Problem>>;

    /// Remove the attempts recorded before `before`, and the problems they leave empty; return how
    /// many attempts went.
    ///
    /// # Errors
    ///
    /// A backend failure.
    fn sweep(&self, scope: &Scope, before: Timestamp) -> MemoryResult<u64>;
}

/// A [`SiMemory`] held behind an `Arc`: what a host or a tool is given.
pub type DynSiMemory = Arc<dyn SiMemory>;

impl<T: SiMemory + ?Sized> SiMemory for Arc<T> {
    fn record(&self, scope: &Scope, draft: AttemptDraft) -> MemoryResult<Problem> {
        (**self).record(scope, draft)
    }

    fn observe(
        &self,
        scope: &Scope,
        root_cause: &str,
        attempt: &AttemptId,
        observation: Observation,
    ) -> MemoryResult<Problem> {
        (**self).observe(scope, root_cause, attempt, observation)
    }

    fn recall(&self, scope: &Scope) -> MemoryResult<Vec<Problem>> {
        (**self).recall(scope)
    }

    fn sweep(&self, scope: &Scope, before: Timestamp) -> MemoryResult<u64> {
        (**self).sweep(scope, before)
    }
}

/// The order a recall serves: the freshest problem first, then by root cause so the answer is
/// stable.
pub(crate) fn sort_newest_first(problems: &mut [Problem]) {
    problems.sort_by(|a, b| {
        b.updated
            .cmp(&a.updated)
            .then_with(|| a.root_cause.cmp(&b.root_cause))
    });
}
