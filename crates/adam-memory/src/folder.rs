//! [`FolderSiMemory`]: L1's only adapter, plain JSON files in a directory the deployment gives the
//! agent.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use adam_core::Timestamp;
use serde::{Deserialize, Serialize};

use crate::slug::{digest, name};
use crate::validate::{prepare, prepare_observation};
use crate::{
    AttemptDraft, AttemptId, MemoryError, MemoryResult, Observation, Problem, Scope, Settings,
    SiMemory,
};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// A scope's file: the key it belongs to and the problem it holds.
///
/// The scope is written into every file so a folder that was copied, or a hash that collided, is
/// caught when it is read instead of serving another scope's problem.
#[derive(Debug, Serialize, Deserialize)]
struct StoredProblem {
    scope: Scope,
    problem: Problem,
}

/// L1's only adapter: one directory, one JSON file per problem, a subdirectory per scope.
///
/// A write is a temp file and a rename, so a reader never sees half a problem; a `Mutex` serialises
/// the writers of one instance. The ADR's topology is one folder per worker (two workers with two
/// folders are two L1s), so two processes are not expected to share a folder; a second process that
/// does would race at the rename and last-writer-wins.
pub struct FolderSiMemory {
    root: PathBuf,
    settings: Settings,
    gate: Mutex<()>,
}

impl FolderSiMemory {
    /// Open the store in `root`, creating the directory (and its parents) when it is missing.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Backend`] when the directory cannot be created.
    pub fn open(root: impl Into<PathBuf>, settings: Settings) -> MemoryResult<Self> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(MemoryError::backend)?;
        Ok(Self {
            root,
            settings,
            gate: Mutex::new(()),
        })
    }

    /// The directory it keeps its files in.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The settings it was built with.
    #[must_use]
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    fn gate(&self) -> MutexGuard<'_, ()> {
        self.gate.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn scope_dir(&self, scope: &Scope) -> PathBuf {
        self.root
            .join(format!("{}-{}", scope.slug(), digest(&scope.key())))
    }

    fn problem_path(&self, scope: &Scope, root_cause: &str) -> PathBuf {
        self.scope_dir(scope)
            .join(format!("{}.json", name(root_cause, 40)))
    }

    fn read_stored(&self, path: &Path) -> MemoryResult<Option<StoredProblem>> {
        match fs::read_to_string(path) {
            Ok(text) => {
                serde_json::from_str(&text)
                    .map(Some)
                    .map_err(|error| MemoryError::Corrupt {
                        path: path.display().to_string(),
                        reason: error.to_string(),
                    })
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(MemoryError::backend(error)),
        }
    }

    fn write_stored(&self, path: &Path, stored: &StoredProblem) -> MemoryResult<()> {
        let Some(dir) = path.parent() else {
            return Err(MemoryError::backend(io::Error::other(
                "the problem path has no directory",
            )));
        };
        fs::create_dir_all(dir).map_err(MemoryError::backend)?;
        let bytes = serde_json::to_vec_pretty(stored).map_err(|error| MemoryError::Corrupt {
            path: path.display().to_string(),
            reason: error.to_string(),
        })?;
        let temp = dir.join(format!(
            ".{}-{}.tmp",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&temp, bytes).map_err(MemoryError::backend)?;
        if let Err(error) = fs::rename(&temp, path) {
            let _ = fs::remove_file(&temp);
            return Err(MemoryError::backend(error));
        }
        Ok(())
    }

    /// Every problem in `scope`, with the file it came from.
    fn list_stored(&self, scope: &Scope) -> MemoryResult<Vec<(PathBuf, Problem)>> {
        let dir = self.scope_dir(scope);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(MemoryError::backend(error)),
        };
        let mut problems = Vec::new();
        for entry in entries {
            let entry = entry.map_err(MemoryError::backend)?;
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            if let Some(stored) = self.read_stored(&path)? {
                if stored.scope != *scope {
                    return Err(MemoryError::Corrupt {
                        path: path.display().to_string(),
                        reason: "it belongs to another scope".to_owned(),
                    });
                }
                problems.push((path, stored.problem));
            }
        }
        Ok(problems)
    }

    /// How many problems the scope holds.
    fn count(&self, scope: &Scope) -> MemoryResult<usize> {
        let dir = self.scope_dir(scope);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(MemoryError::backend(error)),
        };
        let mut count = 0;
        for entry in entries {
            let entry = entry.map_err(MemoryError::backend)?;
            if entry
                .path()
                .extension()
                .and_then(|extension| extension.to_str())
                == Some("json")
            {
                count += 1;
            }
        }
        Ok(count)
    }
}

impl SiMemory for FolderSiMemory {
    fn record(&self, scope: &Scope, draft: AttemptDraft) -> MemoryResult<Problem> {
        scope.validate()?;
        let now = Timestamp::now();
        let prepared = prepare(scope, draft, &self.settings, now)?;
        let _gate = self.gate();
        let path = self.problem_path(scope, &prepared.root_cause);
        let stored = self.read_stored(&path)?;
        if let Some(stored) = &stored {
            if stored.problem.root_cause != prepared.root_cause {
                return Err(MemoryError::Corrupt {
                    path: path.display().to_string(),
                    reason: "the file holds another root cause".to_owned(),
                });
            }
            if stored
                .problem
                .attempts
                .iter()
                .any(|attempt| attempt.id == prepared.attempt.id)
            {
                return Ok(stored.problem.clone());
            }
        } else if self.count(scope)? >= self.settings.limits.problems_per_scope {
            return Err(MemoryError::ScopeFull {
                limit: self.settings.limits.problems_per_scope,
            });
        }
        let mut problem = stored.map_or_else(
            || Problem::new(prepared.root_cause.clone(), now),
            |stored| stored.problem,
        );
        problem.push(prepared.attempt, &self.settings.limits, now)?;
        self.write_stored(
            &path,
            &StoredProblem {
                scope: scope.clone(),
                problem: problem.clone(),
            },
        )?;
        Ok(problem)
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
        let _gate = self.gate();
        let path = self.problem_path(scope, root_cause);
        let Some(stored) = self.read_stored(&path)? else {
            return Err(MemoryError::ProblemNotFound(root_cause.to_owned()));
        };
        if stored.problem.root_cause != root_cause {
            return Err(MemoryError::Corrupt {
                path: path.display().to_string(),
                reason: "the file holds another root cause".to_owned(),
            });
        }
        let mut problem = stored.problem;
        let stored_attempt = problem
            .attempts
            .iter_mut()
            .find(|stored| stored.id == *attempt)
            .ok_or_else(|| {
                MemoryError::AttemptNotFound(attempt.to_string(), root_cause.to_owned())
            })?;
        if stored_attempt.observed.is_none() {
            stored_attempt.observed = Some(observation);
            stored_attempt.observed_at = Some(now);
            problem.updated = now;
            self.write_stored(
                &path,
                &StoredProblem {
                    scope: scope.clone(),
                    problem: problem.clone(),
                },
            )?;
        }
        Ok(problem)
    }

    fn recall(&self, scope: &Scope) -> MemoryResult<Vec<Problem>> {
        scope.validate()?;
        let _gate = self.gate();
        let mut problems: Vec<Problem> = self
            .list_stored(scope)?
            .into_iter()
            .map(|(_, problem)| problem)
            .collect();
        crate::sort_newest_first(&mut problems);
        Ok(problems)
    }

    fn sweep(&self, scope: &Scope, before: Timestamp) -> MemoryResult<u64> {
        scope.validate()?;
        let _gate = self.gate();
        let mut removed = 0_u64;
        for (path, mut problem) in self.list_stored(scope)? {
            let kept = problem.attempts.len();
            problem
                .attempts
                .retain(|attempt| attempt.recorded >= before);
            let gone = kept - problem.attempts.len();
            if gone == 0 {
                continue;
            }
            removed += gone as u64;
            if problem.attempts.is_empty() {
                match fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(MemoryError::backend(error)),
                }
            } else {
                self.write_stored(
                    &path,
                    &StoredProblem {
                        scope: scope.clone(),
                        problem,
                    },
                )?;
            }
        }
        Ok(removed)
    }
}
