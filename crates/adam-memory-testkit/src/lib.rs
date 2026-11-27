//! The conformance suite of [`adam_memory::SiMemory`], and the [`Plan`] a test writes with.
//!
//! [`si_memory_conformance!`] writes the cases of ADR 0081 decisions 3 to 5 into a module of the
//! implementing crate:
//!
//! * a problem and its attempt round-trip, and attempts with one root cause share a problem;
//! * a record lands once for one attempt key, however often it is retried;
//! * a scope answers only its own entries, never another scope's;
//! * a text is scrubbed before it is stored, and a text that still holds the redaction marker is
//!   refused;
//! * every bound refuses the write it would break, and a sweep frees what it removes;
//! * an agent scope refuses a repository, a path, a URL, a host and an e-mail address, while a
//!   repository scope keeps them;
//! * an observation is stamped once and a failed attempt is kept;
//! * sixteen writers on one scope leave sixteen problems.
//!
//! Each case is a function here, so the suite is compiled once and the macro only names it.
//!
//! ```ignore
//! use std::sync::Arc;
//!
//! use adam_memory::{DynSiMemory, InProcessSiMemory};
//! use adam_memory_testkit::Plan;
//!
//! fn make(plan: &Plan) -> DynSiMemory {
//!     Arc::new(InProcessSiMemory::new(plan.settings.clone()))
//! }
//!
//! adam_memory_testkit::si_memory_conformance!(crate::make);
//! ```
//!
//! `$make` is a path to a `fn(&Plan) -> DynSiMemory`. The suite also ships two doubles the cases
//! must reject: [`doubles::ScopeBlindSiMemory`] and [`doubles::DoubleWritingSiMemory`].

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::unwrap_used, clippy::expect_used)] // a test kit asserts by unwrapping

pub mod doubles;

use std::time::Duration;

use adam_core::RunId;
use adam_core::time::span_of;
use adam_memory::{
    AttemptDraft, AttemptId, Limits, MemoryError, Observation, Outcome, Problem, Provenance,
    REDACTION_MARKER, Repository, Scope, Scrubber, Settings, SiMemory,
};

pub use adam_memory::DynSiMemory;

/// A secret the suite's default scrubber replaces with [`REDACTION_MARKER`], so a text that holds
/// it is refused.
pub const SECRET: &str = "conformance-secret";

/// A token the suite's default scrubber removes without leaving the marker, so a case can prove a
/// text is scrubbed and then stored.
pub const TOKEN: &str = "conformance-token";

/// What one conformance run writes with: the injected scrub and the bounds.
///
/// `Plan::default()` scrubs [`SECRET`] into the marker (a refusal) and removes [`TOKEN`] (a
/// scrub), with the ADR's bounds.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Plan {
    /// The scrub and bounds the `$make` adapter is built with.
    pub settings: Settings,
}

impl Plan {
    /// A plan with `scrubber` and the ADR's bounds.
    #[must_use]
    pub fn new(scrubber: Scrubber) -> Self {
        Self {
            settings: Settings::new(scrubber),
        }
    }

    /// A plan with `scrubber` and other bounds.
    #[must_use]
    pub fn with_limits(scrubber: Scrubber, limits: Limits) -> Self {
        Self {
            settings: Settings::new(scrubber).with_limits(limits),
        }
    }
}

impl Default for Plan {
    fn default() -> Self {
        Self::new(Scrubber::new(|text| {
            text.replace(SECRET, REDACTION_MARKER).replace(TOKEN, "")
        }))
    }
}

/// An agent scope of the suite's deployment and caller.
#[must_use]
pub fn scope(agent: &str) -> Scope {
    Scope::agent("conformance", "caller", agent)
}

/// A repository scope of the suite's deployment and caller.
#[must_use]
pub fn repository_scope(agent: &str) -> Scope {
    Scope::repository(
        "conformance",
        "caller",
        agent,
        Repository::new("github.com", "vymalo", "another-adam-rs"),
    )
}

/// A draft with one remedy, a rationale and a prediction, for `root_cause` and attempt `id`.
#[must_use]
pub fn draft(root_cause: &str, id: &str) -> AttemptDraft {
    AttemptDraft::new(
        root_cause,
        AttemptId::new(id),
        Provenance::new(RunId::new(), "conformance"),
    )
    .remedy("restart the worker")
    .rationale("the lease had expired")
    .predicted("the run resumes")
}

/// The cases, as plain functions for harnesses that do not use the macro.
pub mod cases {
    use super::*;

    fn record(store: &DynSiMemory, scope: &Scope, root_cause: &str, id: &str) -> Problem {
        store
            .record(scope, draft(root_cause, id))
            .expect("a valid record is stored")
    }

    /// A problem and its attempt come back as they were written.
    ///
    /// # Panics
    ///
    /// When a field of the stored attempt differs from the draft's.
    pub fn record_then_recall_round_trips(store: DynSiMemory, _plan: Plan) {
        let scope = scope("round-trip");
        let written = store
            .record(
                &scope,
                draft("the lockfile is stale", "run/tool:1")
                    .remedy("regenerate the lockfile")
                    .rationale("the manifest changed")
                    .predicted("the build resolves its dependencies")
                    .anchors(["the dependency block of the manifest"]),
            )
            .unwrap();
        assert_eq!(written.root_cause, "the lockfile is stale");
        assert_eq!(written.attempts.len(), 1);
        let attempt = &written.attempts[0];
        assert_eq!(attempt.id, AttemptId::new("run/tool:1"));
        assert_eq!(attempt.remedy, "regenerate the lockfile");
        assert_eq!(attempt.rationale, "the manifest changed");
        assert_eq!(attempt.predicted, "the build resolves its dependencies");
        assert_eq!(attempt.anchors, ["the dependency block of the manifest"]);
        assert_eq!(attempt.provenance.conversation, "conformance");
        assert!(attempt.observed.is_none());
        let recalled = store.recall(&scope).unwrap();
        assert_eq!(recalled, vec![written]);
    }

    /// Attempts that name one root cause extend one problem; a failed one is kept.
    ///
    /// # Panics
    ///
    /// When the same root cause opens two problems, or an attempt is lost.
    pub fn attempts_with_one_root_cause_share_a_problem(store: DynSiMemory, _plan: Plan) {
        let scope = scope("one-problem");
        record(&store, &scope, "the tests time out", "run/tool:1");
        record(&store, &scope, "the tests time out", "run/tool:2");
        let recalled = store.recall(&scope).unwrap();
        assert_eq!(recalled.len(), 1, "one root cause is one problem");
        assert_eq!(recalled[0].attempts.len(), 2, "both attempts are kept");
        assert_eq!(recalled[0].attempts[0].id.as_str(), "run/tool:1");
        assert_eq!(recalled[0].attempts[1].id.as_str(), "run/tool:2");
    }

    /// A record with an attempt key the problem already holds writes one attempt.
    ///
    /// # Panics
    ///
    /// When a retried record writes a second attempt.
    pub fn a_record_lands_once_for_one_key(store: DynSiMemory, _plan: Plan) {
        let scope = scope("idempotent");
        let draft = draft("the worker dies on a large input", "run/tool:1");
        store.record(&scope, draft.clone()).unwrap();
        store.record(&scope, draft).unwrap();
        let recalled = store.recall(&scope).unwrap();
        assert_eq!(recalled.len(), 1);
        assert_eq!(
            recalled[0].attempts.len(),
            1,
            "a record writes one attempt for one key"
        );
    }

    /// A scope answers only its own entries: another agent's, another caller's, another kind's.
    ///
    /// # Panics
    ///
    /// When a recall serves another scope's problem.
    pub fn recall_answers_only_its_scope(store: DynSiMemory, _plan: Plan) {
        let a = scope("scope-a");
        let b = scope("scope-b");
        let repository = repository_scope("scope-a");
        let other_caller = Scope::agent("conformance", "another-caller", "scope-a");
        record(&store, &a, "the first problem", "a/tool:1");
        record(&store, &b, "the second problem", "b/tool:1");
        record(&store, &repository, "the third problem", "r/tool:1");
        record(&store, &other_caller, "the fourth problem", "c/tool:1");

        let only = |scope: &Scope, root_cause: &str| {
            let recalled = store.recall(scope).unwrap();
            assert_eq!(
                recalled.len(),
                1,
                "{:?} must answer one problem, not {recalled:?}",
                scope.agent_name()
            );
            assert_eq!(recalled[0].root_cause, root_cause);
        };
        only(&a, "the first problem");
        only(&b, "the second problem");
        only(&repository, "the third problem");
        only(&other_caller, "the fourth problem");
    }

    /// A text is scrubbed before it is stored.
    ///
    /// # Panics
    ///
    /// When the stored text still holds the token the plan scrubs.
    pub fn a_text_is_scrubbed_before_the_write(store: DynSiMemory, _plan: Plan) {
        let scope = scope("scrubbed");
        let written = store
            .record(
                &scope,
                draft("the command failed", "s/tool:1")
                    .remedy(format!("retry with {TOKEN} and report")),
            )
            .unwrap();
        assert_eq!(written.attempts[0].remedy, "retry with  and report");
        assert!(!written.attempts[0].remedy.contains(TOKEN));
    }

    /// A text that still holds the redaction marker after the scrub is refused, and nothing is
    /// stored.
    ///
    /// # Panics
    ///
    /// When the write succeeds, or the refusal stores a problem.
    pub fn a_text_carrying_the_marker_is_refused(store: DynSiMemory, _plan: Plan) {
        let scope = scope("marker");
        let error = store
            .record(
                &scope,
                draft("the command failed", "m/tool:1").remedy(format!("retry with {SECRET}")),
            )
            .expect_err("a text the scrub marked is refused");
        assert!(matches!(error, MemoryError::Redacted { .. }), "{error}");
        let error = store
            .record(
                &scope,
                draft("the command failed", "m/tool:2").predicted(REDACTION_MARKER.to_owned()),
            )
            .expect_err("a text that already holds the marker is refused");
        assert!(matches!(error, MemoryError::Redacted { .. }), "{error}");
        assert!(
            store.recall(&scope).unwrap().is_empty(),
            "a refused write stores nothing"
        );
    }

    /// A field over its byte bound is refused.
    ///
    /// # Panics
    ///
    /// When the write succeeds.
    pub fn a_field_over_the_bound_is_refused(store: DynSiMemory, plan: Plan) {
        let scope = scope("field-bound");
        let long = "a".repeat(plan.settings.limits.field_bytes + 1);
        let error = store
            .record(&scope, draft("the output is huge", "b/tool:1").remedy(long))
            .expect_err("a field over the bound is refused");
        assert!(matches!(error, MemoryError::TooManyBytes { .. }), "{error}");
        let anchors = vec!["an anchor".to_owned(); plan.settings.limits.anchors_per_attempt + 1];
        let error = store
            .record(
                &scope,
                draft("the output is huge", "b/tool:2").anchors(anchors),
            )
            .expect_err("too many anchors are refused");
        assert!(
            matches!(error, MemoryError::TooManyAnchors { .. }),
            "{error}"
        );
    }

    /// A root cause over its character bound is refused, and one under it is accepted even when
    /// its bytes would exceed the byte bound.
    ///
    /// # Panics
    ///
    /// When the write succeeds over the bound, or a root cause the ADR allows is refused.
    pub fn a_root_cause_over_the_bound_is_refused(store: DynSiMemory, plan: Plan) {
        let scope = scope("root-cause-bound");
        let long = "é".repeat(plan.settings.limits.root_cause_chars + 1);
        let error = store
            .record(&scope, draft(&long, "c/tool:1"))
            .expect_err("a root cause over the bound is refused");
        assert!(matches!(error, MemoryError::TooManyChars { .. }), "{error}");
        // 199 characters, 398 bytes: under the character bound, over the byte one.
        let accepted = "é".repeat(plan.settings.limits.root_cause_chars - 1);
        assert!(accepted.len() > plan.settings.limits.root_cause_chars);
        store
            .record(&scope, draft(&accepted, "c/tool:2"))
            .expect("the bound counts characters, not bytes");
    }

    /// A full scope refuses a new problem, and a sweep frees the room again.
    ///
    /// # Panics
    ///
    /// When the bound is not enforced, or a sweep does not free what it removes.
    pub fn a_full_scope_refuses_another_problem(store: DynSiMemory, plan: Plan) {
        let scope = scope("scope-bound");
        for i in 0..plan.settings.limits.problems_per_scope {
            record(
                &store,
                &scope,
                &format!("problem {i}"),
                &format!("s/tool:{i}"),
            );
        }
        let error = store
            .record(&scope, draft("one problem too many", "s/tool:last"))
            .expect_err("a full scope refuses a new problem");
        assert!(matches!(error, MemoryError::ScopeFull { .. }), "{error}");
        let future = adam_core::Timestamp::now() + span_of(Duration::from_secs(3600));
        assert_eq!(
            store.sweep(&scope, future).unwrap(),
            plan.settings.limits.problems_per_scope as u64,
            "the sweep removes every attempt"
        );
        record(&store, &scope, "one problem too many", "s/tool:last");
    }

    /// A problem with as many attempts as it may refuses one more.
    ///
    /// # Panics
    ///
    /// When the bound is not enforced.
    pub fn a_full_problem_refuses_another_attempt(store: DynSiMemory, plan: Plan) {
        let scope = scope("problem-bound");
        for i in 0..plan.settings.limits.attempts_per_problem {
            record(&store, &scope, "the tests flake", &format!("a/tool:{i}"));
        }
        let error = store
            .record(&scope, draft("the tests flake", "a/tool:last"))
            .expect_err("a full problem refuses another attempt");
        assert!(matches!(error, MemoryError::ProblemFull { .. }), "{error}");
    }

    /// An agent scope refuses a repository, a path, a URL, a host and an e-mail address; a
    /// repository scope keeps the same texts.
    ///
    /// # Panics
    ///
    /// When an agent-scoped write succeeds, or a repository-scoped one is refused.
    pub fn an_agent_scope_refuses_an_address(store: DynSiMemory, _plan: Plan) {
        let cases = [
            ("repository", "github.com/vymalo/another-adam-rs"),
            ("path", "/etc/passwd"),
            ("URL", "https://example.com/x"),
            ("host", "db.internal:5432"),
            ("e-mail address", "someone@example.com"),
        ];
        let agent_scope = scope("addresses");
        for (index, (what, text)) in cases.iter().enumerate() {
            let error = store
                .record(
                    &agent_scope,
                    draft(&format!("problem {index}"), &format!("a/tool:{index}")).remedy(*text),
                )
                .expect_err("an agent scope refuses an address");
            assert!(
                matches!(error, MemoryError::NamesAddress { what: found, .. } if found == *what),
                "{text:?}: {error}"
            );
        }
        assert!(
            store.recall(&agent_scope).unwrap().is_empty(),
            "the refusals stored nothing"
        );
        let repository_scope = repository_scope("addresses");
        for (index, (_, text)) in cases.iter().enumerate() {
            store
                .record(
                    &repository_scope,
                    draft(&format!("problem {index}"), &format!("r/tool:{index}")).remedy(*text),
                )
                .expect("a repository scope keeps an address");
        }
        assert_eq!(store.recall(&repository_scope).unwrap().len(), 5);
    }

    /// An observation is stamped once, the first wins, and a failure is kept.
    ///
    /// # Panics
    ///
    /// When an observation is lost, overwritten, or an unknown attempt is accepted.
    pub fn an_observation_is_stamped_once(store: DynSiMemory, _plan: Plan) {
        let scope = scope("observation");
        let written = record(&store, &scope, "the build fails", "o/tool:1");
        let failed = record(&store, &scope, "the build fails", "o/tool:2");
        let second = failed.attempts[1].id.clone();
        let observed = store
            .observe(
                &scope,
                &written.root_cause,
                &written.attempts[0].id,
                Observation::new(Outcome::Success, "the build passed"),
            )
            .unwrap();
        assert_eq!(
            observed.attempts[0].observed.as_ref().unwrap().outcome,
            Outcome::Success
        );
        assert!(observed.attempts[0].observed_at.is_some());
        let again = store
            .observe(
                &scope,
                &written.root_cause,
                &written.attempts[0].id,
                Observation::new(Outcome::Failure, "later"),
            )
            .unwrap();
        assert_eq!(
            again.attempts[0].observed.as_ref().unwrap().outcome,
            Outcome::Success,
            "the first observation is the record"
        );
        store
            .observe(
                &scope,
                &written.root_cause,
                &second,
                Observation::new(Outcome::Failure, "the build failed again"),
            )
            .unwrap();
        let recalled = store.recall(&scope).unwrap();
        assert_eq!(
            recalled[0].attempts[1].observed.as_ref().unwrap().outcome,
            Outcome::Failure,
            "a failed attempt is kept with its outcome"
        );

        let error = store
            .observe(
                &scope,
                "no such problem",
                &AttemptId::new("x"),
                Observation::new(Outcome::Success, "nothing"),
            )
            .expect_err("an unknown problem is refused");
        assert!(matches!(error, MemoryError::ProblemNotFound(_)), "{error}");
        let error = store
            .observe(
                &scope,
                &written.root_cause,
                &AttemptId::new("no such attempt"),
                Observation::new(Outcome::Success, "nothing"),
            )
            .expect_err("an unknown attempt is refused");
        assert!(matches!(error, MemoryError::AttemptNotFound(..)), "{error}");
    }

    /// A sweep removes the attempts recorded before its cutoff and keeps the younger ones, and the
    /// freshest problem is served first.
    ///
    /// # Panics
    ///
    /// When the cutoff is not respected, or the order is wrong.
    pub fn a_sweep_removes_only_what_is_old(store: DynSiMemory, _plan: Plan) {
        let scope = scope("sweep");
        record(&store, &scope, "the old problem", "w/tool:1");
        std::thread::sleep(Duration::from_millis(5));
        let cutoff = adam_core::Timestamp::now();
        std::thread::sleep(Duration::from_millis(5));
        record(&store, &scope, "the new problem", "w/tool:2");
        assert_eq!(
            store.recall(&scope).unwrap()[0].root_cause,
            "the new problem",
            "the freshest problem is served first"
        );
        assert_eq!(
            store.sweep(&scope, cutoff).unwrap(),
            1,
            "only the attempt older than the cutoff goes"
        );
        let left = store.recall(&scope).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].root_cause, "the new problem");
        assert_eq!(
            store
                .sweep(
                    &scope,
                    adam_core::Timestamp::now() + span_of(Duration::from_secs(3600))
                )
                .unwrap(),
            1,
            "a cutoff past everything removes the last attempt"
        );
        assert!(store.recall(&scope).unwrap().is_empty());
    }

    /// Sixteen writers on one scope leave sixteen problems: the port is `Send + Sync`, and one
    /// folder is one scope.
    ///
    /// # Panics
    ///
    /// When a writer fails or a problem is lost.
    pub fn sixteen_writers_share_one_scope(store: DynSiMemory, _plan: Plan) {
        let scope = std::sync::Arc::new(scope("writers"));
        let mut writers = Vec::new();
        for i in 0..16 {
            let store = std::sync::Arc::clone(&store);
            let scope = std::sync::Arc::clone(&scope);
            writers.push(std::thread::spawn(move || {
                store
                    .record(
                        &scope,
                        draft(&format!("problem {i}"), &format!("run/tool:{i}")),
                    )
                    .expect("a writer's record is stored");
            }));
        }
        for writer in writers {
            writer.join().expect("a writer thread does not panic");
        }
        assert_eq!(
            store.recall(&scope).unwrap().len(),
            16,
            "every writer's problem is there"
        );
    }
}

/// Write the conformance cases of ADR 0081 decisions 3 to 5 into a `mod si_memory_conformance` of
/// the crate that calls it, for the memory `$make` builds from a [`Plan`].
///
/// `$make` is a path to a `fn(&Plan) -> DynSiMemory`; in a test file of a crate, that is
/// `crate::make`. The cases are synchronous, so the calling crate needs no runtime.
#[macro_export]
macro_rules! si_memory_conformance {
    ($make:path) => {
        #[allow(missing_docs)]
        mod si_memory_conformance {
            use super::*;
            use $crate::Plan;

            fn make(plan: &Plan) -> $crate::DynSiMemory {
                $make(plan)
            }

            #[test]
            fn record_then_recall_round_trips() {
                $crate::cases::record_then_recall_round_trips(
                    make(&Plan::default()),
                    Plan::default(),
                );
            }

            #[test]
            fn attempts_with_one_root_cause_share_a_problem() {
                $crate::cases::attempts_with_one_root_cause_share_a_problem(
                    make(&Plan::default()),
                    Plan::default(),
                );
            }

            #[test]
            fn a_record_lands_once_for_one_key() {
                $crate::cases::a_record_lands_once_for_one_key(
                    make(&Plan::default()),
                    Plan::default(),
                );
            }

            #[test]
            fn recall_answers_only_its_scope() {
                $crate::cases::recall_answers_only_its_scope(
                    make(&Plan::default()),
                    Plan::default(),
                );
            }

            #[test]
            fn a_text_is_scrubbed_before_the_write() {
                $crate::cases::a_text_is_scrubbed_before_the_write(
                    make(&Plan::default()),
                    Plan::default(),
                );
            }

            #[test]
            fn a_text_carrying_the_marker_is_refused() {
                $crate::cases::a_text_carrying_the_marker_is_refused(
                    make(&Plan::default()),
                    Plan::default(),
                );
            }

            #[test]
            fn a_field_over_the_bound_is_refused() {
                let plan = Plan::default();
                $crate::cases::a_field_over_the_bound_is_refused(make(&plan), plan);
            }

            #[test]
            fn a_root_cause_over_the_bound_is_refused() {
                let plan = Plan::default();
                $crate::cases::a_root_cause_over_the_bound_is_refused(make(&plan), plan);
            }

            #[test]
            fn a_full_scope_refuses_another_problem() {
                let plan = Plan::default();
                $crate::cases::a_full_scope_refuses_another_problem(make(&plan), plan);
            }

            #[test]
            fn a_full_problem_refuses_another_attempt() {
                let plan = Plan::default();
                $crate::cases::a_full_problem_refuses_another_attempt(make(&plan), plan);
            }

            #[test]
            fn an_agent_scope_refuses_an_address() {
                $crate::cases::an_agent_scope_refuses_an_address(
                    make(&Plan::default()),
                    Plan::default(),
                );
            }

            #[test]
            fn an_observation_is_stamped_once() {
                $crate::cases::an_observation_is_stamped_once(
                    make(&Plan::default()),
                    Plan::default(),
                );
            }

            #[test]
            fn a_sweep_removes_only_what_is_old() {
                $crate::cases::a_sweep_removes_only_what_is_old(
                    make(&Plan::default()),
                    Plan::default(),
                );
            }

            #[test]
            fn sixteen_writers_share_one_scope() {
                $crate::cases::sixteen_writers_share_one_scope(
                    make(&Plan::default()),
                    Plan::default(),
                );
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Arc;

    use adam_memory::InProcessSiMemory;

    use super::*;

    #[test]
    fn the_reference_passes_the_cases_the_doubles_fail() {
        let plan = Plan::default();
        let reference: DynSiMemory = Arc::new(InProcessSiMemory::new(plan.settings.clone()));
        cases::recall_answers_only_its_scope(reference, plan);
        let plan = Plan::default();
        let reference: DynSiMemory = Arc::new(InProcessSiMemory::new(plan.settings.clone()));
        cases::a_record_lands_once_for_one_key(reference, plan);
    }

    #[test]
    fn a_scope_blind_double_fails_the_suite() {
        let plan = Plan::default();
        let double: DynSiMemory = Arc::new(doubles::ScopeBlindSiMemory::new(plan.settings.clone()));
        let failed = catch_unwind(AssertUnwindSafe(|| {
            cases::recall_answers_only_its_scope(double, plan);
        }))
        .is_err();
        assert!(
            failed,
            "the suite accepted a double that answers another scope"
        );
    }

    #[test]
    fn a_double_writer_fails_the_suite() {
        let plan = Plan::default();
        let double: DynSiMemory =
            Arc::new(doubles::DoubleWritingSiMemory::new(plan.settings.clone()));
        let failed = catch_unwind(AssertUnwindSafe(|| {
            cases::a_record_lands_once_for_one_key(double, plan);
        }))
        .is_err();
        assert!(
            failed,
            "the suite accepted a double that writes twice for one key"
        );
    }
}
