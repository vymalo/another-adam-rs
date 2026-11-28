//! [`InProcessMemory`]: the reference L2 adapter, in the process's memory.
//!
//! It is the double a test and a host with no L2 store use; it follows the same rules as a database
//! adapter (the scope, the scrub, the marker, the bounds, the address check), so the conformance
//! suite proves the rules, not one adapter's storage.
//!
//! [`InProcessLessonStore`] can be shared by several adapters, so a test can put two configured
//! scopes on one store and prove they never see each other's lessons, the way two scopes on one
//! database are proven.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use adam_core::Timestamp;

use crate::lesson::sort_lessons_newest_first;
use crate::{
    Audience, Lesson, LessonDraft, LessonSettings, LessonState, Memory, MemoryError, MemoryResult,
    MemoryScope,
};

/// One audience's lessons, keyed by root cause.
type Lessons = BTreeMap<String, Lesson>;
/// One scope's audiences.
type Audiences = BTreeMap<String, Lessons>;
/// Every scope of the store.
type Scopes = BTreeMap<String, Audiences>;

/// The storage several [`InProcessMemory`] adapters can share.
///
/// It is keyed by the adapter's configured scope first, so two adapters with two scopes over one
/// store are the in-process image of two scopes on one database.
#[derive(Clone, Default)]
pub struct InProcessLessonStore {
    scopes: Arc<Mutex<Scopes>>,
}

impl InProcessLessonStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn scopes(&self) -> MutexGuard<'_, Scopes> {
        self.scopes.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// An L2 memory in a `BTreeMap`, keyed by the configured scope, the audience and the root cause.
pub struct InProcessMemory {
    settings: LessonSettings,
    scope: MemoryScope,
    store: InProcessLessonStore,
}

impl InProcessMemory {
    /// An empty memory, under its own store, with `settings`' scrub and bounds.
    #[must_use]
    pub fn new(settings: LessonSettings, scope: MemoryScope) -> Self {
        Self::shared(settings, scope, InProcessLessonStore::new())
    }

    /// An empty memory over `store`, so several scopes can share one store.
    #[must_use]
    pub fn shared(
        settings: LessonSettings,
        scope: MemoryScope,
        store: InProcessLessonStore,
    ) -> Self {
        Self {
            settings,
            scope,
            store,
        }
    }

    /// The settings it was built with.
    #[must_use]
    pub fn settings(&self) -> &LessonSettings {
        &self.settings
    }

    /// The scope it is configured with.
    #[must_use]
    pub fn scope(&self) -> &MemoryScope {
        &self.scope
    }
}

impl Memory for InProcessMemory {
    fn record(&self, draft: LessonDraft) -> MemoryResult<Lesson> {
        self.scope.validate()?;
        let now = Timestamp::now();
        let prepared = self.settings.prepare(&self.scope, draft, now)?;
        let mut scopes = self.store.scopes();
        let audiences = scopes.entry(self.scope.as_str().to_owned()).or_default();
        let lessons = audiences
            .entry(prepared.audience.as_str().to_owned())
            .or_default();
        let served = lessons
            .values()
            .filter(|lesson| lesson.state.is_served())
            .count();
        let already_served = lessons
            .get(&prepared.root_cause)
            .is_some_and(|lesson| lesson.state.is_served());
        if !already_served && served >= self.settings.limits.lessons_per_audience {
            return Err(MemoryError::AudienceFull {
                limit: self.settings.limits.lessons_per_audience,
            });
        }
        let mut lesson = prepared;
        if let Some(existing) = lessons.get(&lesson.root_cause) {
            // An upsert keeps the creation time, and a fresh approval serves a retired lesson
            // again.
            lesson.created = existing.created;
        }
        lessons.insert(lesson.root_cause.clone(), lesson.clone());
        Ok(lesson)
    }

    fn recall(&self, audience: &Audience) -> MemoryResult<Vec<Lesson>> {
        self.scope.validate()?;
        audience.validate()?;
        let scopes = self.store.scopes();
        let mut lessons: Vec<Lesson> = scopes
            .get(self.scope.as_str())
            .and_then(|audiences| audiences.get(audience.as_str()))
            .map(|lessons| {
                lessons
                    .values()
                    .filter(|lesson| lesson.state.is_served())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        sort_lessons_newest_first(&mut lessons);
        Ok(lessons)
    }

    fn retire(&self, audience: &Audience, root_cause: &str) -> MemoryResult<Lesson> {
        self.scope.validate()?;
        audience.validate()?;
        let now = Timestamp::now();
        let mut scopes = self.store.scopes();
        let lesson = scopes
            .get_mut(self.scope.as_str())
            .and_then(|audiences| audiences.get_mut(audience.as_str()))
            .and_then(|lessons| lessons.get_mut(root_cause))
            .ok_or_else(|| MemoryError::LessonNotFound(root_cause.to_owned()))?;
        if lesson.state != LessonState::Retired {
            lesson.state = LessonState::Retired;
            lesson.retired_at = Some(now);
            lesson.updated = now;
        }
        Ok(lesson.clone())
    }
}
