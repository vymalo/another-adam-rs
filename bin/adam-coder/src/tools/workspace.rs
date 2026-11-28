//! The coder's [`CodeWorkspace`]: the slots of a run's workspace ([`ToolEnv`]), as the file and
//! search tools of `adam-code-tools` see them (ADR 0047, decision 7).
//!
//! A place is a slot: found and refused exactly as [`ToolEnv::slot_to_read`] and
//! [`ToolEnv::slot_to_write`] do, named by its directory, read-only when it is a reference, and
//! carrying the [`Slot`] itself, so the coder's own answers (the nested project instructions, what
//! follows a write) work on what was found.
//!
//! The same `ToolEnv` is the commands' [`CommandWorkspace`]: the run's environment, the deployment's
//! limits (`CHECK_TIMEOUT_SECS`, `CHECK_OUTPUT_TAIL_BYTES`), the coder's kept files, and the guard of
//! a command, [`SlotGuard`]: the slot's [`Snapshot`] before the command, compared and restored after
//! it.

use std::path::Path;
use std::sync::Arc;

use adam_code_tools::git::GitAt;
use adam_code_tools::shell::MissingTool;
use adam_code_tools::{
    CodeWorkspace, CommandGuard, CommandLimits, CommandWorkspace, Guarding, Intent, Outcome, Place,
    Redactor, Settled, Target,
};
use adam_llm_agent::{KeptOutputs, ToolCtx, ToolError};
use adam_workspace::{EnvSession, Slot};
use async_trait::async_trait;

use super::inspect::{Snapshot, cannot_snapshot};
use super::{ToolEnv, background, checks, notes_error, project, scratch};

// A block of the nested instructions fits the room the port promises (`MAX_INSTRUCTIONS_BLOCK`):
// the text, cut at `NESTED_CAP`, and its header and footer. It is exactly that room, the room
// `read_files` has always reserved for a block.
const _: () = assert!(project::NESTED_CAP + 4 * 1024 == adam_code_tools::MAX_INSTRUCTIONS_BLOCK);

/// A slot as a [`Place`].
pub(crate) fn place_of(slot: Slot) -> Place {
    let place = Place::new(slot.dir(), GitAt::slot(&slot));
    let place = if slot.writable() {
        place
    } else {
        place.read_only()
    };
    place.with_handle(slot)
}

#[async_trait]
impl CodeWorkspace for ToolEnv {
    async fn place(
        &self,
        ctx: &ToolCtx,
        repo: Option<&str>,
        intent: Intent,
    ) -> Result<Place, Outcome> {
        let slot = match intent {
            Intent::Read => self.slot_to_read(ctx, repo).await?,
            Intent::Write => self.slot_to_write(ctx, repo).await?,
        };
        Ok(place_of(slot))
    }

    fn redactor(&self) -> &Redactor {
        &self.redactor
    }

    async fn nested_instructions(
        &self,
        ctx: &ToolCtx,
        place: &Place,
        path: Option<&str>,
        target: Target,
    ) -> Result<Option<String>, ToolError> {
        let Some(slot) = place.handle::<Slot>() else {
            return Ok(None);
        };
        let target = match target {
            Target::File => project::Target::File,
            Target::Dir => project::Target::Dir,
            Target::Either => project::Target::Either,
        };
        project::nested(self, ctx, slot, path, target).await
    }

    /// The check epoch moves: a file the project **ignores** does not change the tree id, and a
    /// check may read it, so a failed record from before this change is not reused
    /// (`ChecksNotes::epoch`). A write to a file the project tracks changes the tree anyway; the
    /// bump costs nothing there. Then the model is told to run the checks again and, in a scratch
    /// project that was published, that the change does not reach the repository.
    async fn after_write(
        &self,
        ctx: &ToolCtx,
        place: &Place,
        _paths: &[String],
    ) -> Result<String, ToolError> {
        let run = ctx.root_run_id().to_string();
        self.notes
            .update(&run, |notes| {
                notes.checks.epoch = notes.checks.epoch.saturating_add(1);
            })
            .await
            .map_err(|e| notes_error(&e))?;
        let mut tail = String::from(" Run the checks again before you commit.");
        if let Some(slot) = place.handle::<Slot>() {
            scratch::push_published_note(&mut tail, slot);
        }
        Ok(tail)
    }
}

#[async_trait]
impl CommandWorkspace for ToolEnv {
    async fn session(&self, ctx: &ToolCtx) -> Result<Arc<dyn EnvSession>, ToolError> {
        ToolEnv::session(self, ctx).await
    }

    fn limits(&self) -> CommandLimits {
        CommandLimits {
            timeout: self.settings.check_timeout,
            output_tail: self.settings.check_output_tail,
            output: self.settings.command_output,
        }
    }

    fn governor(&self) -> Option<&adam_code_tools::governor::Governor> {
        self.settings.governor.as_ref()
    }

    fn kept(&self) -> &dyn KeptOutputs {
        &*self.kept
    }

    /// The slot's `Snapshot`, taken now: what a command must leave as it found it. A worktree that
    /// cannot be read, or whose `HEAD` is off the run's own branch, is refused (nothing it does could
    /// be undone), and so is a place the coder did not make, which carries no slot.
    async fn guard(
        &self,
        _ctx: &ToolCtx,
        place: &Place,
        guarding: Guarding,
    ) -> Result<Box<dyn CommandGuard>, String> {
        let Some(slot) = place.handle::<Slot>() else {
            return Err(format!(
                "`{}` is not a slot of this run: nothing was run, because a change a command made \
                 there could not be undone.",
                place.name()
            ));
        };
        let not_done = match guarding {
            Guarding::Look => {
                "Nothing was run, because a command that might change the worktree cannot be \
                 undone."
            }
            Guarding::Make => {
                "Nothing was run, because a command that might change git cannot be undone."
            }
        };
        let before = Snapshot::of_slot(slot)
            .await
            .map_err(|e| cannot_snapshot(e, not_done))?;
        if let Some(refusal) = before.refusal() {
            return Err(refusal);
        }
        Ok(Box::new(SlotGuard {
            slot: slot.clone(),
            before,
            guarding,
        }))
    }

    /// The check epoch moves, as after a write: the files a command keeps, ignored ones included
    /// (`target/`, `node_modules/`), may be what a check reads without the tree id changing, so no
    /// failed check record from before is reused (`ChecksNotes::epoch`). It moves for a command its
    /// guard then undoes too: a change to an ignored file may stay. The text is the published note
    /// of a scratch project.
    async fn after_run(&self, ctx: &ToolCtx, place: &Place) -> Result<String, ToolError> {
        let run = ctx.root_run_id().to_string();
        self.notes
            .update(&run, |notes| {
                notes.checks.epoch = notes.checks.epoch.saturating_add(1);
            })
            .await
            .map_err(|e| notes_error(&e))?;
        let mut tail = String::new();
        if let Some(slot) = place.handle::<Slot>() {
            scratch::push_published_note(&mut tail, slot);
        }
        Ok(tail)
    }

    /// The commands that keep running in the slot (`background::slot_note`): they write in it,
    /// and what they write is not undone.
    fn refusal_note(&self, ctx: &ToolCtx, place: &Place) -> String {
        place.handle::<Slot>().map_or_else(String::new, |slot| {
            background::slot_note(self, &ctx.workspace_run_id().to_string(), slot.dir())
        })
    }

    /// The coder's answer: a missing toolchain, or a dependency of the project with its install
    /// command, said once and then as repeated (`checks::missing_tool_answer`).
    async fn missing_tool_answer(
        &self,
        ctx: &ToolCtx,
        _place: &Place,
        missing: &MissingTool,
        dirs: &[&Path],
        session: &dyn EnvSession,
    ) -> Result<Option<String>, ToolError> {
        checks::missing_tool_answer(self, ctx, missing, dirs, &session.describe().kind)
            .await
            .map(Some)
    }
}

/// The guard of a command in a slot: the [`Snapshot`] before it, and what it is held to.
struct SlotGuard {
    slot: Slot,
    before: Snapshot,
    guarding: Guarding,
}

#[async_trait]
impl CommandGuard for SlotGuard {
    /// A look may change nothing; a make may change the files and nothing of git. What it was not
    /// allowed to change is put back, all of it, its files included. A worktree that cannot be read
    /// after the command is not known to be untouched, and is put back too.
    async fn settle(self: Box<Self>) -> Settled {
        let Self {
            slot,
            before,
            guarding,
        } = *self;
        let Ok(after) = Snapshot::of_slot(&slot).await else {
            return Settled::Unreadable {
                restored: before.restore(&slot).await,
            };
        };
        let allowed = match guarding {
            Guarding::Look => after == before,
            Guarding::Make => before.same_git_as(&after),
        };
        if !allowed {
            return Settled::Undone {
                restored: before.restore(&slot).await,
            };
        }
        if guarding == Guarding::Make && after.files_differ_from(&before) {
            Settled::FilesChanged
        } else {
            Settled::Unchanged
        }
    }
}
