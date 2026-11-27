//! The L2 values and keys: a lesson, the audience it is served to, and the scope an adapter is
//! configured with.
//!
//! L2 is every memory reached over the network, shared across sessions (ADR 0081 decision 1). An
//! adapter is configured with one [`MemoryScope`]; within it a lesson is keyed by its [`Audience`]
//! and its root cause, and two scopes on one database never see each other's lessons (decision 4).

use std::fmt;

use adam_core::Timestamp;
use serde::{Deserialize, Serialize};

use crate::validate::scrub_text;
use crate::{MemoryError, MemoryResult, Scrubber};

/// The scope an L2 adapter is configured with.
///
/// The deployment chooses it: a single store several deployments share, or one per `adam-coder`
/// (ADR 0081 decision 4). Every key and every query an adapter makes carries it, so two adapters
/// over one database with two scopes never see each other's lessons. The port does not interpret
/// it; the adapter only stores under it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MemoryScope(String);

impl MemoryScope {
    /// A scope. It is not checked here: a call refuses an empty one.
    #[must_use]
    pub fn new(scope: impl Into<String>) -> Self {
        Self(scope.into())
    }

    /// The scope as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn validate(&self) -> MemoryResult<()> {
        if self.0.trim().is_empty() {
            return Err(MemoryError::EmptyField {
                field: "memory scope",
            });
        }
        Ok(())
    }
}

impl fmt::Display for MemoryScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Who a lesson is served to, inside its adapter's [`MemoryScope`].
///
/// The promotion pass computes it as the intersection of the lesson's sources' audiences (ADR 0081
/// decision 10): an agent's name, or the value the deployment reserves for every audience. The port
/// keeps it opaque, so a lesson never widens inside an adapter.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Audience(String);

impl Audience {
    /// An audience. It is not checked here: a call refuses an empty one.
    #[must_use]
    pub fn new(audience: impl Into<String>) -> Self {
        Self(audience.into())
    }

    /// The audience as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn validate(&self) -> MemoryResult<()> {
        if self.0.trim().is_empty() {
            return Err(MemoryError::EmptyField { field: "audience" });
        }
        Ok(())
    }
}

impl fmt::Display for Audience {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A lesson's state. A closed set: a new state must fail to compile everywhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LessonState {
    /// A candidate the promotion rule made and no reviewer has approved yet. The port does not
    /// write it: the promotion pass will (ADR 0081 decisions 8 and 9, M7 and M8).
    Proposed,
    /// Approved by the reviewer and served to its audience.
    Active,
    /// Kept, and no longer served (decision 10).
    Retired,
}

impl LessonState {
    /// Whether a recall serves a lesson in this state.
    #[must_use]
    pub fn is_served(&self) -> bool {
        matches!(*self, Self::Active)
    }
}

/// Where a lesson came from: the L1 scopes whose evidence made it, and the review that approved it.
///
/// The sources are opaque labels the promotion pass chooses, not L1 keys: a repository's key would
/// name the repository a lesson may not name (ADR 0081 decision 5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LessonProvenance {
    /// The L1 scopes whose evidence made the lesson; at least one.
    pub sources: Vec<String>,
    /// The review that approved it (decision 9): the reviewer's alias, or the run that reviewed it.
    pub review: String,
}

impl LessonProvenance {
    /// Provenance from `sources` and `review`.
    #[must_use]
    pub fn new(
        sources: impl IntoIterator<Item = impl Into<String>>,
        review: impl Into<String>,
    ) -> Self {
        Self {
            sources: sources.into_iter().map(Into::into).collect(),
            review: review.into(),
        }
    }
}

/// The lesson a caller writes, before the store stamps it.
///
/// The root cause is the lesson's identity inside its audience: a second record with the same
/// scrubbed root cause updates the lesson rather than opening another (the reviewer approves a
/// candidate by upserting it, decision 9).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct LessonDraft {
    /// Who the lesson is served to.
    pub audience: Audience,
    /// The short root cause; the lesson's identity inside the audience.
    pub root_cause: String,
    /// What to do about it.
    pub remedy: String,
    /// Why it holds.
    pub rationale: String,
    /// Code anchors as text, so a lesson can be judged stale; the crate never reads the code.
    pub anchors: Vec<String>,
    /// The L1 scopes and the review that made the lesson.
    pub provenance: LessonProvenance,
}

impl LessonDraft {
    /// A draft for `root_cause` and `audience`, approved by `provenance`; every text is empty.
    #[must_use]
    pub fn new(
        audience: Audience,
        root_cause: impl Into<String>,
        provenance: LessonProvenance,
    ) -> Self {
        Self {
            audience,
            root_cause: root_cause.into(),
            remedy: String::new(),
            rationale: String::new(),
            anchors: Vec::new(),
            provenance,
        }
    }

    /// What to do about it.
    #[must_use]
    pub fn remedy(mut self, remedy: impl Into<String>) -> Self {
        self.remedy = remedy.into();
        self
    }

    /// Why it holds.
    #[must_use]
    pub fn rationale(mut self, rationale: impl Into<String>) -> Self {
        self.rationale = rationale.into();
        self
    }

    /// Code anchors as text.
    #[must_use]
    pub fn anchors(mut self, anchors: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.anchors = anchors.into_iter().map(Into::into).collect();
        self
    }
}

/// A lesson, as stored: the draft's texts after scrubbing, its state and its timestamps.
///
/// A retired lesson is kept, not deleted (ADR 0081 decision 10), so the same root cause can be
/// recorded again when the reviewer approves a fresh version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Lesson {
    /// Who the lesson is served to.
    pub audience: Audience,
    /// The short root cause; the lesson's identity inside its audience.
    pub root_cause: String,
    /// What to do about it.
    pub remedy: String,
    /// Why it holds.
    pub rationale: String,
    /// Code anchors as text.
    pub anchors: Vec<String>,
    /// The L1 scopes and the review that made the lesson.
    pub provenance: LessonProvenance,
    /// `Active` when served, `Retired` when kept but no longer served.
    pub state: LessonState,
    /// When the lesson was first recorded.
    pub created: Timestamp,
    /// When the lesson last changed.
    pub updated: Timestamp,
    /// When it was retired, once.
    pub retired_at: Option<Timestamp>,
}

/// The bounds an L2 audience keeps. The values are ADR 0081 decision 5's, as its agent page
/// records them: 128 lessons an audience, 1 KiB a text field, 200 characters a root cause, and 16
/// anchors a lesson.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct LessonLimits {
    /// Lessons one audience serves at most. A further new root cause is refused, not evicted; a
    /// retired lesson is kept and does not count, so retirement frees a slot.
    pub lessons_per_audience: usize,
    /// Bytes one text field (remedy, rationale, anchor, provenance label) keeps at most, after
    /// scrubbing.
    pub field_bytes: usize,
    /// Characters a root cause keeps at most, after scrubbing.
    pub root_cause_chars: usize,
    /// Anchors one lesson keeps at most.
    pub anchors_per_lesson: usize,
}

impl Default for LessonLimits {
    fn default() -> Self {
        Self {
            lessons_per_audience: 128,
            field_bytes: 1024,
            root_cause_chars: 200,
            anchors_per_lesson: 16,
        }
    }
}

/// What an L2 adapter is built with: the injected scrub and the bounds.
///
/// There is no default scrubber on purpose: a deployment that forgot to inject one would store
/// secrets, so an adapter cannot be built without one.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct LessonSettings {
    /// The scrub every text passes through before it is stored.
    pub scrubber: Scrubber,
    /// The bounds every write respects.
    pub limits: LessonLimits,
}

impl LessonSettings {
    /// The ADR's bounds, scrubbed by `scrubber`.
    #[must_use]
    pub fn new(scrubber: Scrubber) -> Self {
        Self {
            scrubber,
            limits: LessonLimits::default(),
        }
    }

    /// The same scrubber with other bounds.
    #[must_use]
    pub fn with_limits(mut self, limits: LessonLimits) -> Self {
        self.limits = limits;
        self
    }
}

/// Scrub every text of `draft` and check it, then stamp a served lesson at `now`.
///
/// Every text is scrubbed first; a text that still holds [`crate::REDACTION_MARKER`] is refused.
/// A lesson may not name a repository, a path, a URL, a host or an e-mail address whatever its
/// audience (ADR 0081 decision 5), so every text goes through the address check too. The audience
/// and the provenance labels are keys the promotion pass chooses, and are scrubbed but not
/// checked for addresses.
pub(crate) fn prepare(
    scope: &MemoryScope,
    draft: LessonDraft,
    settings: &LessonSettings,
    now: Timestamp,
) -> MemoryResult<Lesson> {
    scope.validate()?;
    draft.audience.validate()?;
    if draft.provenance.sources.is_empty() {
        return Err(MemoryError::EmptyField {
            field: "lesson sources",
        });
    }
    if draft.provenance.review.trim().is_empty() {
        return Err(MemoryError::EmptyField {
            field: "lesson review",
        });
    }
    if draft.anchors.len() > settings.limits.anchors_per_lesson {
        return Err(MemoryError::TooManyAnchors {
            anchors: draft.anchors.len(),
            limit: settings.limits.anchors_per_lesson,
        });
    }
    let root_cause = root_cause(&draft.root_cause, settings)?;
    let remedy = text("remedy", &draft.remedy, settings)?;
    let rationale = text("rationale", &draft.rationale, settings)?;
    let anchors = draft
        .anchors
        .iter()
        .map(|anchor| text("anchor", anchor, settings))
        .collect::<MemoryResult<Vec<_>>>()?;
    let sources = draft
        .provenance
        .sources
        .iter()
        .map(|source| label("lesson source", source, settings))
        .collect::<MemoryResult<Vec<_>>>()?;
    let review = label("lesson review", &draft.provenance.review, settings)?;
    Ok(Lesson {
        audience: draft.audience,
        root_cause,
        remedy,
        rationale,
        anchors,
        provenance: LessonProvenance::new(sources, review),
        state: LessonState::Active,
        created: now,
        updated: now,
        retired_at: None,
    })
}

fn root_cause(raw: &str, settings: &LessonSettings) -> MemoryResult<String> {
    if raw.trim().is_empty() {
        return Err(MemoryError::EmptyField {
            field: "root cause",
        });
    }
    let clean = scrub_text("root cause", raw, &settings.scrubber, true)?;
    let chars = clean.chars().count();
    if chars > settings.limits.root_cause_chars {
        return Err(MemoryError::TooManyChars {
            field: "root cause",
            chars,
            limit: settings.limits.root_cause_chars,
        });
    }
    Ok(clean)
}

/// A lesson's own text: scrubbed, address-checked and bounded by bytes.
fn text(field: &'static str, raw: &str, settings: &LessonSettings) -> MemoryResult<String> {
    let clean = scrub_text(field, raw, &settings.scrubber, true)?;
    bytes(field, clean, settings.limits.field_bytes)
}

/// A key or provenance label: scrubbed and bounded, but not address-checked.
fn label(field: &'static str, raw: &str, settings: &LessonSettings) -> MemoryResult<String> {
    let clean = scrub_text(field, raw, &settings.scrubber, false)?;
    bytes(field, clean, settings.limits.field_bytes)
}

fn bytes(field: &'static str, clean: String, limit: usize) -> MemoryResult<String> {
    if clean.len() > limit {
        return Err(MemoryError::TooManyBytes {
            field,
            bytes: clean.len(),
            limit,
        });
    }
    Ok(clean)
}

/// The order a recall serves: the freshest lesson first, then by root cause so the answer is
/// stable.
pub(crate) fn sort_newest_first(lessons: &mut [Lesson]) {
    lessons.sort_by(|a, b| {
        b.updated
            .cmp(&a.updated)
            .then_with(|| a.root_cause.cmp(&b.root_cause))
    });
}
