//! [`MongoMemory`]: the L2 [`Memory`] port over a MongoDB collection (ADR 0081 decisions 4, 5 and
//! 10), behind the `memory` feature.
//!
//! One document per lesson, keyed by the configured **scope**, the audience and the root cause:
//!
//! ```text
//! { _id, scope: "<the adapter's scope>", audience, root_cause, remedy, ..., state, created, updated }
//! ```
//!
//! The scope is the document's first key so two adapters with two scopes over one database never
//! read each other's lessons — the property `memory_conformance!`'s two-scope case proves.
//!
//! The rules are [`adam_memory::prepare`]'s, the same call the reference adapter makes, so the
//! scrub, the redaction marker, the bounds and the address check cannot drift between adapters. A
//! single document is written at a time, so a standalone `mongod` is enough (no transactions), as
//! for the run store.
//!
//! The audience limit is checked **before** the write, not enforced by a unique index: two
//! concurrent approvals for one audience could each see a free slot and both write. The reference
//! adapter has the same shape behind a mutex; a deployment that needs the bound to hold under
//! concurrency runs one writer per audience, which is what the promotion pass is (ADR 0081
//! decision 8: it runs where L1 is, in the agent's own process).

use adam_core::Timestamp;
use adam_memory::{
    Audience, Lesson, LessonDraft, LessonSettings, LessonState, Memory, MemoryError, MemoryResult,
    MemoryScope, prepare, sort_lessons_newest_first,
};
use mongodb::bson::{Bson, Document, doc, from_document, to_document};
use mongodb::sync::{Collection, Database};

/// The collection a deployment's lessons live in: one per database, so two scopes on one database
/// stay apart by their documents' `scope` field.
pub const LESSONS_COLLECTION: &str = "adam_memory_lessons";

/// The L2 memory over one MongoDB collection.
pub struct MongoMemory {
    collection: Collection<Document>,
    settings: LessonSettings,
    scope: MemoryScope,
}

impl MongoMemory {
    /// A memory over `database`'s `collection`, configured with `scope` and `settings`.
    ///
    /// # Errors
    ///
    /// A MongoDB error, when the collection cannot be described.
    pub fn new(
        database: &Database,
        collection: &str,
        settings: LessonSettings,
        scope: MemoryScope,
    ) -> Result<Self, mongodb::error::Error> {
        Ok(Self {
            collection: database.collection::<Document>(collection),
            settings,
            scope,
        })
    }

    /// Open a memory from a connection string, the shape a deployment's `MEMORY_URL` takes.
    ///
    /// `database` names the database; the collection is this adapter's own (`adam_memory_lessons`),
    /// because a lesson is not a run's and has no run to share a namespace with.
    ///
    /// # Errors
    ///
    /// A MongoDB error, when the client cannot be built.
    pub fn connect(
        uri: &str,
        database: &str,
        settings: LessonSettings,
        scope: MemoryScope,
    ) -> Result<Self, mongodb::error::Error> {
        let client = mongodb::sync::Client::with_uri_str(uri)?;
        Self::new(
            &client.database(database),
            LESSONS_COLLECTION,
            settings,
            scope,
        )
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

    /// Every lesson of one audience in this scope, with its document id.
    fn load(&self, audience: &Audience) -> MemoryResult<Vec<(Bson, Lesson)>> {
        let filter = doc! {
            "scope": self.scope.as_str(),
            "audience": audience.as_str(),
        };
        let cursor = self
            .collection
            .find(filter)
            .run()
            .map_err(MemoryError::backend)?;
        let mut lessons = Vec::new();
        for document in cursor {
            let document = document.map_err(MemoryError::backend)?;
            let id = document.get("_id").cloned().unwrap_or(Bson::Null);
            let lesson: Lesson = from_document(document).map_err(|error| MemoryError::Corrupt {
                path: format!("mongodb collection {}", self.collection.name()),
                reason: error.to_string(),
            })?;
            lessons.push((id, lesson));
        }
        Ok(lessons)
    }
}

impl Memory for MongoMemory {
    fn record(&self, draft: LessonDraft) -> MemoryResult<Lesson> {
        let now = Timestamp::now();
        let mut lesson = prepare(&self.scope, draft, &self.settings, now)?;
        let existing = self.load(&lesson.audience)?;
        let served = existing
            .iter()
            .filter(|(_, held)| held.state.is_served())
            .count();
        let current = existing
            .iter()
            .find(|(_, held)| held.root_cause == lesson.root_cause);
        let already_served = current.is_some_and(|(_, held)| held.state.is_served());
        if !already_served && served >= self.settings.limits.lessons_per_audience {
            return Err(MemoryError::AudienceFull {
                limit: self.settings.limits.lessons_per_audience,
            });
        }
        if let Some((_, held)) = current {
            // An upsert keeps the creation time, and a fresh approval serves a retired lesson
            // again.
            lesson.created = held.created;
        }
        let mut document = to_document(&lesson).map_err(|error| MemoryError::Corrupt {
            path: format!("mongodb collection {}", self.collection.name()),
            reason: error.to_string(),
        })?;
        document.insert("scope", self.scope.as_str());
        match current {
            Some((id, _)) => {
                document.insert("_id", id.clone());
                self.collection
                    .replace_one(doc! { "_id": id }, document)
                    .run()
                    .map_err(MemoryError::backend)?;
            }
            None => {
                self.collection
                    .insert_one(document)
                    .run()
                    .map_err(MemoryError::backend)?;
            }
        }
        Ok(lesson)
    }

    fn recall(&self, audience: &Audience) -> MemoryResult<Vec<Lesson>> {
        self.scope.validate()?;
        audience.validate()?;
        let mut lessons: Vec<Lesson> = self
            .load(audience)?
            .into_iter()
            .map(|(_, lesson)| lesson)
            .filter(|lesson| lesson.state.is_served())
            .collect();
        sort_lessons_newest_first(&mut lessons);
        Ok(lessons)
    }

    fn retire(&self, audience: &Audience, root_cause: &str) -> MemoryResult<Lesson> {
        self.scope.validate()?;
        audience.validate()?;
        let now = Timestamp::now();
        let (id, mut lesson) = self
            .load(audience)?
            .into_iter()
            .find(|(_, held)| held.root_cause == root_cause)
            .ok_or_else(|| MemoryError::LessonNotFound(root_cause.to_owned()))?;
        if lesson.state != LessonState::Retired {
            lesson.state = LessonState::Retired;
            lesson.retired_at = Some(now);
            lesson.updated = now;
            let mut document = to_document(&lesson).map_err(|error| MemoryError::Corrupt {
                path: format!("mongodb collection {}", self.collection.name()),
                reason: error.to_string(),
            })?;
            document.insert("scope", self.scope.as_str());
            document.insert("_id", id.clone());
            self.collection
                .replace_one(doc! { "_id": id }, document)
                .run()
                .map_err(MemoryError::backend)?;
        }
        Ok(lesson)
    }
}
