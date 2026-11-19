//! The port the file and search tools work through: [`CodeWorkspace`] ([ADR 0047], decision 7).
//!
//! A tool asks its workspace for the [`Place`] it works in (the directory a `repo` argument names,
//! or the only one), and for what depends on whose workspace it is: the project instructions a path
//! shows for the first time. What the file tools need after a write comes with them.
//!
//! The coder implements it over the worktrees of a run (`adam_coder::ToolEnv`); a host implements
//! it over a person's own directory (slice 7), which a run never owns, cleans or restores.
//!
//! The commands (`run_command`, `run`) need more of the workspace (a session to run in, a guard that
//! records and undoes what a command changed); that is a port of its own, which comes with them.
//!
//! [ADR 0047]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0047-adam-rs-can-be-embedded-in-an-application.md

use std::any::Any;
use std::fmt;
use std::path::Path;
use std::sync::Arc;

use adam_llm_agent::{ToolCtx, ToolError, ToolOutput};
use adam_workspace::SlotDir;
use async_trait::async_trait;

use crate::git::GitAt;
use crate::redact::Redactor;

/// What a tool answers: its output, or the error the runtime classifies.
pub type Outcome = Result<ToolOutput, ToolError>;

/// What a tool means to do in a place: a place that is not [writable](Place::is_writable) is
/// never given for a [`Write`](Intent::Write).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    /// Read, list or search.
    Read,
    /// Create, change or delete a file.
    Write,
}

/// What a path names, for the project instructions it shows ([`CodeWorkspace::nested_instructions`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// A file: its directories are the ones above it.
    File,
    /// A directory: it is touched too.
    Dir,
    /// Either (`search_files` takes both): looked at to tell.
    Either,
}

/// Where a tool works: a directory held open ([`SlotDir`]), the `git` that answers for it
/// ([`GitAt`]), its name as the model writes it, and whether it may be written.
///
/// The workspace may attach a value of its own ([`with_handle`](Self::with_handle)), so that its
/// other methods get back what it found (the coder's `Slot`) without looking it up again.
#[derive(Clone)]
pub struct Place {
    name: String,
    root: Arc<SlotDir>,
    git: GitAt,
    writable: bool,
    handle: Option<Arc<dyn Any + Send + Sync>>,
}

impl Place {
    /// A place named `name` over `root`, asked about with `git`; writable unless
    /// [`read_only`](Self::read_only).
    pub fn new(name: impl Into<String>, root: Arc<SlotDir>, git: GitAt) -> Self {
        Self {
            name: name.into(),
            root,
            git,
            writable: true,
            handle: None,
        }
    }

    /// The same place, which no tool may write (a reference the run only reads).
    #[must_use]
    pub fn read_only(mut self) -> Self {
        self.writable = false;
        self
    }

    /// The same place, carrying `handle` for the workspace's own methods.
    #[must_use]
    pub fn with_handle<T: Any + Send + Sync>(mut self, handle: T) -> Self {
        self.handle = Some(Arc::new(handle));
        self
    }

    /// Its name as the model writes it: what a tool's result calls it (`slot: <name>`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The directory, held: every path a tool reads or writes is relative to it.
    pub fn root(&self) -> &Arc<SlotDir> {
        &self.root
    }

    /// The directory's name on this machine, for messages that are scrubbed of it.
    pub fn path(&self) -> &Path {
        self.git.path()
    }

    /// Where `git` is asked about this place.
    pub fn git(&self) -> &GitAt {
        &self.git
    }

    /// Whether a tool may write here.
    pub fn is_writable(&self) -> bool {
        self.writable
    }

    /// The value the workspace attached, if it is a `T`.
    pub fn handle<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.handle.as_deref()?.downcast_ref()
    }
}

impl fmt::Debug for Place {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Place")
            .field("name", &self.name)
            .field("writable", &self.writable)
            .finish_non_exhaustive()
    }
}

/// The workspace of the file and search tools.
#[async_trait]
pub trait CodeWorkspace: Send + Sync {
    /// The place a tool works in: `repo` as the model wrote it, or the only place when it wrote
    /// none. `Err` is the tool's answer: `Ok` of a message for the model (no workspace yet, which of
    /// several, a place that cannot be written for a [`Write`](Intent::Write)), or a
    /// [`ToolError`]. A [`Write`](Intent::Write) is never given a place that
    /// [is not writable](Place::is_writable).
    async fn place(
        &self,
        ctx: &ToolCtx,
        repo: Option<&str>,
        intent: Intent,
    ) -> Result<Place, Outcome>;

    /// What every text a tool returns goes through: the process's secrets, scrubbed.
    fn redactor(&self) -> &Redactor;

    /// The project instructions `path` touches in `place` and that this run has not been shown yet
    /// (`AGENTS.md` of a directory below the place's root), as a block put before a tool's result.
    /// `None` when there are none, or when the workspace does not show any (the default).
    ///
    /// # Errors
    ///
    /// A [`ToolError`] when the instructions cannot be read or recorded.
    async fn nested_instructions(
        &self,
        _ctx: &ToolCtx,
        _place: &Place,
        _path: Option<&str>,
        _target: Target,
    ) -> Result<Option<String>, ToolError> {
        Ok(None)
    }
}

/// A shared [`CodeWorkspace`].
pub type DynCodeWorkspace = Arc<dyn CodeWorkspace>;
