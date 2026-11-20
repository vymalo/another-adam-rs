//! The coder's [`CodeWorkspace`]: the slots of a run's workspace ([`ToolEnv`]), as the file and
//! search tools of `adam-code-tools` see them (ADR 0047, decision 7).
//!
//! A place is a slot: found and refused exactly as [`ToolEnv::slot_to_read`] and
//! [`ToolEnv::slot_to_write`] do, named by its directory, read-only when it is a reference, and
//! carrying the [`Slot`] itself, so the coder's own answers (the nested project instructions) work
//! on what was found.

use adam_code_tools::git::GitAt;
use adam_code_tools::{CodeWorkspace, Intent, Outcome, Place, Redactor, Target};
use adam_llm_agent::{ToolCtx, ToolError};
use adam_workspace::Slot;
use async_trait::async_trait;

use super::ToolEnv;
use super::project;

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
}
