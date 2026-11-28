//! [`InProcessSiMemory`]: the reference L1 adapter, in the process's memory.
//!
//! It is the double a test and a host with no folder use; it follows the same rules as the
//! in-folder adapter (the scrub, the marker, the bounds, the agent-scope check), so the conformance
//! suite proves the rules, not one adapter's storage.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use adam_core::Timestamp;

use crate::validate::{prepare, prepare_observation};
use crate::{
    AttemptDraft, AttemptId, MemoryError, MemoryResult, Observation, Problem, Scope, Settings,
    SiMemory,
};

/// An L1 memory in a `BTreeMap`, keyed by the scope's canonical key and the root cause.
pub struct InProcessSiMemory {
    settings: Settings,
    scopes: Mutex<BTreeMap<String, BTreeMap<String, Problem>>>,
}

impl InProcessSiMemory {
    /// An empty memory with `settings`' scrub and bounds.
    #[must_use]
    pub fn new(settings: Settings) -> Self {
        Self {
            settings,
            scopes: Mutex::new(BTreeMap::new()),
        }
    }

    /// The settings it was built with.
    #[must_use]
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    fn scopes(&self) -> MutexGuard<'_, BTreeMap<String, BTreeMap<String, Problem>>> {
        self.scopes.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl SiMemory for InProcessSiMemory {
    fn record(&self, scope: &Scope, draft: AttemptDraft) -> MemoryResult<Problem> {
        scope.validate()?;
        let now = Timestamp::now();
        let prepared = prepare(scope, draft, &self.settings, now)?;
        let mut scopes = self.scopes();
        let problems = scopes.entry(scope.key()).or_default();
        if let Some(existing) = problems.get(&prepared.root_cause) {
            if existing
                .attempts
                .iter()
                .any(|attempt| attempt.id == prepared.attempt.id)
            {
                // A retried write lands once: the first one is the record.
                return Ok(existing.clone());
            }
        } else if problems.len() >= self.settings.limits.problems_per_scope {
            return Err(MemoryError::ScopeFull {
                limit: self.settings.limits.problems_per_scope,
            });
        }
        let problem = problems
            .entry(prepared.root_cause.clone())
            .or_insert_with(|| Problem::new(prepared.root_cause, now));
        problem.push(prepared.attempt, &self.settings.limits, now)?;
        Ok(problem.clone())
    }

    fn observe(
        &self,
        scope: &Scope,
        root_cause: &str,
        attempt: &AttemptId,
        observation: Observation,
    ) -> MemoryResult<Problem> {
        scope.validate()?;
        let now = Timestamp::now();
        let observation = prepare_observation(scope, observation, &self.settings)?;
        let mut scopes = self.scopes();
        let problem = scopes
            .get_mut(&scope.key())
            .and_then(|problems| problems.get_mut(root_cause))
            .ok_or_else(|| MemoryError::ProblemNotFound(root_cause.to_owned()))?;
        let stored = problem
            .attempts
            .iter_mut()
            .find(|stored| stored.id == *attempt)
            .ok_or_else(|| {
                MemoryError::AttemptNotFound(attempt.to_string(), root_cause.to_owned())
            })?;
        if stored.observed.is_none() {
            stored.observed = Some(observation);
            stored.observed_at = Some(now);
            problem.updated = now;
        }
        Ok(problem.clone())
    }

    fn recall(&self, scope: &Scope) -> MemoryResult<Vec<Problem>> {
        scope.validate()?;
        let scopes = self.scopes();
        let mut problems: Vec<Problem> = scopes
            .get(&scope.key())
            .map(|problems| problems.values().cloned().collect())
            .unwrap_or_default();
        crate::sort_newest_first(&mut problems);
        Ok(problems)
    }

    fn sweep(&self, scope: &Scope, before: Timestamp) -> MemoryResult<u64> {
        scope.validate()?;
        let mut scopes = self.scopes();
        let Some(problems) = scopes.get_mut(&scope.key()) else {
            return Ok(0);
        };
        let mut removed = 0_u64;
        problems.retain(|_, problem| {
            let kept = problem.attempts.len();
            problem
                .attempts
                .retain(|attempt| attempt.recorded >= before);
            removed += (kept - problem.attempts.len()) as u64;
            !problem.attempts.is_empty()
        });
        Ok(removed)
    }
}
