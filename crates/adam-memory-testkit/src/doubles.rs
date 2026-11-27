//! The doubles the conformance suite must reject.
//!
//! Each one is correct but for one rule, so a case that fails against it names exactly what it
//! proves. [`ScopeBlindSiMemory`] answers the first scope it saw, whatever scope is asked;
//! [`DoubleWritingSiMemory`] writes the attempt twice. The testkit's own tests run the suite's
//! cases against them and assert they fail.

use std::sync::{Mutex, PoisonError};

use adam_core::Timestamp;
use adam_memory::{
    AttemptDraft, AttemptId, InProcessSiMemory, MemoryResult, Observation, Problem, Scope,
    Settings, SiMemory,
};

/// A memory that stores every scope correctly but answers the first scope it saw to every recall.
///
/// It passes every other case (one scope at a time) and fails
/// [`crate::cases::recall_answers_only_its_scope`].
pub struct ScopeBlindSiMemory {
    inner: InProcessSiMemory,
    first: Mutex<Option<Scope>>,
}

impl ScopeBlindSiMemory {
    /// A double with `settings`' scrub and bounds.
    #[must_use]
    pub fn new(settings: Settings) -> Self {
        Self {
            inner: InProcessSiMemory::new(settings),
            first: Mutex::new(None),
        }
    }

    fn first(&self) -> Option<Scope> {
        self.first
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl SiMemory for ScopeBlindSiMemory {
    fn record(&self, scope: &Scope, draft: AttemptDraft) -> MemoryResult<Problem> {
        let problem = self.inner.record(scope, draft)?;
        let mut first = self.first.lock().unwrap_or_else(PoisonError::into_inner);
        if first.is_none() {
            *first = Some(scope.clone());
        }
        Ok(problem)
    }

    fn observe(
        &self,
        scope: &Scope,
        root_cause: &str,
        attempt: &AttemptId,
        observation: Observation,
    ) -> MemoryResult<Problem> {
        self.inner.observe(scope, root_cause, attempt, observation)
    }

    fn recall(&self, scope: &Scope) -> MemoryResult<Vec<Problem>> {
        match self.first() {
            Some(first) => self.inner.recall(&first),
            None => self.inner.recall(scope),
        }
    }

    fn sweep(&self, scope: &Scope, before: Timestamp) -> MemoryResult<u64> {
        self.inner.sweep(scope, before)
    }
}

/// A memory that writes every attempt twice, the second under a derived id, so one record leaves
/// two attempts.
///
/// It passes every other case (they write distinct attempts on purpose) and fails
/// [`crate::cases::a_record_lands_once_for_one_key`].
pub struct DoubleWritingSiMemory {
    inner: InProcessSiMemory,
}

impl DoubleWritingSiMemory {
    /// A double with `settings`' scrub and bounds.
    #[must_use]
    pub fn new(settings: Settings) -> Self {
        Self {
            inner: InProcessSiMemory::new(settings),
        }
    }
}

impl SiMemory for DoubleWritingSiMemory {
    fn record(&self, scope: &Scope, draft: AttemptDraft) -> MemoryResult<Problem> {
        let mut second = draft.clone();
        second.id = AttemptId::new(format!("{}/again", draft.id.as_str()));
        self.inner.record(scope, draft)?;
        self.inner.record(scope, second)
    }

    fn observe(
        &self,
        scope: &Scope,
        root_cause: &str,
        attempt: &AttemptId,
        observation: Observation,
    ) -> MemoryResult<Problem> {
        self.inner.observe(scope, root_cause, attempt, observation)
    }

    fn recall(&self, scope: &Scope) -> MemoryResult<Vec<Problem>> {
        self.inner.recall(scope)
    }

    fn sweep(&self, scope: &Scope, before: Timestamp) -> MemoryResult<u64> {
        self.inner.sweep(scope, before)
    }
}
