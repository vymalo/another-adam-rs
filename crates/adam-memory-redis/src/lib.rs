//! The L2 [`Memory`] port on Redis: [`RedisMemory`].
//!
//! Redis is **L2, a store and not an L1 cache** ([ADR 0081] decision 2): a database reached over
//! the network is L2 even when one agent alone uses it, so this adapter implements [`Memory`] and
//! never `SiMemory`. The lessons that held across L1 scopes are shared across sessions, keyed
//! within the adapter's configured [`MemoryScope`] by audience (decision 4).
//!
//! [ADR 0081]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0081-an-agent-keeps-a-self-improvement-memory-near-it-and-what-holds-becomes-the-deployments-memory.md
//!
//! # Keys
//!
//! One Redis hash per `(scope, audience)`:
//!
//! ```text
//! <prefix>:<scope_len>:<scope>:<audience>     (prefix "adam-memory:v1" by default)
//! ```
//!
//! The field is the lesson's stored (scrubbed) root cause, the value the lesson as JSON. The
//! length prefix makes the scope unambiguous whatever bytes it or the audience hold, so two
//! adapters with two scopes over one Redis never share a hash, and the audience is the rest of
//! the key. The rules that decide what a lesson may hold — the scope validation, the scrub, the
//! marker, the address check, the bounds — are [`adam_memory`]'s own, through
//! [`adam_memory::prepare`], so this adapter and the in-process reference enforce
//! the same rules.
//!
//! # Concurrency and connections
//!
//! A write reads the audience's lessons, checks the bound that counts what is served, and writes
//! one field, under WATCH/MULTI/EXEC: `redis::transaction` watches the key, retries when it
//! changed under the write, and the loop gives up after 128 attempts. Retirement is the same
//! transaction, so a retried step lands once and nothing is deleted (decision 10: a retired
//! lesson is kept and no longer served).
//!
//! The port is `Send + Sync`, and `redis::Connection` is not `Sync`, so the adapter opens **one
//! connection per call**. That is a round trip's worth of cost per call and no more; a pooled
//! or multiplexed connection is a follow-up, and does not change what the suite proves.
//!
//! # Durability
//!
//! Redis is a cache until its deployment says otherwise. The adapter documents the two settings a
//! lesson store needs — AOF (`appendonly yes`, with `appendfsync everysec` or `always`) and
//! `maxmemory-policy noeviction` — and cannot enforce them: a deployment that chooses an
//! eviction policy silently drops lessons instead of refusing a write. `rediss://` works through
//! the bundled webpki roots.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::HashMap;

use adam_core::Timestamp;
use adam_memory::{
    Audience, Lesson, LessonDraft, LessonSettings, LessonState, Memory, MemoryError, MemoryResult,
    MemoryScope, prepare, sort_lessons_newest_first,
};
use redis::{Client, Connection, RedisError};

/// The key prefix a [`RedisMemory`] is built with when none is given.
const DEFAULT_KEY_PREFIX: &str = "adam-memory:v1";

/// How often a write retries the WATCH/MULTI/EXEC loop before it gives up as a backend failure.
///
/// The sixteen-writer conformance case contends on one audience's key, and each writer expects on
/// the order of sixteen retries (one per competing writer), so 128 keeps a wide margin and removes
/// a flake class; the loop is still bounded and gives up as a `Transient` failure.
const WRITE_ATTEMPTS: u32 = 128;

/// The L2 [`Memory`] port on Redis: one configured scope, one hash per audience.
///
/// Built with [`RedisMemory::open`], which parses the URL and refuses one this client cannot
/// read, so a deployment fails at startup rather than at its first lesson (ADR 0081 decision 12).
/// Nothing connects until the first call; at run time an unreachable Redis is a `Transient`
/// error and the run goes on without its memory.
pub struct RedisMemory {
    settings: LessonSettings,
    scope: MemoryScope,
    client: Client,
    key_prefix: String,
}

impl RedisMemory {
    /// Parse `url` and build an adapter for `scope`.
    ///
    /// Nothing connects until the first call; a URL this client cannot parse is refused here, so
    /// a deployment fails at startup rather than at the first lesson (ADR 0081 decision 12).
    ///
    /// # Errors
    ///
    /// [`MemoryError::Backend`] when the URL does not parse as a Redis address.
    pub fn open(
        url: impl AsRef<str>,
        settings: LessonSettings,
        scope: MemoryScope,
    ) -> MemoryResult<Self> {
        let client = Client::open(url.as_ref()).map_err(MemoryError::backend)?;
        Ok(Self {
            settings,
            scope,
            client,
            key_prefix: DEFAULT_KEY_PREFIX.to_owned(),
        })
    }

    /// Override the key prefix (default `adam-memory:v1`).
    ///
    /// A deployment that shares one Redis with other applications gives its own; every key of the
    /// adapter carries it.
    #[must_use]
    pub fn with_key_prefix(self, prefix: impl Into<String>) -> Self {
        Self {
            key_prefix: prefix.into(),
            ..self
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

    /// The hash that holds one audience's lessons: `<prefix>:<scope_len>:<scope>:<audience>`.
    fn key(&self, audience: &Audience) -> String {
        let scope = self.scope.as_str();
        format!("{}:{}:{}:{}", self.key_prefix, scope.len(), scope, audience)
    }

    /// A connection for one call: `redis::Connection` is not `Sync`, so the `Send + Sync` port
    /// opens one per call rather than holding one.
    fn connection(&self, key: &str) -> MemoryResult<Connection> {
        self.client
            .get_connection()
            .map_err(|error| storage(error, key))
    }
}

impl Memory for RedisMemory {
    fn record(&self, draft: LessonDraft) -> MemoryResult<Lesson> {
        let prepared = prepare(&self.scope, draft, &self.settings, Timestamp::now())?;
        let key = self.key(&prepared.audience);
        let mut conn = self.connection(&key)?;
        let mut attempts = 0;
        let outcome: MemoryResult<Lesson> =
            redis::transaction(&mut conn, &[key.as_str()], |conn, pipe| {
                attempts += 1;
                if attempts > WRITE_ATTEMPTS {
                    return Ok(Some(Err(MemoryError::backend(std::io::Error::other(
                        "the audience's lessons changed under every write attempt",
                    )))));
                }
                let all: HashMap<String, String> = redis::cmd("HGETALL").arg(&key).query(conn)?;
                let mut lessons = Vec::with_capacity(all.len());
                for value in all.values() {
                    let lesson: Lesson = match serde_json::from_str(value) {
                        Ok(lesson) => lesson,
                        Err(reason) => {
                            return Ok(Some(Err(MemoryError::Corrupt {
                                path: key.clone(),
                                reason: reason.to_string(),
                            })));
                        }
                    };
                    lessons.push(lesson);
                }
                let served = lessons
                    .iter()
                    .filter(|lesson| lesson.state.is_served())
                    .count();
                let already_served = lessons.iter().any(|lesson| {
                    lesson.root_cause == prepared.root_cause && lesson.state.is_served()
                });
                if !already_served && served >= self.settings.limits.lessons_per_audience {
                    return Ok(Some(Err(MemoryError::AudienceFull {
                        limit: self.settings.limits.lessons_per_audience,
                    })));
                }
                let mut lesson = prepared.clone();
                if let Some(existing) = lessons
                    .iter()
                    .find(|existing| existing.root_cause == lesson.root_cause)
                {
                    // An upsert keeps the creation time, and a fresh approval serves a retired
                    // lesson again when a slot is free.
                    lesson.created = existing.created;
                }
                let encoded = match serde_json::to_string(&lesson) {
                    Ok(encoded) => encoded,
                    Err(reason) => return Ok(Some(Err(MemoryError::backend(reason)))),
                };
                let landed: Option<(usize,)> =
                    pipe.hset(&key, &lesson.root_cause, encoded).query(conn)?;
                match landed {
                    // The key changed between WATCH and EXEC: the loop retries.
                    None => Ok(None),
                    Some(_) => Ok(Some(Ok(lesson))),
                }
            })
            .map_err(|error| storage(error, &key))?;
        outcome
    }

    fn recall(&self, audience: &Audience) -> MemoryResult<Vec<Lesson>> {
        self.scope.validate()?;
        audience.validate()?;
        let key = self.key(audience);
        let mut conn = self.connection(&key)?;
        let all: HashMap<String, String> = redis::cmd("HGETALL")
            .arg(&key)
            .query(&mut conn)
            .map_err(|error| storage(error, &key))?;
        let mut lessons = Vec::with_capacity(all.len());
        for value in all.into_values() {
            let lesson: Lesson =
                serde_json::from_str(&value).map_err(|reason| MemoryError::Corrupt {
                    path: key.clone(),
                    reason: reason.to_string(),
                })?;
            if lesson.state.is_served() {
                lessons.push(lesson);
            }
        }
        sort_lessons_newest_first(&mut lessons);
        Ok(lessons)
    }

    fn retire(&self, audience: &Audience, root_cause: &str) -> MemoryResult<Lesson> {
        self.scope.validate()?;
        audience.validate()?;
        let key = self.key(audience);
        let mut conn = self.connection(&key)?;
        let mut attempts = 0;
        let outcome: MemoryResult<Lesson> =
            redis::transaction(&mut conn, &[key.as_str()], |conn, pipe| {
                attempts += 1;
                if attempts > WRITE_ATTEMPTS {
                    return Ok(Some(Err(MemoryError::backend(std::io::Error::other(
                        "the audience's lessons changed under every retire attempt",
                    )))));
                }
                let stored: Option<String> =
                    redis::cmd("HGET").arg(&key).arg(root_cause).query(conn)?;
                let Some(stored) = stored else {
                    return Ok(Some(Err(MemoryError::LessonNotFound(
                        root_cause.to_owned(),
                    ))));
                };
                let mut lesson: Lesson = match serde_json::from_str(&stored) {
                    Ok(lesson) => lesson,
                    Err(reason) => {
                        return Ok(Some(Err(MemoryError::Corrupt {
                            path: key.clone(),
                            reason: reason.to_string(),
                        })));
                    }
                };
                if lesson.state == LessonState::Retired {
                    // A retried step lands once: the retirement is already there.
                    return Ok(Some(Ok(lesson)));
                }
                let now = Timestamp::now();
                lesson.state = LessonState::Retired;
                lesson.retired_at = Some(now);
                lesson.updated = now;
                let encoded = match serde_json::to_string(&lesson) {
                    Ok(encoded) => encoded,
                    Err(reason) => return Ok(Some(Err(MemoryError::backend(reason)))),
                };
                let landed: Option<(usize,)> =
                    pipe.hset(&key, &lesson.root_cause, encoded).query(conn)?;
                match landed {
                    // The key changed between WATCH and EXEC: the loop retries.
                    None => Ok(None),
                    Some(_) => Ok(Some(Ok(lesson))),
                }
            })
            .map_err(|error| storage(error, &key))?;
        outcome
    }
}

/// What a Redis failure becomes: a backend error, except a `WRONGTYPE` reply on our own key,
/// which is a value this adapter does not own the shape of, and so is corrupt.
fn storage(error: RedisError, key: &str) -> MemoryError {
    if error.code() == Some("WRONGTYPE") {
        MemoryError::Corrupt {
            path: key.to_owned(),
            reason: "the key holds a value that is not a hash of lessons".to_owned(),
        }
    } else {
        MemoryError::backend(error)
    }
}
