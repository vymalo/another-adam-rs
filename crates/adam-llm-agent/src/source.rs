//! [`ToolSource`]: tools that are not known when the agent is built.

use std::sync::Arc;
use std::time::Duration;

use adam_core::RunId;
use adam_model::ToolSpec;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::access::ToolAccess;
use crate::conversation::{Loaded, Loader};
use crate::session_tools::SessionNarrowing;
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
    root_run: RunId,
    /// The run whose workspace the run reads (ADR 0045 slice 7): the root unless the run's session
    /// names another.
    workspace_run: RunId,
    conversation_id: Option<String>,
    context: Arc<Map<String, Value>>,
    task: Option<Arc<str>>,
    loaded: Arc<[Loaded]>,
    removed_tools: Arc<[String]>,
}

impl SourceCtx {
    pub(crate) fn new(
        run_id: RunId,
        root_run: RunId,
        conversation_id: Option<String>,
        context: Arc<Map<String, Value>>,
    ) -> Self {
        Self {
            run_id,
            root_run,
            workspace_run: root_run,
            conversation_id,
            context,
            task: None,
            loaded: Arc::default(),
            removed_tools: Arc::default(),
        }
    }

    /// This context with the run's task: the words of the first part of its first message
    /// ([`Conversation::task`](crate::Conversation::task)), the same text at every turn and at every
    /// call of the run. The agent sets it on every context it makes; a source reads it with
    /// [`task`](Self::task). A test may set it on a [`detached`](Self::detached) context.
    #[must_use]
    pub fn with_task(mut self, task: Option<&str>) -> Self {
        self.task = task.map(Arc::from);
        self
    }

    /// A context detached from any run, for unit-testing a source: a fresh run id, no
    /// conversation, and `context` as the run's inbound context.
    pub fn detached(context: Map<String, Value>) -> Self {
        let run = RunId::new();
        Self::new(run, run, None, Arc::new(context))
    }

    /// Like [`detached`](Self::detached), for a run that is a child of the run `root`.
    pub fn detached_under(root: RunId, context: Map<String, Value>) -> Self {
        Self::new(RunId::new(), root, None, Arc::new(context))
    }

    /// The run the model is about to be called for.
    pub fn run_id(&self) -> RunId {
        self.run_id
    }

    /// The root of that run's parent chain: the run itself when it is not a child run
    /// ([`ToolCtx::root_run_id`](crate::ToolCtx::root_run_id)). What a source that keeps state per
    /// task (the tier a person set, the notes of the task) reads for a subagent.
    pub fn root_run_id(&self) -> RunId {
        self.root_run
    }

    /// This context with the run whose workspace the run reads, when it is not
    /// [`root_run_id`](Self::root_run_id): a [`ToolSource`] that lists what a worktree holds reads
    /// it, so that a round a person started lists the repository of the task it serves.
    #[must_use]
    pub fn with_workspace_run(mut self, run: RunId) -> Self {
        self.workspace_run = run;
        self
    }

    /// The run whose workspace this run works in: [`root_run_id`](Self::root_run_id) unless the
    /// run's session names another ([`ToolCtx::workspace_run_id`](crate::ToolCtx::workspace_run_id)).
    /// What a source that looks at the worktree reads; the notes stay on the root.
    pub fn workspace_run_id(&self) -> RunId {
        self.workspace_run
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

    /// The ceiling a session put on the access tier of this round
    /// ([`SESSION_ACCESS_TIER_CONTEXT_KEY`](crate::SESSION_ACCESS_TIER_CONTEXT_KEY)), `None` when
    /// it set none. A source that enforces a tier takes the lower of it and the tier of the run, so
    /// a `plan` session of an `auto` parent is held to `plan` (ADR 0054, slice 8).
    pub fn session_ceiling(&self) -> Option<adam_model::AccessTier> {
        adam_runtime::access_tier_ceiling(&self.context)
    }

    /// The words the run was started with: the first part of its first message
    /// ([`Conversation::task`](crate::Conversation::task)), the same text at every turn and at every
    /// call of the run, and in a run that continues it. `None` when the first message has no words.
    ///
    /// A source that loads something only when the task asks for it (an MCP server's `when`) reads
    /// this, so what it offers is decided before the run's first model call and never moves.
    pub fn task(&self) -> Option<&str> {
        self.task.as_deref()
    }

    /// This context with the groups and tools the run has loaded so far, in the order it loaded
    /// them ([`Conversation::loaded`](crate::Conversation::loaded)): the agent sets it on every
    /// context it makes, from the state as it stands before the turn, so a group loaded in the
    /// turn before is not withheld from this one. A test may set it on a
    /// [`detached`](Self::detached) context.
    #[must_use]
    pub fn with_loaded(mut self, loaded: impl Into<Arc<[Loaded]>>) -> Self {
        self.loaded = loaded.into();
        self
    }

    /// This context with the loaded tools the run no longer holds
    /// ([`Conversation::removed_tools`](crate::Conversation::removed_tools)): a member its group
    /// shrank out of, or a task-loaded member the tier hid. An explicit load's tier-hidden member
    /// is not one. A source withholds a call to one. A test may set it on a
    /// [`detached`](Self::detached) context.
    #[must_use]
    pub fn with_removed_tools(mut self, removed: impl Into<Arc<[String]>>) -> Self {
        self.removed_tools = removed.into();
        self
    }

    /// The groups and tools the run has loaded so far, in the order it loaded them: a source that
    /// withholds a group's tools stops withholding it once its loader is here, and offers its tools
    /// from [`appended`](ToolSource::appended) instead.
    pub fn loaded(&self) -> &[Loaded] {
        &self.loaded
    }

    /// The loaded tools the run no longer holds: a member its group shrank out of, or a
    /// task-loaded member the access tier hid (permanent for the run). An explicit load's
    /// tier-hidden member is **not** here: the tier is per message. A source withholds a call to
    /// one.
    pub fn removed_tools(&self) -> &[String] {
        &self.removed_tools
    }
}

/// A tool a [`ToolSource`] holds back on one turn, and what to tell the model if it calls it anyway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Withheld {
    /// The tool's name: the agent's own tool or one of a source's.
    pub tool: String,
    /// What the model is told when it calls the tool: the result of the call, as an error.
    pub reason: String,
}

impl Withheld {
    /// `tool` is held back; a call to it answers `reason`.
    pub fn new(tool: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            tool: tool.into(),
            reason: reason.into(),
        }
    }
}

/// The tools a [`ToolSource`] can offer, when it can name them **before a run calls it** (ADR 0054,
/// decision 4): what a session's `tools` and `deny_tools` are checked against, beside the agent's
/// own names.
///
/// A session's bounds are made at startup, before any run exists, so a source answers only what it
/// knows then: a folder's own tools, a server it connected and listed, the members of a group. A
/// source whose tools are another system's, and change with the conversation, answers
/// [`Unknown`](Self::Unknown): a session's set is then incomplete, and
/// `SessionBounds::admit` checks the shape of an entry alone, never that it matches a tool.
///
/// **A name may be a shape** (the owner's answer to ANR-Q155, 2026-11-28). A declared name
/// containing `*` stands for any run of characters, in the same syntax as a folder's `tools:`
/// (`SessionBounds`'s `glob`), so a source that relays another system's tools can name the family
/// it may offer and still be complete: the thread-tools source declares `*__*` for the
/// `<server>__<tool>` tools of the servers a person attached. A shape is what **can** arrive, not
/// what one turn offers, like every other name here.
///
/// [ADR 0054]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Declared {
    /// Every tool the source can offer, by name, each once: what it offers on a turn **and** what it
    /// withholds until a load, a mode or the access tier allows it. The set, not the offering of one
    /// turn. A name may be a shape (`*`, see the type's doc).
    All(Vec<String>),
    /// The source cannot name its tools before a run calls it: a remote endpoint that lists per
    /// conversation, or a listing that may change under it. A session's set is then incomplete.
    #[default]
    Unknown,
}

impl Declared {
    /// Whether these are every tool the source can offer.
    pub fn is_complete(&self) -> bool {
        matches!(self, Self::All(_))
    }

    /// The names, when they are all of them.
    pub fn names(&self) -> Option<&[String]> {
        match self {
            Self::All(names) => Some(names),
            Self::Unknown => None,
        }
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
///   are offered. A source's tools are sorted by name before that cap and those clashes, so the
///   tools that survive the cap, and which of two same-named tools of one source is offered (the
///   first in the listing), do not depend on the order the source lists in.
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

    /// The tools this source's system has given the run **during itself**, in the order it loaded
    /// them, to be offered **after every other tool** of the turn: a load appends to the tools array
    /// and never inserts into it, so the request before stays a byte prefix of the one after it
    /// (ADR 0044, decision 1). A tool named here is taken out of the place it would have had (the
    /// agent's own list, or this source's listing) and put at the end; the source must therefore
    /// also stop withholding it from [`withhold`](Self::withhold). The order is the source's and is
    /// never sorted: the second load follows the first.
    ///
    /// Read inside the journaled step of the model call, from the run's state
    /// ([`SourceCtx::loaded`]), so a replay offers the same array. The default is none,
    /// which is what a source written before this method existed offers.
    async fn appended(&self, ctx: &SourceCtx) -> Vec<ToolSpec> {
        let _ = ctx;
        Vec::new()
    }

    /// The loaders this source still offers on this turn, with what each holds: the run's first
    /// turn records them in [`Conversation::loaders`](crate::Conversation::loaders), so a
    /// compaction's marker says what is left to load without asking the live source. Read inside
    /// the journaled step of the model call, like [`appended`](Self::appended). A loader the task
    /// already loads is not here: it is loaded, not available. The default is none, which is what a
    /// source written before loaders existed offers.
    async fn loaders(&self, ctx: &SourceCtx) -> Vec<Loader> {
        let _ = ctx;
        Vec::new()
    }

    /// The groups the run's **task** loaded, with what each holds: journaled on the run's first
    /// turn (`Conversation::note_task_loaded`), so a
    /// compaction's recap names them. They record no growth: their tools were never withheld. The
    /// default is none, which is what a source written before groups existed offers.
    async fn loaded_by_task(&self, ctx: &SourceCtx) -> Vec<Loaded> {
        let _ = ctx;
        Vec::new()
    }

    /// The loaded tools this source **still holds**, as the source knows them: a group's current
    /// members, minus the journaled removals. The loop journals a loaded tool that is not here as
    /// removed (the group shrank); a tool the access tier hides on this message is still here,
    /// because the tier is per message. `None`, the default, says the source does not track its
    /// loaded tools, and the loop falls back to what the request carried before a source withheld
    /// any.
    async fn loaded_tools(&self, ctx: &SourceCtx) -> Option<Vec<String>> {
        let _ = ctx;
        None
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

    /// Words this source adds to the agent's instructions **during** the run, after everything
    /// [`instructions`](Self::instructions) says: the words of a system the run loaded during
    /// itself, which must follow the prompt and never be inserted into it, or the request before
    /// would stop being a byte prefix of the one after (ADR 0044, decision 1). `None`, the default,
    /// adds nothing, which is what a source written before this method existed offers.
    async fn appended_instructions(&self, ctx: &SourceCtx) -> Option<String> {
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

    /// The tools this source can offer, when it can name them **before a run calls it**: a session's
    /// `tools` and `deny_tools` are checked against them, with the agent's own names (ADR 0054,
    /// decision 4; `SessionBounds::tools_complete` in `adam-runtime`).
    ///
    /// [`Declared::All`] is the whole set: a tool the source withholds on a turn (a mode that does
    /// not list it, a group a run has not loaded, an access tier that is too low) is named too,
    /// because a later turn of the run may offer it. A name may be a **shape**: a declared name
    /// containing `*` stands, in the folder's syntax (`*` is any run of characters), for a family
    /// the source can offer, so `*__*` names the relayed `<server>__<tool>` tools of a server a
    /// person attached without naming each one. `SessionBounds::admit` admits an entry that selects
    /// a concrete declared name with its pattern, or that a declared shape matches as a string.
    /// [`Declared::Unknown`], the default, says the source cannot name its tools: a remote endpoint
    /// whose listing is another system's, or one that changes with the conversation. A session's
    /// set is then incomplete and `admit` checks the shape of an entry alone. The default is what a
    /// source written before this method existed answers, so adding it breaks no implementer.
    fn declared(&self) -> Declared {
        Declared::Unknown
    }

    /// Which of `tools` this source holds back on this turn, **whoever offers them**: the agent's
    /// own, a source's, one of an MCP server's. `tools` are their names: all of those the model is
    /// about to be shown, or the one it is calling. The agent leaves the ones named out of what the
    /// model is shown, and **again refuses a call to one of them** at the moment it would run (the
    /// model may name a tool it was not shown, or one it was shown a turn ago), with the
    /// [`Withheld::reason`] as the call's error result. For a limit the run's context sets, such as
    /// the access tier a person picked for a message: the prompt tells the model the limit, this
    /// makes it hold. A source answers with names out of `tools` only.
    ///
    /// Read from the run's context at both moments (a limit that changes takes effect on the next
    /// turn and the next call), inside the journaled step, so a replay reads nothing. It has no
    /// error to return: a source that cannot tell holds nothing back. The default holds nothing
    /// back, so a source written before this method keeps working.
    async fn withhold(&self, ctx: &SourceCtx, tools: &[&str]) -> Vec<Withheld> {
        let _ = (ctx, tools);
        Vec::new()
    }

    /// What a call to the tool `name` with `args` shares with the other calls of its model turn,
    /// as [`Tool::access`](crate::Tool::access) says for the agent's own tools. Answer
    /// [`ToolAccess::Independent`] or [`ToolAccess::Uses`] **only for a tool this source offers**
    /// and [`ToolAccess::Alone`] for any other name: the agent takes the first source that answers
    /// something else. The default is `Alone`, so a source written before this method existed keeps
    /// its calls one at a time. A source that wraps another must forward it.
    fn access(&self, name: &str, args: &Value) -> ToolAccess {
        let _ = (name, args);
        ToolAccess::Alone
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

/// The tools `sources` offer for this turn, source by source and each source's by name, without the names in `taken` (the agent's
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
        // By name, a stable sort, and before the cap and the name clashes below: the order a source
        // lists in (an MCP server's, across reconnects) is not one to build the cached start of a
        // request on, and it decides which tools survive `MAX_SOURCE_TOOLS` and which of two
        // same-named tools of one source is offered (the first of them in the listing).
        let mut specs = specs;
        specs.sort_by(|a, b| a.name.cmp(&b.name));
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

/// The groups the sources say the task loaded, each loader once, in source order: what the run's
/// first turn journals in [`Conversation::loaded`](crate::Conversation::loaded).
pub(crate) async fn loaded_by_task(sources: &[DynToolSource], ctx: &SourceCtx) -> Vec<Loaded> {
    let mut out: Vec<Loaded> = Vec::new();
    for source in sources {
        for one in source.loaded_by_task(ctx).await {
            if !out.iter().any(|present| present.loader == one.loader) {
                out.push(one);
            }
        }
    }
    out
}

/// The loaded tools the sources still hold, each name once, in source order: `None` when no source
/// tracks them.
pub(crate) async fn loaded_offered(
    sources: &[DynToolSource],
    ctx: &SourceCtx,
) -> Option<Vec<String>> {
    let mut out: Option<Vec<String>> = None;
    for source in sources {
        // One source that does not track its loaded tools makes the whole set unknown: the loop
        // must fall back to what the request carried, not treat that source's tools as gone.
        let tools = source.loaded_tools(ctx).await?;
        let out = out.get_or_insert_with(Vec::new);
        for tool in tools {
            if !out.contains(&tool) {
                out.push(tool);
            }
        }
    }
    out
}

/// The loaders the sources still offer on this turn, each name once, in source order: what the
/// run's first turn records in [`Conversation::loaders`](crate::Conversation::loaders).
pub(crate) async fn loaders_offered(sources: &[DynToolSource], ctx: &SourceCtx) -> Vec<Loader> {
    let mut out: Vec<Loader> = Vec::new();
    for source in sources {
        for loader in source.loaders(ctx).await {
            if !out.iter().any(|one| one.name == loader.name) {
                out.push(loader);
            }
        }
    }
    out
}

/// The tools the sources say the run has loaded during itself, in their order, each name once: the
/// suffix the tools array of this turn grows by, never an insertion.
pub(crate) async fn appended_by(sources: &[DynToolSource], ctx: &SourceCtx) -> Vec<ToolSpec> {
    let mut out: Vec<ToolSpec> = Vec::new();
    for source in sources {
        for spec in source.appended(ctx).await {
            if !out.iter().any(|one| one.name == spec.name) {
                out.push(spec);
            }
        }
    }
    out
}

/// What the run holds back of `tools` on this turn, in order: the names, each once, with the first
/// reason. The session's own narrowing ([`SessionNarrowing`], ADR 0054 slice 3) comes **first**,
/// then each source's holds. A name a source answers that is not among `tools` is not kept.
pub(crate) async fn withheld(
    narrowing: &SessionNarrowing,
    sources: &[DynToolSource],
    ctx: &SourceCtx,
    tools: &[&str],
) -> Vec<Withheld> {
    let mut held: Vec<Withheld> = narrowing.held(tools);
    for source in sources {
        for one in source.withhold(ctx, tools).await {
            if tools.contains(&one.tool.as_str()) && !held.iter().any(|h| h.tool == one.tool) {
                held.push(one);
            }
        }
    }
    held
}

/// What the sources add to the agent's instructions for this turn, in order, as one text (blank-line
/// separated); `None` when none says anything. What a source gave the run **during** itself
/// ([`ToolSource::appended_instructions`]) goes after everything else, the way its tools do: a load
/// appends to the prompt and never inserts into it.
pub(crate) async fn instructed(sources: &[DynToolSource], ctx: &SourceCtx) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for source in sources {
        if let Some(text) = source.instructions(ctx).await
            && !text.trim().is_empty()
        {
            parts.push(text.trim().to_owned());
        }
    }
    for source in sources {
        if let Some(text) = source.appended_instructions(ctx).await
            && !text.trim().is_empty()
        {
            parts.push(text.trim().to_owned());
        }
    }
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}
