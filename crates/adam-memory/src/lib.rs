//! The memories of adam-rs: L1, the self-improvement memory near the agent (the problems it met,
//! the attempts it made and what it saw), and L2, [`Memory`], the lessons that held across L1
//! scopes.
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
//! L2 is [`Memory`]: the lessons that held across L1 scopes, reached over the network and shared
//! across sessions (ADR 0081 decisions 1 and 2). An adapter is configured with one
//! [`MemoryScope`]; within it a lesson is keyed by its [`Audience`] and its root cause, and two
//! scopes on one database never see each other's lessons (decision 4). A lesson is `Active` when
//! served, and `Retired` when its evidence turned: kept, and no longer served (decision 10).
//! [`InProcessMemory`] is the in-process reference; the Redis adapter is
//! `adam-memory-redis`, and the PostgreSQL and MongoDB adapters are features of crates of their
//! own (M2b, ANR-1292 and ANR-1293, not built). An adapter builds on the same rules the reference
//! enforces: `LessonSettings::prepare` stamps and checks a lesson, and
//! [`sort_lessons_newest_first`] orders a recall. The suite is `adam-memory-testkit`'s
//! `memory_conformance!`.
//!
//! ```
//! use adam_memory::{
//!     Audience, InProcessMemory, LessonDraft, LessonProvenance, LessonSettings, Memory,
//!     MemoryScope, Scrubber,
//! };
//!
//! let memory = InProcessMemory::new(
//!     LessonSettings::new(Scrubber::new(|text| text.to_owned())),
//!     MemoryScope::new("release-1"),
//! );
//! let audience = Audience::new("adam-coder");
//! let draft = LessonDraft::new(
//!     audience.clone(),
//!     "the lockfile was stale",
//!     LessonProvenance::new(["scope-a", "scope-b"], "the reviewer"),
//! )
//! .remedy("regenerate the lockfile");
//! memory.record(draft).expect("the write is valid");
//! assert_eq!(memory.recall(&audience).expect("a readable memory").len(), 1);
//! memory
//!     .retire(&audience, "the lockfile was stale")
//!     .expect("the lesson is there");
//! assert!(memory.recall(&audience).expect("a readable memory").is_empty());
//! ```
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
mod lesson;
mod lesson_memory;
mod memory;
mod scope;
mod slug;
mod types;
mod validate;

pub use error::{MemoryError, MemoryResult};
pub use folder::FolderSiMemory;
pub use lesson::{
    Audience, Lesson, LessonDraft, LessonLimits, LessonProvenance, LessonSettings, LessonState,
    MemoryScope, sort_lessons_newest_first,
};
pub use lesson_memory::{InProcessLessonStore, InProcessMemory};
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

/// The L2 memory port: the lessons that held across L1 scopes, shared across sessions.
///
/// An adapter is configured with one [`MemoryScope`] and never serves another's lessons; within
/// it, a lesson is keyed by its [`Audience`] and its root cause (ADR 0081 decision 4). A lesson is
/// reached over the network — a database is L2 even when one agent alone uses it — so this port
/// carries no folder and no local file (decision 2).
///
/// A new required method breaks every implementer, so say so in the pull request and the crate
/// README (CLAUDE.md rule 2).
pub trait Memory: Send + Sync + 'static {
    /// Record a lesson the reviewer approved, or update the one its audience already holds under
    /// the same root cause.
    ///
    /// Every lesson text is scrubbed first; the audience is a key, validated and bounded, never
    /// scrubbed; the provenance labels are scrubbed and bounded. A text that still holds
    /// [`REDACTION_MARKER`], a text over a bound, a text that names a repository, a path, a URL, a
    /// host or an e-mail address, or a write that would make the audience *serve* more than the
    /// limit is refused, and nothing is written. A second record for a key the audience holds
    /// updates it and keeps its creation time, so a retried approval is one lesson; a record for a
    /// retired lesson serves it again when a free slot remains.
    ///
    /// # Errors
    ///
    /// [`MemoryError`], whose class says whether to retry (a backend failure) or not (a refusal).
    fn record(&self, draft: LessonDraft) -> MemoryResult<Lesson>;

    /// The audience's active lessons, the freshest first, and no other audience's or scope's.
    ///
    /// A retired lesson is kept and not served.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Corrupt`] when a stored lesson does not parse, or a backend failure.
    fn recall(&self, audience: &Audience) -> MemoryResult<Vec<Lesson>>;

    /// Retire the lesson its audience holds under `root_cause`, and return it as stored.
    ///
    /// `root_cause` is the lesson's stored root cause, as [`record`](Self::record) returned it —
    /// the scrubbed text. A root cause the scrub changed does not find the lesson, so the adapter
    /// answers [`MemoryError::LessonNotFound`]. Retiring a retired lesson is a no-op that returns
    /// the same record, so a retried step lands once. Nothing is deleted: a retired lesson is kept
    /// (ADR 0081 decision 10).
    ///
    /// # Errors
    ///
    /// [`MemoryError::LessonNotFound`], or a backend failure.
    fn retire(&self, audience: &Audience, root_cause: &str) -> MemoryResult<Lesson>;
}

/// A [`Memory`] held behind an `Arc`: what a host or a tool is given.
pub type DynMemory = Arc<dyn Memory>;

impl<T: Memory + ?Sized> Memory for Arc<T> {
    fn record(&self, draft: LessonDraft) -> MemoryResult<Lesson> {
        (**self).record(draft)
    }

    fn recall(&self, audience: &Audience) -> MemoryResult<Vec<Lesson>> {
        (**self).recall(audience)
    }

    fn retire(&self, audience: &Audience, root_cause: &str) -> MemoryResult<Lesson> {
        (**self).retire(audience, root_cause)
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
