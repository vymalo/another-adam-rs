//! [`ToolSource`]: tools that are not known when the agent is built.

use std::sync::Arc;
use std::time::Duration;

use adam_core::RunId;
use adam_model::ToolSpec;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::tool::{ToolCtx, ToolError, ToolOutput};

/// The most tools the sources of an agent may offer on one model turn, all sources together. The
/// ones past it are left out, with a warning: a source that lists without bound would fill the
/// model's context with tool definitions.
pub const MAX_SOURCE_TOOLS: usize = 64;

/// What a [`ToolSource`] says, when it lists a tool, about how a call to it is carried out: not for
/// the model, for the agent and for the source's own [`call`](ToolSource::call) later.
///
/// The agent keeps the notes of the turn in the run's state (journaled with the model's answer), so
/// a call made in a later transition, by another worker, after a restart, reads what the listing
/// said without listing again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolNote {
    /// The tool's name, as listed.
    pub tool: String,
    /// The system behind the source reports each call of this tool as a step itself, so the agent
    /// reports none of its own (it would be drawn twice).
    #[serde(default, skip_serializing_if = "is_false")]
    pub reports_step: bool,
    /// The longest the system behind the source lets a call of this tool run, in milliseconds: how long
    /// the source should wait for it, within its own limits. `None`: the source's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// What the step of a call of this tool is called, when the listing gave the tool a human title
    /// (`Search the web` for `search__web_search`). `None`: the tool's name. An agent's own tool
    /// says it with [`Tool::step_style`](crate::Tool::step_style); this is for a source's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl ToolNote {
    /// A note for `tool` that says nothing yet (such a note is not kept).
    pub fn new(tool: impl Into<String>) -> Self {
        Self {
            tool: tool.into(),
            reports_step: false,
            timeout_ms: None,
            label: None,
        }
    }

    /// Call the step of this tool's calls `label` instead of its name.
    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// The system behind the source reports this tool's calls as steps.
    #[must_use]
    pub fn reporting_steps(mut self) -> Self {
        self.reports_step = true;
        self
    }

    /// A call of this tool may run `timeout` at most.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout_ms = Some(u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX));
        self
    }

    /// The time a call may run, if the listing said.
    pub fn timeout(&self) -> Option<Duration> {
        self.timeout_ms.map(Duration::from_millis)
    }

    /// Whether the note says anything (a note that does not is not kept).
    pub fn is_meaningful(&self) -> bool {
        self.reports_step || self.timeout_ms.is_some() || self.label.is_some()
    }
}

/// What a [`ToolSource`] offers for one model turn: the tools, and what it says about them
/// ([`ToolNote`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Listing {
    /// The tools to show the model.
    pub specs: Vec<ToolSpec>,
    /// What the source says about some of them. A note for a tool that is not in `specs`, or that
    /// the agent leaves out (a name taken, past the limit), is dropped.
    pub notes: Vec<ToolNote>,
}

impl Listing {
    /// A listing of `specs` with no notes.
    pub fn new(specs: Vec<ToolSpec>) -> Self {
        Self {
            specs,
            notes: Vec::new(),
        }
    }

    /// Say something about a tool of this listing.
    #[must_use]
    pub fn with_note(mut self, note: ToolNote) -> Self {
        self.notes.push(note);
        self
    }
}

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
///   (or an earlier source) already has is left out, with a warning (at debug level when the source
///   [expects the repeat](Self::expects_repeat)). At most [`MAX_SOURCE_TOOLS`]
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

    /// The tools to offer on this turn **and what the source says about them** ([`ToolNote`]): which
    /// of its calls the system behind it reports as steps itself, and how long a call may run. The
    /// agent calls this, not [`specs`](Self::specs). The default is `specs` and no notes; a source
    /// that has notes lists once, here, and answers [`specs`](Self::specs) from the same listing.
    async fn listing(&self, ctx: &SourceCtx) -> Listing {
        Listing::new(self.specs(ctx).await)
    }

    /// Words to add to the agent's instructions on this turn, when the run's context calls for them
    /// (the agents a person mentioned, for instance). `None`, the default, adds nothing. Like
    /// [`specs`](Self::specs) it runs only when the model is really called, inside the journaled
    /// step: text for the model to read, never what a call does. The agent appends what the
    /// sources say, in their order, after its own instructions, each part set apart by a blank line.
    async fn instructions(&self, ctx: &SourceCtx) -> Option<String> {
        let _ = ctx;
        None
    }

    /// Change how the tools offered on this model turn are described, once every source has listed
    /// its own: `specs` holds the agent's own tools, then each source's, as the model is about to be
    /// shown them. A source that knows something the tool's author could not (what the screen of this
    /// conversation can draw, for the description of a tool that draws on it) rewrites the
    /// `description` of that tool here.
    ///
    /// Like [`specs`](Self::specs) it runs only when the model is really called, and a replay reads
    /// nothing: it is for text the model reads, never for what a call does. Leave the names and
    /// the schemas alone (the model's calls are matched to the tools by name), and fail to
    /// nothing: a source that cannot tell leaves the descriptions as they are. The default does.
    async fn refine(&self, ctx: &SourceCtx, specs: &mut [ToolSpec]) {
        let _ = (ctx, specs);
    }

    /// Whether `name`, a tool this source listed and the agent leaves out because `taken` (the
    /// tools already offered on this turn) has it, is a repeat the source **expects**: it offers
    /// whatever the system behind it has, among which the agent may already have a tool of its own
    /// (a server the agent connects itself and a conversation attaches too). The agent logs such
    /// a clash at debug level instead of warning at every model turn. The default, `false`, is a
    /// clash nobody expected. The tool is left out either way.
    fn expects_repeat(&self, name: &str, taken: &[ToolSpec]) -> bool {
        let _ = (name, taken);
        false
    }

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

/// Let each of `sources` refine the descriptions of `specs`, the tools of this turn, in order.
pub(crate) async fn refined(sources: &[DynToolSource], ctx: &SourceCtx, specs: &mut [ToolSpec]) {
    for source in sources {
        source.refine(ctx, specs).await;
    }
}

/// The tools `sources` offer for this turn, in order, without the names in `taken` (the agent's
/// own tools) or offered by an earlier source, and at most [`MAX_SOURCE_TOOLS`] of them, with the
/// notes of the ones that were kept (the meaningful ones only).
pub(crate) async fn offered(
    sources: &[DynToolSource],
    ctx: &SourceCtx,
    taken: &[ToolSpec],
) -> (Vec<ToolSpec>, Vec<ToolNote>) {
    let mut out: Vec<ToolSpec> = Vec::new();
    let mut notes: Vec<ToolNote> = Vec::new();
    for source in sources {
        let Listing {
            specs,
            notes: offered_notes,
        } = source.listing(ctx).await;
        let first = out.len();
        for spec in specs {
            if spec.name.trim().is_empty() {
                tracing::warn!("a tool source offered a tool with no name: left out");
                continue;
            }
            if taken.iter().chain(&out).any(|t| t.name == spec.name) {
                let known: Vec<ToolSpec> = taken.iter().chain(&out).cloned().collect();
                if source.expects_repeat(&spec.name, &known) {
                    tracing::debug!(tool = %spec.name, "a tool source offered a tool the agent already has: left out");
                } else {
                    tracing::warn!(tool = %spec.name, "a tool source offered a name that is already taken: left out");
                }
                continue;
            }
            if out.len() >= MAX_SOURCE_TOOLS {
                tracing::warn!(
                    limit = MAX_SOURCE_TOOLS,
                    "the tool sources offer more tools than the limit: the rest are left out"
                );
                keep_notes(&mut notes, offered_notes, &out[first..]);
                return (out, notes);
            }
            out.push(spec);
        }
        keep_notes(&mut notes, offered_notes, &out[first..]);
    }
    (out, notes)
}

/// The meaningful `from` notes whose tool is among `kept`, added to `notes`.
fn keep_notes(notes: &mut Vec<ToolNote>, from: Vec<ToolNote>, kept: &[ToolSpec]) {
    for note in from {
        if note.is_meaningful()
            && kept.iter().any(|spec| spec.name == note.tool)
            && !notes.iter().any(|n| n.tool == note.tool)
        {
            notes.push(note);
        }
    }
}

/// What the sources add to the agent's instructions for this turn, in order, as one text (blank-line
/// separated); `None` when none says anything.
pub(crate) async fn instructed(sources: &[DynToolSource], ctx: &SourceCtx) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for source in sources {
        if let Some(text) = source.instructions(ctx).await
            && !text.trim().is_empty()
        {
            parts.push(text.trim().to_owned());
        }
    }
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}
