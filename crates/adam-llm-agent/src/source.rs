//! [`ToolSource`]: tools that are not known when the agent is built.

use std::sync::Arc;

use adam_core::RunId;
use adam_model::ToolSpec;
use async_trait::async_trait;
use serde_json::{Map, Value};

use crate::tool::{ToolCtx, ToolError, ToolOutput};

/// The most tools the sources of an agent may offer on one model turn, all sources together. The
/// ones past it are left out, with a warning: a source that lists without bound would fill the
/// model's context with tool definitions.
pub const MAX_SOURCE_TOOLS: usize = 64;

/// What a [`ToolSource`] sees of the run when the model is about to be called.
///
/// Owned and cheap to clone.
#[derive(Debug, Clone)]
pub struct SourceCtx {
    run_id: RunId,
    conversation_id: Option<String>,
    context: Arc<Map<String, Value>>,
}

impl SourceCtx {
    pub(crate) fn new(
        run_id: RunId,
        conversation_id: Option<String>,
        context: Arc<Map<String, Value>>,
    ) -> Self {
        Self {
            run_id,
            conversation_id,
            context,
        }
    }

    /// A context detached from any run, for unit-testing a source: a fresh run id, no
    /// conversation, and `context` as the run's inbound context.
    pub fn detached(context: Map<String, Value>) -> Self {
        Self::new(RunId::new(), None, Arc::new(context))
    }

    /// The run the model is about to be called for.
    pub fn run_id(&self) -> RunId {
        self.run_id
    }

    /// The conversation the run belongs to, if any.
    pub fn conversation_id(&self) -> Option<&str> {
        self.conversation_id.as_deref()
    }

    /// A value of the run's inbound context ([`Conversation::context`](crate::Conversation::context)).
    pub fn context(&self, key: &str) -> Option<&Value> {
        self.context.get(key)
    }

    /// The whole inbound context.
    pub fn context_map(&self) -> &Map<String, Value> {
        &self.context
    }
}

/// A group of tools the agent learns about while it runs, not when it is built: what a caller
/// says it can offer in the messages it sends (the endpoint for a thread's tools, for instance),
/// read again at every model turn.
///
/// A [`Tool`](crate::Tool) has one fixed [`ToolSpec`]. A source lists its tools each time the
/// model is about to be called, and answers the calls the model makes to them, so the tools a
/// model sees can change from one turn to the next: a server attached to the conversation a moment
/// ago is offered on the next turn.
///
/// * [`specs`](Self::specs) runs only when the model is really called. A turn whose model call is
///   already in the journal (a replay after a crash) lists nothing and offers nothing: the recorded
///   answer is used.
/// * [`call`](Self::call) runs inside the journaled step of the call, as a [`Tool`](crate::Tool)
///   does, so a recorded result is never computed again, and a call that dies before it is recorded
///   runs again: a tool of a source must be safe to retry.
/// * The agent's own tools come first and win a name clash: a listed tool whose name an own tool
///   (or an earlier source) already has is left out, with a warning. At most [`MAX_SOURCE_TOOLS`]
///   are offered.
/// * A tool of a source cannot wait on a task of another system
///   ([`ToolError::AwaitRemote`]): the wait is answered with an error result, because the agent
///   asks the tool that started it how it stands, and a source's tool is not known by then. It can
///   ask the person ([`ToolError::NeedsInput`]) and wait for a child run ([`ToolError::AwaitRun`]).
///
/// Sources are chosen when the binary is built ([`LlmAgentBuilder::tool_source`](crate::LlmAgentBuilder::tool_source)),
/// never loaded at run time.
#[async_trait]
pub trait ToolSource: Send + Sync + 'static {
    /// The tools to offer on this model turn, besides the agent's own. A source that cannot
    /// reach what it lists (no credential, a server that is down) offers none: a failure here
    /// must not stop the run, so there is no error to return. Say why in a log line, never with a
    /// credential in it.
    async fn specs(&self, ctx: &SourceCtx) -> Vec<ToolSpec>;

    /// Run the tool `name` with `args`. `None`: this source does not offer a tool of that name,
    /// and the next one is asked; the agent answers a name nobody owns with an error result.
    ///
    /// A source that cannot tell whether a name is its own without asking the system behind it
    /// (a remote list) may answer `Some` for any name it could have offered, with an error result
    /// when the system refuses it; it then belongs last among the sources.
    async fn call(
        &self,
        ctx: &ToolCtx,
        name: &str,
        args: Value,
    ) -> Option<Result<ToolOutput, ToolError>>;
}

/// A shared, type-erased [`ToolSource`].
pub type DynToolSource = Arc<dyn ToolSource>;

/// The tools `sources` offer for this turn, in order, without the names in `taken` (the agent's
/// own tools) or offered by an earlier source, and at most [`MAX_SOURCE_TOOLS`] of them.
pub(crate) async fn offered(
    sources: &[DynToolSource],
    ctx: &SourceCtx,
    taken: &[ToolSpec],
) -> Vec<ToolSpec> {
    let mut out: Vec<ToolSpec> = Vec::new();
    for source in sources {
        for spec in source.specs(ctx).await {
            if spec.name.trim().is_empty() {
                tracing::warn!("a tool source offered a tool with no name: left out");
                continue;
            }
            if taken.iter().chain(&out).any(|t| t.name == spec.name) {
                tracing::warn!(tool = %spec.name, "a tool source offered a name that is already taken: left out");
                continue;
            }
            if out.len() >= MAX_SOURCE_TOOLS {
                tracing::warn!(
                    limit = MAX_SOURCE_TOOLS,
                    "the tool sources offer more tools than the limit: the rest are left out"
                );
                return out;
            }
            out.push(spec);
        }
    }
    out
}
