//! What this crate keeps on the workspace volume, beside the worktrees.
//!
//! ```text
//! <root>/environments/
//!   .tools/<version>/{adam-exec,opencode}   written once, mounted read-only by every run
//!   .cli-home/                              HOME of the devcontainer CLI (its caches)
//!   <run>/
//!     state.json      what is known of the run's environment (below)
//!     devcontainer.json  the override file the CLI is given
//!     secrets/model-key  mode 0600, mounted read-only in the container
//!     build.log       the last 64 KiB of what the CLI said
//!     lock            the flock that makes `ensure` single-flight across processes
//! ```
//!
//! `state.json` is what lets a coder that restarted find its environments again, and what makes a
//! broken one stay broken (cheaply) until the file that broke it changes. It is scratch, like the
//! worktree it sits beside: it is deleted with the run.

use std::io;
use std::path::{Path, PathBuf};

use adam_workspace::EnvError;
use serde::{Deserialize, Serialize};

/// The version of `state.json`.
pub(crate) const STATE_VERSION: u32 = 1;

/// The directory of everything this crate keeps.
pub(crate) fn environments_dir(root: &Path) -> PathBuf {
    root.join("environments")
}

/// The directory of a run's environment, for a run id that is a single safe path segment.
///
/// # Errors
///
/// [`EnvError::Refused`] for an id that could name another directory.
pub(crate) fn run_dir(root: &Path, run: &str) -> Result<PathBuf, EnvError> {
    let safe = !run.is_empty()
        && run.len() <= 128
        && !run.starts_with('.')
        && run
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if safe {
        Ok(environments_dir(root).join(run))
    } else {
        Err(EnvError::Refused(format!("{run:?} is not a run id")))
    }
}

/// Where the phase of a run's environment stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Phase {
    /// Being made (or interrupted while it was).
    Building,
    /// Made, and the container was alive when last looked at.
    Ready,
    /// Could not be made; the error is kept.
    Broken,
    /// The run goes on in the coder's own container.
    Local,
}

/// A slot as the container was made with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SlotRecord {
    pub dir: String,
    pub seq: u32,
    pub mirror: Option<String>,
}

/// An error that is kept: the ones that a retry would only repeat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub(crate) enum StoredError {
    Config { file: String, reason: String },
    Refused { reason: String },
    Build { reason: String, log_tail: String },
    Timeout { phase: String, secs: u64 },
}

impl StoredError {
    /// The error to keep, or `None` for one that a retry may get past.
    pub(crate) fn of(error: &EnvError) -> Option<Self> {
        Some(match error {
            EnvError::Config { file, reason } => Self::Config {
                file: file.display().to_string(),
                reason: reason.clone(),
            },
            EnvError::Refused(reason) => Self::Refused {
                reason: reason.clone(),
            },
            EnvError::Build { reason, log_tail } => Self::Build {
                reason: reason.clone(),
                log_tail: log_tail.clone(),
            },
            EnvError::Timeout { phase, secs } => Self::Timeout {
                phase: (*phase).to_owned(),
                secs: *secs,
            },
            _ => return None,
        })
    }

    /// The error again.
    pub(crate) fn to_error(&self) -> EnvError {
        match self {
            Self::Config { file, reason } => EnvError::Config {
                file: PathBuf::from(file),
                reason: reason.clone(),
            },
            Self::Refused { reason } => EnvError::Refused(reason.clone()),
            Self::Build { reason, log_tail } => EnvError::Build {
                reason: reason.clone(),
                log_tail: log_tail.clone(),
            },
            Self::Timeout { phase, secs } => EnvError::Timeout {
                phase: match phase.as_str() {
                    "configuration" => "configuration",
                    "setup" => "setup",
                    _ => "build",
                },
                secs: *secs,
            },
        }
    }
}

/// `state.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct State {
    pub version: u32,
    pub run: String,
    pub deployment: String,
    pub phase: Phase,
    /// The devcontainer file used, relative to the first slot; `None` for the default image.
    pub config_source: Option<String>,
    /// Names the content of that file (or of the default image).
    pub config_digest: String,
    /// The person chose the default image for this run (`DevContainer::rebuild`).
    #[serde(default)]
    pub use_default: bool,
    /// The slots the container was made with, in the order they joined.
    pub slots: Vec<SlotRecord>,
    pub container_id: Option<String>,
    pub image: Option<String>,
    #[serde(default)]
    pub keep_id: bool,
    /// Whether a model key was written for the container.
    #[serde(default)]
    pub model_key: bool,
    /// The tools directory that was mounted.
    pub tools: Option<String>,
    /// Why the run is local (`phase` is `local`).
    pub local_reason: Option<String>,
    /// The error of a broken environment.
    pub error: Option<StoredError>,
}

impl State {
    /// A state for `run` that is being built.
    pub(crate) fn building(run: &str, deployment: &str) -> Self {
        Self {
            version: STATE_VERSION,
            run: run.to_owned(),
            deployment: deployment.to_owned(),
            phase: Phase::Building,
            config_source: None,
            config_digest: String::new(),
            use_default: false,
            slots: Vec::new(),
            container_id: None,
            image: None,
            keep_id: false,
            model_key: false,
            tools: None,
            local_reason: None,
            error: None,
        }
    }

    /// Whether the container was made with exactly these slots.
    pub(crate) fn has_slots(&self, slots: &[SlotRecord]) -> bool {
        self.slots == slots
    }

    /// Read the state of `run`, if there is one. A file that cannot be read as a state counts as
    /// none (the environment is made again, which removes the container by its labels).
    ///
    /// # Errors
    ///
    /// [`EnvError::Refused`] for a bad run id; [`EnvError::Io`] when the file cannot be read.
    pub(crate) fn read(root: &Path, run: &str) -> Result<Option<Self>, EnvError> {
        let path = run_dir(root, run)?.join("state.json");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        match serde_json::from_str::<Self>(&text) {
            Ok(state) if state.version == STATE_VERSION => Ok(Some(state)),
            Ok(state) => {
                tracing::warn!(
                    run,
                    version = state.version,
                    "state.json of another version: ignored"
                );
                Ok(None)
            }
            Err(e) => {
                tracing::warn!(run, error = %e, "state.json cannot be read: ignored");
                Ok(None)
            }
        }
    }

    /// Write the state (to a temporary file, then renamed over `state.json`).
    ///
    /// # Errors
    ///
    /// [`EnvError::Refused`] for a bad run id; [`EnvError::Io`].
    pub(crate) fn write(&self, root: &Path) -> Result<(), EnvError> {
        let dir = run_dir(root, &self.run)?;
        std::fs::create_dir_all(&dir)?;
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        let tmp = dir.join("state.json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(tmp, dir.join("state.json"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> State {
        let mut state = State::building("run-1", "coder-a");
        state.phase = Phase::Ready;
        state.config_source = Some(".devcontainer/devcontainer.json".into());
        state.config_digest = "abc".into();
        state.slots = vec![
            SlotRecord {
                dir: "devbox".into(),
                seq: 1,
                mirror: Some("/work/git/h/o/devbox.git".into()),
            },
            SlotRecord {
                dir: "notes".into(),
                seq: 2,
                mirror: None,
            },
        ];
        state.container_id = Some("0123".into());
        state.image = Some("localhost/vsc-devbox-1-uid:latest".into());
        state.keep_id = true;
        state.model_key = true;
        state.tools = Some("/work/environments/.tools/ab".into());
        state
    }

    #[test]
    fn a_state_round_trips_through_the_file() {
        let root = tempfile::tempdir().unwrap();
        let state = sample();
        assert_eq!(State::read(root.path(), "run-1").unwrap(), None);
        state.write(root.path()).unwrap();
        assert_eq!(
            State::read(root.path(), "run-1").unwrap(),
            Some(state.clone())
        );
        let text =
            std::fs::read_to_string(root.path().join("environments/run-1/state.json")).unwrap();
        assert!(
            text.contains("\"phase\": \"ready\"") && text.contains("\"version\": 1"),
            "{text}"
        );
        assert!(
            !root
                .path()
                .join("environments/run-1/state.json.tmp")
                .exists()
        );
        // Written again, it is replaced.
        let mut broken = state;
        broken.phase = Phase::Broken;
        broken.error = Some(StoredError::Refused {
            reason: "privileged".into(),
        });
        broken.write(root.path()).unwrap();
        assert_eq!(State::read(root.path(), "run-1").unwrap(), Some(broken));
    }

    #[test]
    fn a_state_that_cannot_be_read_counts_as_none() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("environments/run-1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("state.json"), "{ not json").unwrap();
        assert_eq!(State::read(root.path(), "run-1").unwrap(), None);
        let mut other = sample();
        other.version = 99;
        std::fs::write(
            dir.join("state.json"),
            serde_json::to_string(&other).unwrap(),
        )
        .unwrap();
        assert_eq!(State::read(root.path(), "run-1").unwrap(), None);
    }

    #[test]
    fn only_a_run_id_that_is_one_safe_segment_names_a_directory() {
        let root = Path::new("/work");
        assert_eq!(
            run_dir(root, "018f3a2b-7c1d-7000").unwrap(),
            Path::new("/work/environments/018f3a2b-7c1d-7000")
        );
        for bad in [
            "",
            ".",
            "..",
            "../x",
            "a/b",
            ".hidden",
            "a b",
            &"x".repeat(200),
        ] {
            assert!(
                matches!(run_dir(root, bad), Err(EnvError::Refused(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn the_errors_that_are_kept_come_back_as_they_were() {
        let errors = [
            EnvError::Config {
                file: PathBuf::from(".devcontainer.json"),
                reason: "no image".into(),
            },
            EnvError::Refused("privileged".into()),
            EnvError::Build {
                reason: "exit 1".into(),
                log_tail: "boom".into(),
            },
            EnvError::Timeout {
                phase: "setup",
                secs: 900,
            },
        ];
        for error in errors {
            let stored = StoredError::of(&error).unwrap();
            let json = serde_json::to_string(&stored).unwrap();
            let back: StoredError = serde_json::from_str(&json).unwrap();
            assert_eq!(back.to_error().to_string(), error.to_string());
        }
        // A retry may get past these, so they are not kept.
        assert!(StoredError::of(&EnvError::Lost).is_none());
        assert!(StoredError::of(&EnvError::Unavailable("down".into())).is_none());
    }

    #[test]
    fn slots_are_compared_with_their_order() {
        let state = sample();
        let mut same = state.slots.clone();
        assert!(state.has_slots(&same));
        same.reverse();
        assert!(!state.has_slots(&same));
        same.truncate(1);
        assert!(!state.has_slots(&same));
    }
}
