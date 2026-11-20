//! Root runs a step may start: the scheduler's firings (ADR 0060, decision 2).

use adam_core::RunId;

use crate::agent::Inbound;
use crate::runtime::{Runtime, RuntimeError};

/// The conversations a [`RunStarter`] may start runs in: those of schedules and their firings.
pub const SCHEDULE_CONVERSATION_PREFIX: &str = "adam.schedule:";

/// The key of a message's `context` that says **nobody answers this run**, with the value `true`:
/// a firing of a schedule carries it (ADR 0060), and an agent's child runs inherit it with the
/// other protected keys (`adam_llm_agent::PROTECTED_CONTEXT_KEYS`), so a subagent of a firing,
/// which has no conversation of its own, knows it too.
pub const UNATTENDED_CONTEXT_KEY: &str = "adam.unattended";

/// The right of a step to start **root** runs, as an owned, cloneable handle.
///
/// [`Ctx::run_starter`](crate::Ctx::run_starter) makes one from the runtime that is stepping the
/// run. It offers [`Runtime::start_with_id`] and nothing else, and only in conversations under
/// [`SCHEDULE_CONVERSATION_PREFIX`], so a step can neither open a run in a person's conversation
/// nor deliver into one (as [`ChildStarter`](crate::ChildStarter) is bound to its parent).
///
/// The namespace is a convention: a custom authenticator whose subject is `adam.schedule` would
/// map that subject's conversations into it, so none should be given that subject.
///
/// It is transient: a step gets a new one, and holding on to it past the step keeps the runtime
/// alive.
#[derive(Clone)]
pub struct RunStarter {
    runtime: Runtime,
}

impl std::fmt::Debug for RunStarter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunStarter").finish_non_exhaustive()
    }
}

impl RunStarter {
    pub(crate) fn new(runtime: Runtime) -> Self {
        Self { runtime }
    }

    /// Start a run of `agent` under `id` in `conversation_id`: exactly [`Runtime::start_with_id`],
    /// so it is idempotent (`false` when a run with `id` exists, and `input` is then ignored) and
    /// never delivers into another run.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::ForeignConversation`] for a conversation not under
    /// [`SCHEDULE_CONVERSATION_PREFIX`], before anything is read or written;
    /// [`RuntimeError::ConversationBusy`] when the conversation has an open run under another id;
    /// [`RuntimeError::UnknownAgent`]; and the store's errors.
    pub async fn start_with_id(
        &self,
        id: RunId,
        agent: &str,
        input: Inbound,
        conversation_id: &str,
    ) -> Result<bool, RuntimeError> {
        if !conversation_id.starts_with(SCHEDULE_CONVERSATION_PREFIX) {
            return Err(RuntimeError::ForeignConversation {
                conversation_id: conversation_id.to_owned(),
            });
        }
        self.runtime
            .start_with_id(id, agent, input, Some(conversation_id))
            .await
    }
}
