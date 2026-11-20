//! The port the file and search tools work through: [`CodeWorkspace`] ([ADR 0047], decision 7).
//!
//! A tool asks its workspace for the [`Place`] it works in (the directory a `repo` argument names,
//! or the only one), and for what depends on whose workspace it is: the project instructions a path
//! shows for the first time, and what follows a write ([`CodeWorkspace::after_write`]).
//!
//! The coder implements it over the worktrees of a run (`adam_coder::ToolEnv`); a host implements
//! it over a person's own directory (slice 7), which a run never owns, cleans or restores.
//!
//! The commands (`run_command`, `run`) need more of the workspace (a session to run in, a guard that
//! records what a command changed and, where the workspace owns the place, undoes it). They get a
//! port that extends this one, [`CommandWorkspace`](crate::commands::CommandWorkspace), and takes a
//! [`Place`] from the same [`CodeWorkspace::place`], so a command and a file tool find a place and
//! refuse a write the same way.
//!
//! [`CodeWorkspace::after_write`] is a **required** method, added when `write_file` and `edit_file`
//! moved here: a breaking change for every implementation (the coder's and the testkit's
//! `DirWorkspace`, the only ones then).
//!
//! A place's `git` is a [`GitAt`] from the workspace's own metadata (`GitAt::slot`,
//! `GitAt::worktree`). A host's directory, which no workspace metadata describes, gets a
//! constructor of its own when a host implementation exists (slice 7); adding it changes nothing
//! here.
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

/// The most bytes a block of [`CodeWorkspace::nested_instructions`] has: the room a tool that shows
/// several files leaves for it.
pub const MAX_INSTRUCTIONS_BLOCK: usize = 12 * 1024;

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
    /// A place named `name`: the directory `git` is asked in, held ([`GitAt::root`]), so the
    /// directory a tool checks a path against is always the one `git` writes in. Writable unless
    /// [`read_only`](Self::read_only).
    pub fn new(name: impl Into<String>, git: GitAt) -> Self {
        Self {
            name: name.into(),
            root: Arc::clone(git.root()),
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

    /// The value the workspace attached, if it is exactly a `T` (an `Arc<T>` attached is not a `T`).
    /// For the implementation's own methods: a tool does not look at it.
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
    /// The place a tool works in: the one `repo` names, as the model wrote it; with no `repo`, the
    /// one place a tool works in by default, which is the only place that may be written (a place
    /// that may only be read is used when it is named, never by default), and none when there are
    /// several or none.
    ///
    /// `Err` is the tool's answer: `Ok` of an **error result** for the model (no workspace yet, which
    /// of several, an unknown name, a place that cannot be written for a [`Write`](Intent::Write)),
    /// or a [`ToolError`]. A [`Write`](Intent::Write) is never given a place that
    /// [is not writable](Place::is_writable).
    async fn place(
        &self,
        ctx: &ToolCtx,
        repo: Option<&str>,
        intent: Intent,
    ) -> Result<Place, Outcome>;

    /// The process's secrets, for what a tool must not let through while it works: a search matches
    /// the scrubbed lines, so a secret is never findable. It is **not** applied to every result by
    /// the tools: the workspace's host scrubs each result before the model sees it (the coder's
    /// `Redacting` layer), and a host that serves these tools does the same.
    fn redactor(&self) -> &Redactor;

    /// The project instructions `path` touches in `place` and that this run has not been shown yet
    /// (`AGENTS.md` of a directory below the place's root), as a block put before a tool's result.
    /// `None` when there are none, or when the workspace does not show any (the default).
    ///
    /// A block returned is **claimed**: the workspace counts it as shown, so the tool puts it in its
    /// result. It is at most [`MAX_INSTRUCTIONS_BLOCK`] bytes, which the tools leave room for.
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

    /// After a write tool changed `paths` (relative to `place`): what the workspace does. Returns
    /// text the tool appends verbatim to its sentence: empty, or starting with ' ' or '\n'.
    ///
    /// **Required**: every implementation says what a write is followed by, even when it is
    /// nothing (`Ok(String::new())`). It is called once per successful write, after the files were
    /// written and before the tool reports progress; a failed write does not call it. The coder
    /// moves its run's check epoch (a file the project ignores is still read by a check) and says
    /// to run the checks again, and, in a scratch project that was published, that the change does
    /// not reach the repository.
    ///
    /// The text goes to the model as part of the tool's answer and is **not redacted by the tool**:
    /// the host scrubs each result (the coder's `Redacting` layer), as for every other answer.
    ///
    /// # Errors
    ///
    /// A [`ToolError`] when what follows the write cannot be done or recorded: the tool returns it,
    /// and the files stay written.
    async fn after_write(
        &self,
        ctx: &ToolCtx,
        place: &Place,
        paths: &[String],
    ) -> Result<String, ToolError>;
}

/// A shared [`CodeWorkspace`].
pub type DynCodeWorkspace = Arc<dyn CodeWorkspace>;
