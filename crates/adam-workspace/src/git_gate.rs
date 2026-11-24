//! The pool of git processes of one process (`MAX_GIT_PROCESSES`, ADR 0065 decision 5).
//!
//! Every `git` this crate runs in this container (one of a workspace, or a [`LocalGit`](crate::LocalGit)
//! of a tool) takes one seat of [`Pool::Git`] while it runs, waiting
//! for it up to the wait [`govern_git`] was given. A git that runs in the git sandbox is another
//! process's and takes none. Nothing is governed until [`govern_git`] is called: a library user
//! that never calls it runs git as before.
//!
//! The gate is one per process, because the workspaces and the tools that run git are many and a
//! pool is a property of the process.

use std::sync::OnceLock;
use std::time::Duration;

use adam_budget::{Admission as _, Ask, ConversationId, DynAdmission, Permit, Pool, Refused};

struct Gate {
    admission: DynAdmission,
    wait: Duration,
}

static GATE: OnceLock<Gate> = OnceLock::new();

/// Make every git process of this process take a seat of [`Pool::Git`] from `admission`, waiting
/// up to `wait` for it. A git that finds no seat in time is refused as busy: `RateLimited`, which
/// a caller retries.
///
/// Once per process: returns `false`, and changes nothing, when it was called before.
pub fn govern_git(admission: DynAdmission, wait: Duration) -> bool {
    GATE.set(Gate { admission, wait }).is_ok()
}

/// A seat for a git that runs in `directory`, which stands for whose share it counts against (a
/// run's worktree is its own). `None` when git is not governed.
pub(crate) async fn seat(directory: Option<&std::path::Path>) -> Result<Option<Permit>, Refused> {
    let Some(gate) = GATE.get() else {
        return Ok(None);
    };
    let conversation = directory.map_or_else(
        || ConversationId::new("git"),
        |dir| ConversationId::new(dir.to_string_lossy().into_owned()),
    );
    gate.admission
        .admit(Ask::new(Pool::Git, conversation).wait(gate.wait))
        .await
        .map(Some)
}
