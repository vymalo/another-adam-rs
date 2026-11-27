//! The values an L1 entry is made of: the scope, the problem, its attempts and the settings.

use std::fmt;
use std::sync::Arc;

use adam_core::{RunId, Timestamp};
use serde::{Deserialize, Serialize};

/// The idempotency key of an attempt: the tool's `<run>/tool:<call id>`, or a caller's own.
///
/// A record with an id the problem already holds is a no-op, so a retried tool step writes one
/// attempt. The model never chooses it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AttemptId(String);

impl AttemptId {
    /// An id. It is not checked here: a write refuses an empty one.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AttemptId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where an attempt came from: the run that wrote it and the run's conversation.
///
/// The paper's provenance is a domain, a benchmark, a model and a round; at task time the run and
/// its conversation are what adam can tell, and they are enough to weigh an attempt later.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Provenance {
    /// The run whose tool step wrote the attempt.
    pub run: RunId,
    /// The run's conversation.
    pub conversation: String,
}

impl Provenance {
    /// The provenance of an attempt written by `run` in `conversation`.
    #[must_use]
    pub fn new(run: RunId, conversation: impl Into<String>) -> Self {
        Self {
            run,
            conversation: conversation.into(),
        }
    }
}

/// What an observer saw: the outcome and a short account of it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Observation {
    /// Whether the attempt worked.
    pub outcome: Outcome,
    /// What was seen, scrubbed like every other text.
    pub detail: String,
}

impl Observation {
    /// An observation.
    #[must_use]
    pub fn new(outcome: Outcome, detail: impl Into<String>) -> Self {
        Self {
            outcome,
            detail: detail.into(),
        }
    }
}

/// Whether an attempt worked. A closed set: a new outcome must fail to compile everywhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The attempt fixed what it predicted, or the run went on.
    Success,
    /// The attempt did not; it is kept, because a failure is evidence too.
    Failure,
}

/// The attempt a caller writes, before the store stamps it.
///
/// The root cause is the problem's identity inside the scope: two attempts with the same scrubbed
/// root cause extend one problem, and a new one opens another.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AttemptDraft {
    /// The short root cause; the problem this attempt belongs to.
    pub root_cause: String,
    /// The idempotency key, `<run>/tool:<call id>`.
    pub id: AttemptId,
    /// What was done.
    pub remedy: String,
    /// Why it was expected to work.
    pub rationale: String,
    /// What it was predicted to fix, written before the outcome.
    pub predicted: String,
    /// Code anchors as text, so a lesson can be judged stale; the crate never reads the code.
    pub anchors: Vec<String>,
    /// The run and conversation that wrote it.
    pub provenance: Provenance,
}

impl AttemptDraft {
    /// A draft for `root_cause` with id `id`, written by `provenance`; every text is empty.
    #[must_use]
    pub fn new(root_cause: impl Into<String>, id: AttemptId, provenance: Provenance) -> Self {
        Self {
            root_cause: root_cause.into(),
            id,
            remedy: String::new(),
            rationale: String::new(),
            predicted: String::new(),
            anchors: Vec::new(),
            provenance,
        }
    }

    /// What was done.
    #[must_use]
    pub fn remedy(mut self, remedy: impl Into<String>) -> Self {
        self.remedy = remedy.into();
        self
    }

    /// Why it was expected to work.
    #[must_use]
    pub fn rationale(mut self, rationale: impl Into<String>) -> Self {
        self.rationale = rationale.into();
        self
    }

    /// What it was predicted to fix, written before the outcome.
    #[must_use]
    pub fn predicted(mut self, predicted: impl Into<String>) -> Self {
        self.predicted = predicted.into();
        self
    }

    /// Code anchors as text.
    #[must_use]
    pub fn anchors(mut self, anchors: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.anchors = anchors.into_iter().map(Into::into).collect();
        self
    }
}

/// One attempt, as stored: the draft's texts after scrubbing, plus when it was recorded and what
/// an observer stamped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Attempt {
    /// The idempotency key.
    pub id: AttemptId,
    /// What was done.
    pub remedy: String,
    /// Why it was expected to work.
    pub rationale: String,
    /// What it was predicted to fix.
    pub predicted: String,
    /// Code anchors as text.
    pub anchors: Vec<String>,
    /// The run and conversation that wrote it.
    pub provenance: Provenance,
    /// When the attempt was recorded.
    pub recorded: Timestamp,
    /// What an observer saw, once.
    pub observed: Option<Observation>,
    /// When the observation was stamped.
    pub observed_at: Option<Timestamp>,
}

/// A problem: a short root cause and the attempts that addressed it.
///
/// A failed attempt is kept. `updated` moves on every write, so a recall serves the freshest
/// problem first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Problem {
    /// The short root cause; the problem's identity inside its scope.
    pub root_cause: String,
    /// The attempts, oldest first.
    pub attempts: Vec<Attempt>,
    /// When the problem was opened.
    pub created: Timestamp,
    /// When it last changed.
    pub updated: Timestamp,
}

impl Problem {
    pub(crate) fn new(root_cause: String, now: Timestamp) -> Self {
        Self {
            root_cause,
            attempts: Vec::new(),
            created: now,
            updated: now,
        }
    }

    pub(crate) fn push(
        &mut self,
        attempt: Attempt,
        limits: &Limits,
        now: Timestamp,
    ) -> crate::MemoryResult<()> {
        if self.attempts.len() >= limits.attempts_per_problem {
            return Err(crate::MemoryError::ProblemFull {
                limit: limits.attempts_per_problem,
            });
        }
        self.attempts.push(attempt);
        self.updated = now;
        Ok(())
    }
}

/// The bounds every L1 scope keeps. The values are ADR 0081 decision 5's, as its agent page
/// records them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Limits {
    /// Problems one scope keeps at most. A further new root cause is refused, not evicted.
    pub problems_per_scope: usize,
    /// Attempts one problem keeps at most. A further attempt is refused.
    pub attempts_per_problem: usize,
    /// Bytes one text field (remedy, rationale, prediction, observation, anchor) keeps at most,
    /// after scrubbing.
    pub field_bytes: usize,
    /// Characters a root cause keeps at most, after scrubbing.
    pub root_cause_chars: usize,
    /// Anchors one attempt keeps at most. The ADR bounds every list but names no count; this
    /// slice's value is 16.
    pub anchors_per_attempt: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            problems_per_scope: 64,
            attempts_per_problem: 16,
            field_bytes: 1024,
            root_cause_chars: 200,
            anchors_per_attempt: 16,
        }
    }
}

/// What replaces a secret where it is removed.
///
/// It is `adam_llm_agent::REDACTED_MARKER`, copied here because `adam-memory` must not depend on
/// `adam-llm-agent` (which will depend on this crate). A test in `adam-llm-agent` asserts the two
/// are equal, so they cannot drift.
pub const REDACTION_MARKER: &str = "[redacted: secret removed, do not reuse]";

/// The scrub every text of a write passes through before it is stored.
///
/// The deployment injects it (the coder's scrubber knows its secrets; a test knows its own), so the
/// crate depends on no redactor and no driver. A text that still holds [`REDACTION_MARKER`] after
/// the scrub is refused: the write fails rather than storing a marker.
#[derive(Clone)]
pub struct Scrubber(Arc<dyn Fn(&str) -> String + Send + Sync>);

impl Scrubber {
    /// A scrubber from a closure.
    #[must_use]
    pub fn new(scrub: impl Fn(&str) -> String + Send + Sync + 'static) -> Self {
        Self(Arc::new(scrub))
    }

    /// `text` with every secret it holds removed.
    #[must_use]
    pub fn scrub(&self, text: &str) -> String {
        (self.0)(text)
    }
}

impl fmt::Debug for Scrubber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Scrubber(<injected>)")
    }
}

/// What an adapter is built with: the injected scrub and the bounds.
///
/// There is no default scrubber on purpose: a deployment that forgot to inject one would store
/// secrets, so an adapter cannot be built without one.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Settings {
    /// The scrub every text passes through before it is stored.
    pub scrubber: Scrubber,
    /// The bounds every write respects.
    pub limits: Limits,
}

impl Settings {
    /// The ADR's bounds, scrubbed by `scrubber`.
    #[must_use]
    pub fn new(scrubber: Scrubber) -> Self {
        Self {
            scrubber,
            limits: Limits::default(),
        }
    }

    /// The same scrubber with other bounds.
    #[must_use]
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }
}
