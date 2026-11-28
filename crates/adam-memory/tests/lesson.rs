//! The L2 conformance suite against the in-process reference, over one shared store so the
//! two-scope case is real: two adapters with two configured scopes read one store.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, OnceLock};

use adam_memory::{DynMemory, InProcessLessonStore, InProcessMemory, MemoryScope};
use adam_memory_testkit::LessonPlan;

fn store() -> &'static InProcessLessonStore {
    static STORE: OnceLock<InProcessLessonStore> = OnceLock::new();
    STORE.get_or_init(InProcessLessonStore::new)
}

fn make(plan: &LessonPlan, scope: &MemoryScope) -> DynMemory {
    Arc::new(InProcessMemory::shared(
        plan.settings.clone(),
        scope.clone(),
        InProcessLessonStore::clone(store()),
    ))
}

adam_memory_testkit::memory_conformance!(crate::make);
