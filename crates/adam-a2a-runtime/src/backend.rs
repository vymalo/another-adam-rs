//! [`RuntimeTaskBackend`]: the A2A [`TaskBackend`] over an `adam_runtime::Runtime`.

use std::sync::Arc;
use std::time::Duration;

use a2a::{Message, Task, TaskState};
use adam_a2a::{
    ACCESS_TIER_EXTENSION, AccessTiers, BackendError, COMMANDS_EXTENSION, Caller, STEER_EXTENSION,
    TaskBackend, TaskEvent, TaskPage, TaskQuery,
};
use adam_core::{RunId, StoreError};
use adam_error::ErrorClass;
use adam_runtime::{
    AgentError, ArtifactRef, BroadcastSink, Classify, FileSource, RunView, Runtime, RuntimeError,
};
use async_trait::async_trait;
use futures::stream::BoxStream;
use uuid::Uuid;

use crate::convert::{
    InboundFn, PromptFn, decode_conversation, default_inbound, default_prompt, encode_conversation,
    task_from_view,
};
use crate::ids::task_id_for;
use crate::subscribe;
use crate::tier;

/// Default interval at which a subscription re-reads the durable run: the fallback, for a state
/// change that no live event announced. Two seconds, because the events are the mechanism: a
/// worker's commit emits a `Status` event in-process and (with a notifier) to every other process,
/// and the pump polls at once on one. A slower fallback costs nothing when events arrive and saves
/// a full record read per subscriber per interval when they do not.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Reason recorded on a run cancelled through A2A.
const CANCEL_REASON: &str = "canceled by client";

/// How often a submission retries when the conversation's open task finishes
/// under it.
const MAX_JOIN_ATTEMPTS: usize = 5;

/// How many of a message's `referenceTaskIds` are looked at, in order. Each one costs a store
/// read, and a client names one (the task it builds on), so a long list is a mistake or an
/// attempt to make the server read a lot. The ones after these are ignored.
pub const MAX_REFERENCES: usize = 8;

/// A task is a run: `task_id` is the run id, `context_id` the conversation
/// (namespaced by the caller, see below).
///
/// # Mapping
///
/// | A2A | Runtime |
/// |---|---|
/// | `SendMessage` (new) | `Runtime::start_with_id` with [`task_id_for`], conversation `<subject>:<context id>` |
/// | `SendMessage` (new) with `referenceTaskIds` | `Runtime::start_with_id_continuing` from the first reference that qualifies (see below) |
/// | `SendMessage` with `taskId` | `Runtime::deliver` while `input-required`, or while the run is parked on the model (`auth-required`, or `working` on a timer); and, with `steer/v1` activated, while `submitted` or `working` (see "A message for a running task") |
/// | `SendMessage` with `taskId` and `adam.resume: true` in its metadata | `Runtime::deliver` of a wake-up that adds nothing, only while the run is parked on the model (see "A run parked on the model") |
/// | `submitted` | runnable, nothing committed, no worker holds it |
/// | `working` | runnable and a worker holds the run's lease (`RunView::claimed`, from the first claim, before the first commit), or committed at least once, or parked on a timer |
/// | `input-required` | run parked with no timer (`RunView::waiting`); the question comes from [`PromptFn`], or the reason of a park on the model |
/// | `auth-required` | run parked on the model after a refusal of status 401 or 403; the reason is the status message |
/// | `completed` | `Done`; `output.text` is the status message, `RunView::artifacts` are the task artifacts |
/// | `failed` | `Failed`; the error is the status message |
/// | `canceled` | `Failed` with `cancelled: ...` (what `Runtime::cancel` writes) |
/// | `CancelTask` | `Runtime::cancel` |
///
/// # A run parked on the model
///
/// A run whose model call was refused, under a provider that parks ([ADR 0067]), keeps its
/// conversation and waits to make the call again. Its task says so from the run's
/// `state.model_wait`, with the reason as the status message:
///
/// | The park | Task |
/// |---|---|
/// | a refusal of status 401 or 403 (timer or not) | `auth-required` |
/// | until woken (a usage limit reached) | `input-required` |
/// | on a timer | `working`, with a status message |
///
/// A message with the `taskId` of such a task is delivered, timer or not; its text is the person's
/// turn. A message whose metadata holds `adam.resume: true` is a **resume**: it may have no text, it
/// wakes the run and adds nothing to the conversation, and a task that is not parked on the model
/// (a finished or canceled one included) refuses it with `InvalidParams`
/// ([`NOT_PARKED`](crate::NOT_PARKED)), writing nothing. A resume names its `taskId`. Cancelling
/// ends the park as it ends any wait. This is a contract, written in
/// `docs/reference/a2a-server.md`.
///
/// [ADR 0067]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0067-a-model-call-through-an-injector-names-its-run-and-a-refusal-parks-the-run.md
///
/// # Idempotent submission
///
/// A new task's id is derived from the caller, the `contextId` and the
/// message's `messageId` ([`task_id_for`]), so a client that repeats a
/// `SendMessage` (a retry after a crash or a timeout) gets the task the first
/// attempt made, and the agent reads the input once. This holds for a request
/// that started a task; a message that was delivered to an already open task
/// (below) and a follow-up to a `taskId` are not recognised on a repeat: the
/// follow-up is refused as the task is no longer `input-required`, the
/// delivery to an open task is delivered again (the runtime keeps no record of
/// consumed inbound ids).
///
/// A message that carries a `contextId` but no `taskId` while that context
/// still has an open task is delivered to that task (the runtime allows one
/// open run per conversation); once it is finished the message starts a new
/// task in the same context.
///
/// # A message for a running task (`steer/v1`)
///
/// A2A does not say what a message with the `taskId` of a `working` task is. When the request
/// **activated** [`STEER_EXTENSION`] (the card declares it, and the client named it in the
/// `A2A-Extensions` header or in `message.extensions`: `Caller::extensions`), a message for a task
/// that is `submitted` or `working` is **delivered to the open task**, to the run's inbox
/// (`Runtime::deliver`, durable in the store, so it survives a worker crash or a lease that moves),
/// and the answer is the task in its current state. The agent reads it at its next step, and a run
/// that is about to finish takes another step first (`Ctx::reopen_on_arrival`, which the agent loop
/// asks for). Without the activation the message is refused exactly as before
/// (`InvalidParams`, "cannot take a follow-up"), because the specification leaves it undefined.
///
/// | The task | The request | Answer |
/// |---|---|---|
/// | `submitted` or `working` | activated | delivered; the task |
/// | `submitted` or `working` | not activated | `InvalidParams`, as before |
/// | terminal (`completed`, `failed`, `canceled`) | any | `UnsupportedOperation` (A2A's error) |
/// | `submitted` or `working`, another context | activated | `TaskNotFound` |
/// | unknown, or another caller's | any | `TaskNotFound` |
/// | `input-required` | any | the follow-up that resumes it, as plain A2A: the extension changes nothing |
///
/// The backend does not deduplicate: a message sent twice is delivered twice, and **the agent**
/// reads one copy per `messageId` (the inbound's id; `adam-llm-agent` keeps the ids it has read in
/// its state). Declaring the extension on the card is the host's promise that its agent does that
/// and never loses an accepted message.
///
/// # A new task continues the task it references
///
/// A refinement, a follow-up or a rework is a **new** task in the same `contextId`, and A2A says
/// it names the task it builds on in `Message.referenceTaskIds`. Without help the new run would
/// start from nothing and the agent would forget the conversation, so a new task whose message
/// has `referenceTaskIds` starts **continuing** one of them: the new run's first state is
/// `Agent::init_continuing` (`AgentStarter::init_continuing` on a front that only holds the
/// starter) of the referenced run's last state. It is still a new run with a new id, journal and
/// limits, and `task_id_for` is unchanged.
///
/// The backend takes the **first** reference (of the first [`MAX_REFERENCES`]) that is all of:
///
/// * a task of this agent owned by the caller, the rule of every other call ("Ownership" below);
/// * in the **same context** as the new task: the message's `contextId`, so a message without
///   one, which gets a context of its own, continues nothing;
/// * **terminal**: `completed`, `failed` or `canceled` (`rejected`, the fourth terminal A2A
///   state, is never produced here). A task that is `input-required` is not finished: a message
///   for it is the follow-up that resumes it, and what a message with a `contextId` and no
///   `taskId` does while the context has an open task is unchanged (it is delivered to it).
///
/// A reference that is malformed, unknown, someone else's, another context's, still open or
/// unreadable is skipped. The client cannot tell why: it gets a fresh task, as it would for an id
/// that never existed, so a reference is no way to learn whether another caller's task exists.
/// The record is judged from its raw fields (agent, conversation, status) before its state is
/// decoded, so a record the caller does not own is never decoded and cannot fail the request; one
/// of the caller's own that does not decode is skipped with a warning. The operator sees **one
/// `info` line per request that named references and started a fresh task anyway**, with a count
/// per reason (never the ids of other callers' tasks). It is said once, for the outcome the
/// request really had: not when a reference was continued, not when the message was delivered to
/// the open task of its context (a reference means nothing there), and not again when a busy
/// conversation made the request pick a second time; a repeat of a request (same `messageId`)
/// says nothing either. Without any usable reference the task starts from nothing, as before:
/// the backend never guesses "the latest task of the context". Repeating a request (same
/// `messageId`) is idempotent as ever.
///
/// # The access tier (`access-tier/v1`)
///
/// With [`with_access_tiers`](Self::with_access_tiers), a message whose request **activated**
/// [`ACCESS_TIER_EXTENSION`] must name a tier the agent honours in its metadata under that URI,
/// or it is refused (`InvalidParams`, before anything is written). The tier reaches the run as the
/// context key [`CONTEXT_ACCESS_TIER`](crate::CONTEXT_ACCESS_TIER) and the agent keeps to it from
/// its next step; a message that does not activate the extension carries no tier, whatever its
/// metadata says. To a caller that activated it, a task says the tier it works under in its
/// `metadata` (the `SendMessage` answer, `GetTask`, the first frame of a stream) and a stream says
/// it again in a status update when it changes. The state of the run is the truth: the tier said is
/// the one in its context, else the agent's default.
///
/// # Commands (`commands/v1`)
///
/// With [`with_commands`](Self::with_commands), a message that names a command the agent serves is
/// expanded in this request, right after the inbound function, and the expansion is what is
/// stored (see [`Commands`](crate::Commands)): in the metadata under [`COMMANDS_EXTENSION`] when the
/// request activated the extension (an unknown name is `InvalidParams`, before anything is
/// written), or as `/name arguments` at the start of the text, always. A `/word` that is no
/// command is plain text.
///
/// "The caller" is the authenticated subject ([`Caller::subject`]). The anonymous caller of an
/// unauthenticated server is every client at once, so **a message from it never continues
/// anything**: references are ignored (debug log) and the task starts fresh. With token
/// authentication the subject is `token-<index>` in the configured list, so reordering or
/// replacing tokens hands the history of an index to whoever holds it next.
///
/// # Ownership
///
/// The caller's subject is part of the run's conversation id
/// (`<subject>:<context id>`), which is durable, so ownership survives
/// restarts and needs no side table. A task owned by someone else is
/// indistinguishable from one that does not exist.
///
/// # Subscriptions survive restarts
///
/// [`subscribe`](TaskBackend::subscribe) is built from durable state: it
/// takes its snapshot from `Runtime::view` and then polls the view every
/// [`poll_interval`](Self::with_poll_interval), so it works for a task started
/// by another process or before a restart. Live events from the
/// [`BroadcastSink`] only add low latency and intermediate progress: they are
/// de-duplicated against what the durable record reports, and losing them
/// never loses a state change or an artifact. A subscription starts with the
/// run's recent live events (`BroadcastSink::subscribe_run`), so a step that began
/// between `submit` and `subscribe` is not lost.
///
/// # Wiring
///
/// The `events` sink must be the one the runtime was built with
/// (`RuntimeBuilder::event_sink`), otherwise no live events arrive (polling
/// still works). The runtime must have `agent` registered, either as an
/// agent (`RuntimeBuilder::agent`) or as a start-only starter
/// (`RuntimeBuilder::starter`): the backend only starts, delivers to, reads and
/// cancels runs, and never steps one. Workers are run by the caller
/// (`Runtime::run_worker`); a process that registered only the starter has
/// nothing to step, so a worker with the full agent has to run elsewhere.
///
/// `ListTasks` reports `total_size` exactly without a `status` filter and for `completed`, and as
/// an upper bound for the other states (see the `list` module).
#[derive(Clone)]
pub struct RuntimeTaskBackend {
    pub(crate) runtime: Runtime,
    pub(crate) events: BroadcastSink,
    pub(crate) agent: String,
    pub(crate) poll: Duration,
    pub(crate) prompt: PromptFn,
    pub(crate) inbound: InboundFn,
    /// The tiers of `access-tier/v1` this agent honours, when its card declares them.
    pub(crate) access_tiers: Option<AccessTiers>,
    /// The commands of `commands/v1` this agent serves, when its card lists them.
    pub(crate) commands: Option<crate::Commands>,
    /// The helpers of `child-sessions/v1` this agent serves, when its card lists them.
    pub(crate) child_sessions: Option<crate::ChildSessions>,
}

impl std::fmt::Debug for RuntimeTaskBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeTaskBackend")
            .field("agent", &self.agent)
            .field("poll", &self.poll)
            .finish_non_exhaustive()
    }
}

impl RuntimeTaskBackend {
    /// A backend serving `agent` on `runtime`, with live events from `events`.
    pub fn new(runtime: Runtime, events: BroadcastSink, agent: impl Into<String>) -> Self {
        Self {
            runtime,
            events,
            agent: agent.into(),
            poll: DEFAULT_POLL_INTERVAL,
            prompt: Arc::new(default_prompt),
            inbound: Arc::new(default_inbound),
            access_tiers: None,
            commands: None,
            child_sessions: None,
        }
    }

    #[cfg(feature = "push")]
    /// The push-notification state of this backend's agent, kept in the same store as its runs
    /// ([`StorePushStore`](crate::StorePushStore)): give it to
    /// `adam_a2a::push::PushSupport::new` so configs and delivery progress survive a restart and
    /// are shared between replicas.
    pub fn push_store(&self) -> crate::StorePushStore {
        crate::StorePushStore::new(self.runtime.store().clone(), self.agent.clone())
    }

    /// How often a subscription re-reads the durable run (default
    /// [`DEFAULT_POLL_INTERVAL`], 2 s). This bounds the latency of a state change
    /// that no live event announced, e.g. one made by another replica; a `Status`
    /// event (a commit) makes the pump poll at once, so the interval is the
    /// fallback, not the latency of a run this process steps.
    #[must_use]
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll = interval.max(Duration::from_millis(1));
        self
    }

    /// Where the question of an `input-required` task comes from (default:
    /// [`default_prompt`](crate::default_prompt)).
    ///
    /// Called only when the run changed, not on every poll: it must be a function of the view.
    #[must_use]
    pub fn with_prompt(
        mut self,
        f: impl Fn(&RunView) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.prompt = Arc::new(f);
        self
    }

    /// How an A2A message becomes the agent's input (default:
    /// [`default_inbound`](crate::default_inbound)).
    #[must_use]
    pub fn with_inbound(
        mut self,
        f: impl Fn(&Message) -> Result<adam_runtime::Inbound, String> + Send + Sync + 'static,
    ) -> Self {
        self.inbound = Arc::new(f);
        self
    }

    /// Serve `access-tier/v1` for an agent whose card declares `tiers`
    /// ([`ExtensionConfig::access_tier`](adam_a2a::ExtensionConfig::access_tier)): see
    /// "The access tier" in the type's documentation. Without it the extension is never read or
    /// echoed, whatever a request names.
    #[must_use]
    pub fn with_access_tiers(mut self, tiers: AccessTiers) -> Self {
        self.access_tiers = Some(tiers);
        self
    }

    /// Serve `commands/v1` for an agent whose card lists `commands`
    /// ([`ExtensionConfig::commands`](adam_a2a::ExtensionConfig::commands)): see "Commands" in the
    /// type's documentation. Without it a message is never read as a command, whatever it says.
    #[must_use]
    pub fn with_commands(mut self, commands: crate::Commands) -> Self {
        self.commands = Some(commands).filter(|c| !c.is_empty());
        self
    }

    /// Serve `child-sessions/v1` for an agent whose card lists `sessions`
    /// ([`ChildSessions::card_extension`](crate::ChildSessions::card_extension)): see the
    /// [`child_sessions`](crate::ChildSessions) module for the requests. A helper that is not
    /// registered on the runtime is a wiring mistake: it is logged and the extension is not
    /// served, so the card must not list it either.
    #[must_use]
    pub fn with_child_sessions(mut self, sessions: crate::ChildSessions) -> Self {
        match sessions.names_exist_in(&self.runtime.agent_names()) {
            Ok(()) => self.child_sessions = Some(sessions),
            Err(missing) => tracing::error!(
                agent = %missing,
                "a child-sessions helper is not registered on the runtime; the extension is not served"
            ),
        }
        self
    }

    /// The sessions this backend serves `child-sessions/v1` for: what
    /// [`with_child_sessions`](Self::with_child_sessions) kept, `None` when it was never given or
    /// refused because a helper is not registered. A composition root that lists the extension on
    /// the card checks this first, so the card never promises what the backend does not serve.
    pub fn child_sessions(&self) -> Option<&crate::ChildSessions> {
        self.child_sessions.as_ref()
    }

    /// The run behind `task_id` as the runtime has it, if it exists and is `caller`'s, a task of
    /// this agent or a round of one of the caller's child sessions (with the extension activated):
    /// the ownership check every call makes (`locate`), without building a task, so no file's
    /// bytes are read (a file artifact stays an `ArtifactRef` whose bytes
    /// [`Runtime::artifact_bytes`] fetches). For a host that serves the run's files itself.
    ///
    /// # Errors
    ///
    /// The runtime's failures, as every call's.
    pub async fn owned_view(
        &self,
        caller: &Caller,
        task_id: &str,
    ) -> Result<Option<RunView>, BackendError> {
        Ok(self.locate(caller, task_id).await?.map(|(_, view, _)| view))
    }

    /// The task this backend would serve for `view`, under the run's own conversation id as its
    /// context and without its artifacts, **with no ownership check**: for a composition root
    /// that decided itself that the run may be seen, as `adam-engine` shows its host a run of the
    /// agent that a schedule fired, which is no caller's. Never answer an A2A caller with it: what
    /// a caller may see is what [`owned_view`](Self::owned_view) and the `TaskBackend` calls say.
    pub fn unowned_task(&self, view: &RunView) -> Task {
        crate::convert::task_from_view(
            view,
            view.conversation_id.as_deref().unwrap_or_default(),
            &self.prompt,
            Vec::new(),
        )
    }

    /// The run behind `task_id` if it exists, belongs to this agent and to
    /// `caller`: `(run id, its durable view, context id)`.
    pub(crate) async fn owned(
        &self,
        caller: &Caller,
        task_id: &str,
    ) -> Result<Option<(RunId, RunView, String)>, BackendError> {
        let Ok(uuid) = Uuid::parse_str(task_id) else {
            return Ok(None);
        };
        let run = RunId(uuid);
        let Some(view) = self.runtime.view(run).await.map_err(map_err)? else {
            return Ok(None);
        };
        Ok(self
            .ownership(caller, view)
            .map(|(view, ctx)| (run, view, ctx)))
    }

    /// `Some((view, context id))` if the run is this agent's and the caller's.
    pub(crate) fn ownership(&self, caller: &Caller, view: RunView) -> Option<(RunView, String)> {
        let context = self.context_of(caller, &view.agent, view.conversation_id.as_deref())?;
        Some((view, context))
    }

    /// The context id of a run of `agent` in `conversation_id`, if it is this agent's and the
    /// caller's: the one rule of ownership, from what the record says, before any of its state is
    /// read.
    fn context_of(
        &self,
        caller: &Caller,
        agent: &str,
        conversation_id: Option<&str>,
    ) -> Option<String> {
        if agent != self.agent {
            return None;
        }
        let (subject, context) = decode_conversation(conversation_id?)?;
        (subject == caller.subject).then_some(context)
    }

    /// The view and context of a run a repeated submission found. `start_with_id` said the run
    /// exists; if the view says it is not this caller's, a purge between the two reads is a retry
    /// (the id is derived, and a retry can create it again), while a record that exists under
    /// another owner is a bug.
    async fn repeated(
        &self,
        caller: &Caller,
        run: RunId,
    ) -> Result<(RunView, String), BackendError> {
        let Some((_, view, context)) = self.owned(caller, &run.to_string()).await? else {
            let gone = self
                .runtime
                .store()
                .load_run(run)
                .await
                .map_err(|e| map_err(e.into()))?
                .is_none();
            return Err(if gone {
                BackendError::unavailable(
                    "the task was removed while a repeated request was settled",
                )
            } else {
                BackendError::internal("a repeated submission found a foreign task")
            });
        };
        Ok((view, context))
    }

    /// The run that a new task started by `message` in `context_id` continues: the first of its
    /// `referenceTaskIds` that is this caller's, this agent's, in this context and finished.
    ///
    /// Everything else is skipped without a word to the client (see the type's docs), and a
    /// reference is judged from the raw record (agent, conversation, status) before any state of
    /// it is decoded, so a record the caller does not own is never read, and cannot fail the
    /// request. The caller is the authenticated subject: the anonymous one is every client of an
    /// unauthenticated server at once, and continues nothing.
    ///
    /// Nothing is logged at `info` here: a request may pick more than once (the conversation was
    /// busy and is picked again) or end up delivered to an open task, where a reference means
    /// nothing. The [`Pick`] carries what was skipped, and [`start_or_join`](Self::start_or_join)
    /// says it once, for the outcome the request really had.
    async fn continued_run(
        &self,
        caller: &Caller,
        message: &Message,
        context_id: Option<&str>,
    ) -> Result<Pick, BackendError> {
        let references = message.reference_task_ids.as_deref().unwrap_or_default();
        if references.is_empty() {
            return Ok(Pick::default());
        }
        if caller.subject == Caller::ANONYMOUS {
            tracing::debug!(
                given = references.len(),
                "referenceTaskIds are not honoured for the anonymous caller, which is shared by all; the task starts fresh"
            );
            return Ok(Pick::default());
        }
        let mut skipped = Skipped {
            given: references.len(),
            over_limit: references.len().saturating_sub(MAX_REFERENCES),
            ..Skipped::default()
        };
        for reference in references.iter().take(MAX_REFERENCES) {
            match self.judge(caller, reference, context_id).await? {
                Verdict::Continue(run) => {
                    return Ok(Pick {
                        run: Some(run),
                        skipped: Some(skipped),
                    });
                }
                Verdict::Skip(why) => {
                    tracing::debug!(reference = ?shown(reference), ?why, "a referenced task is skipped");
                    skipped.count(why);
                }
            }
        }
        Ok(Pick {
            run: None,
            skipped: Some(skipped),
        })
    }

    /// Whether one reference can be continued, from the raw record first: its agent, its
    /// conversation (subject and context) and its status. Only a record that passes all of those,
    /// which is therefore the caller's own, has its state decoded, and one that does not decode is
    /// skipped with a warning instead of failing the request.
    async fn judge(
        &self,
        caller: &Caller,
        reference: &str,
        context_id: Option<&str>,
    ) -> Result<Verdict, BackendError> {
        let Ok(uuid) = Uuid::parse_str(reference) else {
            return Ok(Verdict::Skip(Why::Malformed));
        };
        let run = RunId(uuid);
        let Some(rec) = self
            .runtime
            .store()
            .load_run(run)
            .await
            .map_err(|e| map_err(e.into()))?
        else {
            return Ok(Verdict::Skip(Why::Unknown));
        };
        let Some(context) = self.context_of(caller, &rec.agent, rec.conversation_id.as_deref())
        else {
            return Ok(Verdict::Skip(Why::NotTheCallers));
        };
        if context_id != Some(context.as_str()) {
            return Ok(Verdict::Skip(Why::OtherContext));
        }
        if !rec.status.is_terminal() {
            return Ok(Verdict::Skip(Why::Open));
        }
        match self.runtime.view(run).await {
            Ok(Some(_)) => Ok(Verdict::Continue(run)),
            Ok(None) => Ok(Verdict::Skip(Why::Unknown)),
            Err(RuntimeError::Corrupt { .. }) => {
                tracing::warn!(%run, "a referenced task of the caller has an unreadable state; it is not continued");
                Ok(Verdict::Skip(Why::Unreadable))
            }
            Err(e) => Err(map_err(e)),
        }
    }

    /// A new-task submission: the run, its context id, and the view a repeated request already
    /// read while settling it (a run that existed and was not written to).
    ///
    /// The run id is [`task_id_for`], so a repeat of the same request (same
    /// caller, `contextId` and `messageId`) finds the run its first attempt
    /// created and starts nothing. If the conversation already has an open task
    /// the message is delivered to it, as `Runtime::start` does. Otherwise the
    /// task starts from nothing, or continuing the run that
    /// [`continued_run`](Self::continued_run) picks.
    async fn start_or_join(
        &self,
        caller: &Caller,
        message: &Message,
        inbound: adam_runtime::Inbound,
        context_id: Option<String>,
    ) -> Result<Joined, BackendError> {
        let Pick {
            run: mut prior,
            skipped: mut tally,
        } = self
            .continued_run(caller, message, context_id.as_deref())
            .await?;
        // Without a message id there is nothing to recognise a repeat by.
        if message.message_id.is_empty() {
            let context = context_id.unwrap_or_else(a2a::new_context_id);
            let conversation = encode_conversation(&caller.subject, &context);
            let started = match prior {
                Some(prior) => {
                    self.runtime
                        .start_continuing(&self.agent, inbound.clone(), Some(&conversation), prior)
                        .await
                }
                None => {
                    self.runtime
                        .start(&self.agent, inbound.clone(), Some(&conversation))
                        .await
                }
            };
            let run = match started {
                // The run it continued was purged in the meantime: there is nothing to continue.
                Err(e) if prior_is_gone(&e, prior) => {
                    prior = None;
                    if let Some(tally) = tally.as_mut() {
                        tally.count(why_gone(&e));
                    }
                    self.runtime
                        .start(&self.agent, inbound, Some(&conversation))
                        .await
                }
                other => other,
            }
            .map_err(map_err)?;
            say_fresh(prior, tally.as_ref());
            return Ok(Joined {
                run,
                context,
                view: None,
            });
        }
        let run = task_id_for(
            &self.agent,
            &caller.subject,
            context_id.as_deref(),
            &message.message_id,
        );
        let context = context_id.clone().unwrap_or_else(a2a::new_context_id);
        let conversation = encode_conversation(&caller.subject, &context);
        for _ in 0..MAX_JOIN_ATTEMPTS {
            let started = match prior {
                Some(prior) => {
                    self.runtime
                        .start_with_id_continuing(
                            run,
                            &self.agent,
                            inbound.clone(),
                            Some(&conversation),
                            prior,
                        )
                        .await
                }
                None => {
                    self.runtime
                        .start_with_id(run, &self.agent, inbound.clone(), Some(&conversation))
                        .await
                }
            };
            match started {
                Ok(true) => {
                    say_fresh(prior, tally.as_ref());
                    return Ok(Joined {
                        run,
                        context,
                        view: None,
                    });
                }
                Ok(false) => {
                    // A repeat: the task exists, and its context is the one it
                    // was created with (a request without `contextId` got a
                    // generated one the first time). The view that proves it is
                    // the task's: no write followed the failed start.
                    let (view, context) = self.repeated(caller, run).await?;
                    return Ok(Joined {
                        run,
                        context,
                        view: Some(view),
                    });
                }
                // The run it continued was purged or became unreadable in the meantime: there is
                // nothing to continue.
                Err(e) if prior_is_gone(&e, prior) => {
                    prior = None;
                    if let Some(tally) = tally.as_mut() {
                        tally.count(why_gone(&e));
                    }
                }
                Err(RuntimeError::ConversationBusy { .. }) => {
                    let open = self
                        .runtime
                        .store()
                        .open_run_for_conversation(&self.agent, &conversation)
                        .await
                        .map_err(|e| map_err(e.into()))?;
                    if let Some(open) = open {
                        match self.runtime.deliver(open.id, inbound.clone()).await {
                            // The delivery wrote: the view is read below, not here.
                            Ok(()) => {
                                return Ok(Joined {
                                    run: open.id,
                                    context,
                                    view: None,
                                });
                            }
                            // It finished meanwhile: the context takes a new task.
                            Err(RuntimeError::Finished { .. } | RuntimeError::NotFound(_)) => {}
                            Err(e) => return Err(map_err(e)),
                        }
                    }
                    // The open task was gone or finished by the time it was looked at, and the
                    // task that just finished may be the one this message references (it was open,
                    // so it was skipped): pick again instead of starting from nothing.
                    let again = self
                        .continued_run(caller, message, context_id.as_deref())
                        .await?;
                    (prior, tally) = (again.run, again.skipped);
                }
                Err(e) => return Err(map_err(e)),
            }
        }
        Err(BackendError::unavailable("the conversation is contended"))
    }

    /// Deliver `inbound` to the open task `run` (`steer/v1`) and answer with the task as it is
    /// then. A task that finished between the read and the delivery is the terminal task's error;
    /// one that vanished is not found.
    async fn steer(
        &self,
        caller: &Caller,
        run: RunId,
        context: &str,
        inbound: adam_runtime::Inbound,
        task_id: &str,
    ) -> Result<Task, BackendError> {
        match self.runtime.deliver(run, inbound).await {
            Ok(()) => {}
            Err(RuntimeError::Finished { status, .. }) => {
                return Err(BackendError::UnsupportedOperation(format!(
                    "task {task_id} is {status} and cannot take a message"
                )));
            }
            Err(RuntimeError::NotFound(_)) => {
                return Err(BackendError::TaskNotFound(task_id.to_owned()));
            }
            Err(e) => return Err(map_err(e)),
        }
        // The delivery is a commit, so a run nobody has stepped yet reads `working` from here on.
        let view = self.view_of(run).await?;
        let task = self.task_of(&view, context).await;
        Ok(self.said(caller, &view, task))
    }

    /// A resume (`adam.resume: true`, ADR 0067) to the task `task_id`, already located: it wakes a
    /// run parked on the model and adds nothing to its conversation. Any other task, a finished or
    /// canceled one included, is refused with [`NOT_PARKED`](crate::NOT_PARKED), and nothing is
    /// written.
    pub(crate) async fn resume(
        &self,
        caller: &Caller,
        (run, view, context): (RunId, RunView, String),
        message: &Message,
        context_id: Option<&str>,
    ) -> Result<Task, BackendError> {
        if context_id.is_some_and(|c| c != context) {
            return Err(BackendError::InvalidParams(
                "contextId does not match the task".to_owned(),
            ));
        }
        if !crate::model_park::parked_on_model(&view) {
            return Err(BackendError::InvalidParams(
                crate::model_park::NOT_PARKED.to_owned(),
            ));
        }
        match self
            .runtime
            .deliver(run, crate::model_park::resume_inbound(message))
            .await
        {
            Ok(()) => {}
            // It ended between the read and the delivery: it is not parked on the model any more.
            Err(RuntimeError::Finished { .. }) => {
                return Err(BackendError::InvalidParams(
                    crate::model_park::NOT_PARKED.to_owned(),
                ));
            }
            Err(e) => return Err(map_err(e)),
        }
        self.woken(caller, run, &context).await
    }

    /// A message to a root context or to a task of the root agent.
    async fn submit_root(
        &self,
        caller: &Caller,
        message: Message,
        task_id: Option<String>,
        context_id: Option<String>,
    ) -> Result<Task, BackendError> {
        // A resume is read before the inbound function, which refuses a message with no text: it
        // adds nothing, so neither a command nor a tier is read from it.
        if crate::model_park::is_resume(&message) {
            let Some(task_id) = task_id else {
                return Err(BackendError::InvalidParams(
                    crate::model_park::RESUME_NAMES_ITS_TASK.to_owned(),
                ));
            };
            let located = self
                .owned(caller, &task_id)
                .await?
                .ok_or_else(|| BackendError::TaskNotFound(task_id.clone()))?;
            return self
                .resume(caller, located, &message, context_id.as_deref())
                .await;
        }
        let mut inbound = (self.inbound)(&message).map_err(BackendError::InvalidParams)?;
        // A command is expanded here, once, and the expansion is what is stored: a replay never
        // reads the folder again. A name the agent does not serve in the metadata is refused
        // before anything is written.
        if let Some(commands) = &self.commands {
            crate::commands::apply(
                &message,
                &mut inbound,
                commands,
                caller.has_extension(COMMANDS_EXTENSION),
            )
            .await
            .map_err(|refusal| match refusal {
                crate::CommandRefusal::Invalid(why) => BackendError::InvalidParams(why),
                crate::CommandRefusal::Unavailable(why) => BackendError::unavailable(why),
            })?;
        }
        // A message that activated the extension names a tier the agent honours, or nothing is
        // written. One that did not is read as it always was.
        if let Some(tiers) = &self.access_tiers
            && caller.has_extension(ACCESS_TIER_EXTENSION)
        {
            let named = tier::requested(&message, tiers).map_err(BackendError::InvalidParams)?;
            tier::put(&mut inbound, named).map_err(BackendError::InvalidParams)?;
        }

        let Some(task_id) = task_id else {
            let Joined { run, context, view } = self
                .start_or_join(caller, &message, inbound, context_id)
                .await?;
            // A repeated request already read the run it found; only a start (or a delivery) is
            // read again, because it wrote.
            let view = match view {
                Some(view) => view,
                None => self.view_of(run).await?,
            };
            let mut task = self.said(caller, &view, self.task_of(&view, &context).await);
            let mut first = message;
            first.task_id = Some(task.id.clone());
            first.context_id = Some(context);
            task.history = Some(vec![first]);
            return Ok(task);
        };

        let located = self
            .owned(caller, &task_id)
            .await?
            .ok_or_else(|| BackendError::TaskNotFound(task_id.clone()))?;
        self.follow_up(caller, located, inbound, &task_id, context_id)
            .await
    }

    /// A message that names `task_id`, a task of the caller already located: a steer, the answer
    /// to a question, or the refusal its state calls for (see the type's documentation).
    pub(crate) async fn follow_up(
        &self,
        caller: &Caller,
        (run, view, context): (RunId, RunView, String),
        inbound: adam_runtime::Inbound,
        task_id: &str,
        context_id: Option<String>,
    ) -> Result<Task, BackendError> {
        let state = crate::task_state(&view);
        let steering = caller.has_extension(STEER_EXTENSION)
            && matches!(state, TaskState::Submitted | TaskState::Working);
        if context_id.is_some_and(|c| c != context) {
            return Err(if steering {
                BackendError::TaskNotFound(task_id.to_owned())
            } else {
                BackendError::InvalidParams("contextId does not match the task".to_owned())
            });
        }
        if state.is_terminal() {
            return Err(BackendError::UnsupportedOperation(format!(
                "task {task_id} is {state:?} and cannot take a message"
            )));
        }
        if steering {
            return self.steer(caller, run, &context, inbound, task_id).await;
        }
        // A run parked on the model takes a message, timer or not (ADR 0067): its text is the
        // person's turn, and the call is made again.
        if !view.waiting && !crate::model_park::parked_on_model(&view) {
            return Err(BackendError::InvalidParams(format!(
                "task {task_id} is {state:?} and cannot take a follow-up"
            )));
        }
        self.runtime.deliver(run, inbound).await.map_err(map_err)?;
        self.woken(caller, run, &context).await
    }

    /// The task a delivery just woke: the delivery made the run runnable, so this reads `working`;
    /// make that certain even if the record is read before the commit is visible.
    async fn woken(
        &self,
        caller: &Caller,
        run: RunId,
        context: &str,
    ) -> Result<Task, BackendError> {
        let view = self.view_of(run).await?;
        let mut task = self.task_of(&view, context).await;
        if matches!(
            task.status.state,
            TaskState::InputRequired | TaskState::AuthRequired
        ) {
            task.status.state = TaskState::Working;
            task.status.message = None;
            // A working task carries no totals: they are said when it ends or waits again.
            task.metadata = None;
        }
        Ok(self.said(caller, &view, task))
    }

    async fn view_of(&self, run: RunId) -> Result<RunView, BackendError> {
        self.runtime
            .view(run)
            .await
            .map_err(map_err)?
            .ok_or_else(|| BackendError::TaskNotFound(run.to_string()))
    }

    /// The A2A task of `view`: the run's artifacts resolved for the wire, then the mapping.
    ///
    /// A file artifact is fetched by its blob, once: the runtime's bounded cache answers every
    /// later read of the same file, so a poll of an unchanged run transfers no bytes.
    pub(crate) async fn task_of(&self, view: &RunView, context_id: &str) -> Task {
        let artifacts = self.artifacts_of(view).await;
        task_from_view(view, context_id, &self.prompt, artifacts)
    }

    /// Every artifact of `view` as A2A serves it. A file whose blob is gone is left out (the run
    /// is intact; that one artifact cannot be served).
    pub(crate) async fn artifacts_of(&self, view: &RunView) -> Vec<a2a::Artifact> {
        let mut artifacts = Vec::with_capacity(view.artifacts.len());
        for artifact in &view.artifacts {
            if let Some(artifact) = self.artifact_of(artifact).await {
                artifacts.push(artifact);
            }
        }
        artifacts
    }

    /// One artifact as A2A serves it, its blob fetched when it has one.
    pub(crate) async fn artifact_of(&self, artifact: &ArtifactRef) -> Option<a2a::Artifact> {
        let id = crate::convert::artifact_id(artifact);
        self.artifact_of_with(artifact, id).await
    }

    /// [`artifact_of`](Self::artifact_of) for a caller that already derived the id (a subscriber
    /// caches it): the id is not derived again, so a poll hashes an artifact once.
    ///
    /// Only a [`FileSource::Blob`] is fetched. An inline file (a v4 run) is served from the view's
    /// own bytes, so the backend neither copies them into an `Arc` nor copies them again.
    pub(crate) async fn artifact_of_with(
        &self,
        artifact: &ArtifactRef,
        id: String,
    ) -> Option<a2a::Artifact> {
        let bytes = match &artifact.file {
            Some(file) if matches!(file.source, FileSource::Blob(_)) => {
                #[cfg(test)]
                fetch_hook::fetched();
                match self.runtime.artifact_bytes(file).await {
                    Ok(Some(bytes)) => Some(bytes),
                    Ok(None) => {
                        tracing::warn!(
                            artifact = %artifact.name,
                            blob = %file.digest(),
                            "the file's blob is gone; the artifact is left out of the task"
                        );
                        None
                    }
                    Err(error) => {
                        tracing::warn!(
                            artifact = %artifact.name,
                            error = %adam_error::report(&error),
                            "a file artifact's bytes could not be read; it is left out of the task"
                        );
                        None
                    }
                }
            }
            // An inline file (a v4 run): the ref carries its bytes, and `convert` serves them.
            Some(_) => None,
            None => None,
        };
        crate::convert::artifact_of_with(artifact, id, bytes)
    }
}

/// Counts the blobs the backend fetches, for the test that proves an inline file is not fetched.
#[cfg(test)]
mod fetch_hook {
    use std::cell::Cell;

    thread_local! {
        static FETCHES: Cell<usize> = const { Cell::new(0) };
    }

    pub(super) fn fetched() {
        FETCHES.with(|n| n.set(n.get() + 1));
    }

    pub(super) fn take() -> usize {
        FETCHES.with(|n| n.replace(0))
    }
}

/// What starting or joining a task settled.
struct Joined {
    run: RunId,
    context: String,
    /// The view of a run that already existed, read while settling a repeat. No write followed it,
    /// so the task can be built from it instead of reading the run again.
    view: Option<RunView>,
}

/// What picking the run to continue found: the run, if a reference qualified, and what was
/// skipped on the way (`None` when the message named no reference, or the caller is anonymous,
/// which honours none).
#[derive(Default)]
struct Pick {
    run: Option<RunId>,
    skipped: Option<Skipped>,
}

/// What [`RuntimeTaskBackend::judge`] made of a reference.
enum Verdict {
    Continue(RunId),
    Skip(Why),
}

/// Why a reference was skipped. Only the log says.
#[derive(Clone, Copy, Debug)]
enum Why {
    /// Not a task id at all.
    Malformed,
    /// No such run (or it was purged meanwhile).
    Unknown,
    /// Another agent's, or another caller's: the client cannot be told which, and neither is the log.
    NotTheCallers,
    /// The caller's, in another context than the new task.
    OtherContext,
    /// Still open.
    Open,
    /// The caller's own, finished, in this context, and unreadable.
    Unreadable,
}

/// How many references were skipped for each reason, for the one line that says nothing could be
/// continued.
#[derive(Default)]
struct Skipped {
    given: usize,
    malformed: usize,
    unknown: usize,
    not_the_callers: usize,
    other_context: usize,
    open: usize,
    unreadable: usize,
    over_limit: usize,
}

impl Skipped {
    fn count(&mut self, why: Why) {
        *match why {
            Why::Malformed => &mut self.malformed,
            Why::Unknown => &mut self.unknown,
            Why::NotTheCallers => &mut self.not_the_callers,
            Why::OtherContext => &mut self.other_context,
            Why::Open => &mut self.open,
            Why::Unreadable => &mut self.unreadable,
        } += 1;
    }
}

/// The one line for a request that named references and **started a fresh task** anyway: what it
/// asked, and why none could be continued. Said once per request, by
/// [`start_or_join`](RuntimeTaskBackend::start_or_join), after the start that settled the outcome:
/// not when a run was continued, not when the message was delivered to the open task of its
/// context (where a reference means nothing), and not again when a busy conversation made the
/// request pick a second time. The client is told nothing; the operator sees that it asked.
fn say_fresh(continued: Option<RunId>, tally: Option<&Skipped>) {
    let (None, Some(skipped)) = (continued, tally) else {
        return;
    };
    tracing::info!(
        given = skipped.given,
        malformed = skipped.malformed,
        unknown = skipped.unknown,
        not_the_callers = skipped.not_the_callers,
        other_context = skipped.other_context,
        open = skipped.open,
        unreadable = skipped.unreadable,
        over_limit = skipped.over_limit,
        "none of the referenceTaskIds could be continued; the task starts fresh"
    );
}

/// Why the run a start was to continue turned out not to be continuable after all (see
/// [`prior_is_gone`]): purged, or unreadable.
fn why_gone(error: &RuntimeError) -> Why {
    match error {
        RuntimeError::Corrupt { .. } => Why::Unreadable,
        _ => Why::Unknown,
    }
}

/// A client-supplied id for a log line: cut short, so that it cannot flood one. It is formatted
/// with `?`, which escapes what is not printable.
fn shown(reference: &str) -> String {
    const MAX: usize = 48;
    match reference.char_indices().nth(MAX) {
        Some((at, _)) => format!("{}...", &reference[..at]),
        None => reference.to_owned(),
    }
}

/// Whether `error` says the run `prior` (named to continue) cannot be continued from any more
/// (purged, or unreadable) rather than that the request failed.
fn prior_is_gone(error: &RuntimeError, prior: Option<RunId>) -> bool {
    match (error, prior) {
        (RuntimeError::NotFound(gone), Some(prior)) => *gone == prior,
        (RuntimeError::Corrupt { run, .. }, Some(prior)) => *run == prior,
        _ => false,
    }
}

/// Map a runtime failure to the backend seam's error by its [`ErrorClass`], keeping the runtime
/// error as the source. The A2A server logs the whole chain and sends the client only what this
/// function chose to say.
///
/// | class | backend error |
/// |---|---|
/// | `NotFound` | `TaskNotFound` |
/// | `Invalid`, `Rejected` | `InvalidParams` (the agent rejected the request, or the task's state forbids it) |
/// | `Transient`, `RateLimited`, `Conflict` | `Unavailable` |
/// | anything else | `Internal` |
pub(crate) fn map_err(e: RuntimeError) -> BackendError {
    match e.class() {
        ErrorClass::NotFound => {
            let task = match &e {
                RuntimeError::NotFound(run) | RuntimeError::Store(StoreError::NotFound(run)) => {
                    run.to_string()
                }
                _ => "unknown".to_owned(),
            };
            BackendError::TaskNotFound(task)
        }
        ErrorClass::Invalid | ErrorClass::Rejected => {
            BackendError::InvalidParams(client_detail(&e))
        }
        ErrorClass::Transient | ErrorClass::RateLimited | ErrorClass::Conflict => {
            BackendError::unavailable("the task runtime is unavailable").with_source(e)
        }
        _ => BackendError::internal("the task runtime failed").with_source(e),
    }
}

/// What a client may be told about a request the runtime refused: the agent's own message about
/// the request, the state that forbids it, or a fixed sentence. Never a transport or driver text.
fn client_detail(e: &RuntimeError) -> String {
    match e {
        RuntimeError::Finished { run, status } => format!("task {run} is already {status}"),
        RuntimeError::ConversationBusy { .. } => "the conversation already has an open task".into(),
        RuntimeError::Agent(AgentError::Permanent { message, .. }) => message.clone(),
        RuntimeError::Store(StoreError::InvalidInput(message)) => message.clone(),
        _ => "the request was rejected".into(),
    }
}

#[async_trait]
impl TaskBackend for RuntimeTaskBackend {
    #[tracing::instrument(skip_all, fields(subject = %caller.subject))]
    async fn submit(
        &self,
        caller: Caller,
        message: Message,
        task_id: Option<String>,
        context_id: Option<String>,
    ) -> Result<Task, BackendError> {
        if self.child_sessions.is_some()
            && let Some(task) = self
                .submit_child(&caller, &message, task_id.as_deref(), context_id.as_deref())
                .await?
        {
            return Ok(task);
        }
        let task = self
            .submit_root(&caller, message, task_id, context_id)
            .await?;
        self.with_sessions(&caller, task).await
    }

    async fn get(&self, caller: &Caller, task_id: &str) -> Result<Option<Task>, BackendError> {
        let Some((_, view, context)) = self.locate(caller, task_id).await? else {
            return Ok(None);
        };
        let task = self.said(caller, &view, self.task_of(&view, &context).await);
        Ok(Some(self.with_sessions(caller, task).await?))
    }

    #[tracing::instrument(skip(self, caller), fields(subject = %caller.subject))]
    async fn cancel(&self, caller: &Caller, task_id: &str) -> Result<Task, BackendError> {
        let (run, view, context) = self
            .locate(caller, task_id)
            .await?
            .ok_or_else(|| BackendError::TaskNotFound(task_id.to_owned()))?;
        let mut state = crate::task_state(&view);
        let mut view = view;
        if !state.is_terminal() {
            self.runtime
                .cancel(run, CANCEL_REASON)
                .await
                .map_err(map_err)?;
            view = self.view_of(run).await?;
            state = crate::task_state(&view);
            if let Some(round) = self.round_of(caller, task_id).await? {
                self.clean_up_after_cancel(caller, &round).await?;
            }
        }
        match state {
            TaskState::Canceled => {
                let task = self.task_of(&view, &context).await;
                self.with_sessions(caller, task).await
            }
            other => Err(BackendError::NotCancelable {
                task_id: task_id.to_owned(),
                state: format!("{other:?}"),
            }),
        }
    }

    fn subscribe(
        &self,
        caller: &Caller,
        task_id: &str,
    ) -> BoxStream<'static, Result<TaskEvent, BackendError>> {
        subscribe::subscribe(self.clone(), caller.clone(), task_id.to_owned())
    }

    async fn list(&self, caller: &Caller, query: &TaskQuery) -> Result<TaskPage, BackendError> {
        self.list_tasks(caller, query).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn lower() -> std::io::Error {
        std::io::Error::other("connection reset by peer")
    }

    fn run() -> RunId {
        RunId(Uuid::nil())
    }

    /// The A2A error a client is told, by kind.
    #[derive(Debug, Clone, Copy)]
    enum Told {
        TaskNotFound,
        InvalidParams,
        Unavailable,
        Internal,
    }

    /// One row per class the runtime can report: the error and what the client is told. The
    /// class itself is only printed on failure; the runtime's own tests pin it.
    #[test]
    fn runtime_errors_map_by_class() {
        use adam_core::store::StoreError as S;
        let cases: Vec<(RuntimeError, Told)> = vec![
            (RuntimeError::NotFound(run()), Told::TaskNotFound),
            (RuntimeError::Store(S::NotFound(run())), Told::TaskNotFound),
            (
                RuntimeError::Finished {
                    run: run(),
                    status: adam_core::RunStatus::Done,
                },
                Told::InvalidParams,
            ),
            (
                RuntimeError::ConversationBusy {
                    agent: "a".into(),
                    conversation_id: "secret-subject:ctx".into(),
                },
                Told::InvalidParams,
            ),
            (
                RuntimeError::Agent(AgentError::permanent("unusable start message: empty")),
                Told::InvalidParams,
            ),
            (
                RuntimeError::Store(S::InvalidInput("no NUL please".into())),
                Told::InvalidParams,
            ),
            (RuntimeError::UnknownAgent("x".into()), Told::InvalidParams),
            (RuntimeError::Contended("run x".into()), Told::Unavailable),
            (
                RuntimeError::Store(S::unavailable(lower())),
                Told::Unavailable,
            ),
            (
                RuntimeError::Agent(AgentError::transient_after(
                    "slow down",
                    Duration::from_secs(3),
                )),
                Told::Unavailable,
            ),
            (
                RuntimeError::Corrupt {
                    run: run(),
                    reason: "x".into(),
                    source: None,
                },
                Told::Internal,
            ),
            (RuntimeError::Store(S::internal(lower())), Told::Internal),
        ];
        for (e, told) in cases {
            let shown = e.to_string();
            let class = e.class();
            let mapped = map_err(e);
            let ok = matches!(
                (told, &mapped),
                (Told::TaskNotFound, BackendError::TaskNotFound(_))
                    | (Told::InvalidParams, BackendError::InvalidParams(_))
                    | (Told::Unavailable, BackendError::Unavailable { .. })
                    | (Told::Internal, BackendError::Internal { .. })
            );
            assert!(
                ok,
                "{shown} ({class:?}) mapped to {mapped:?}, expected {told:?}"
            );
        }
    }

    #[test]
    fn what_a_client_is_told_carries_no_cause_and_no_conversation_id() {
        let busy = map_err(RuntimeError::ConversationBusy {
            agent: "a".into(),
            conversation_id: "secret-subject:ctx".into(),
        });
        let told = a2a::A2AError::from(busy);
        assert!(!told.message.contains("secret-subject"), "{}", told.message);

        let down = map_err(RuntimeError::Store(StoreError::unavailable(lower())));
        assert!(std::error::Error::source(&down).is_some());
        let told = a2a::A2AError::from(down);
        assert_eq!(told.code, -32603);
        assert_eq!(told.message, "backend temporarily unavailable");
        assert!(!told.message.contains("connection reset"));
    }

    /// A repeated submission whose run vanished between the start and the view is a retry, not a
    /// server fault; a record of another agent under the same id is a bug.
    #[tokio::test]
    async fn a_repeated_run_that_vanished_is_a_retry_and_a_foreign_one_is_a_bug() {
        use adam_core::{DynStore, MemoryStore};
        use adam_runtime::{AgentStarter, Inbound};
        use std::sync::Arc;

        struct Other;
        impl AgentStarter for Other {
            type State = serde_json::Value;
            fn name(&self) -> &str {
                "someone-else"
            }
            fn init(&self, _input: Inbound) -> Result<Self::State, AgentError> {
                Ok(serde_json::json!({}))
            }
        }

        let store: DynStore = Arc::new(MemoryStore::new());
        let runtime = Runtime::builder(store.clone()).starter(Other).build();
        let backend =
            RuntimeTaskBackend::new(runtime.clone(), BroadcastSink::default(), "scripted");
        let caller = Caller::new("token-0");

        let ghost = backend.repeated(&caller, RunId::new()).await.unwrap_err();
        assert!(
            matches!(ghost, BackendError::Unavailable { .. }),
            "{ghost:?}"
        );

        let id = RunId::new();
        runtime
            .start_with_id(
                id,
                "someone-else",
                Inbound::new("m-1", serde_json::json!({"text": "go"})),
                Some("token-1:ctx"),
            )
            .await
            .unwrap();
        let foreign = backend.repeated(&caller, id).await.unwrap_err();
        assert!(
            matches!(foreign, BackendError::Internal { .. }),
            "{foreign:?}"
        );
    }

    /// An inline file is served from the view's bytes with **no** `artifact_bytes` call; a blob is
    /// fetched (and the runtime's cache answers a second read).
    #[tokio::test]
    async fn an_inline_file_is_not_fetched_and_a_blob_is() {
        use std::sync::Arc;

        use adam_core::{DynStore, MemoryStore};
        use adam_runtime::FileRef;

        let store: DynStore = Arc::new(MemoryStore::new());
        let runtime = Runtime::builder(store.clone()).build();
        let backend = RuntimeTaskBackend::new(runtime, BroadcastSink::default(), "a");

        let bytes = vec![7_u8; 4 * 1024 * 1024];
        let inline = ArtifactRef::file("big", "application/octet-stream", "big.bin", bytes.clone())
            .expect("a file");
        super::fetch_hook::take();
        let served = backend.artifact_of(&inline).await.expect("served");
        assert_eq!(
            super::fetch_hook::take(),
            0,
            "an inline file is not fetched"
        );
        assert_eq!(
            served.parts[0].content,
            a2a::PartContent::Raw(bytes.clone())
        );

        let digest = adam_runtime::blob_id(&bytes);
        store.put_blob(&digest, &bytes).await.expect("put the blob");
        let mut blob = ArtifactRef::new(
            "big",
            Some("application/octet-stream".into()),
            serde_json::Value::Null,
        );
        blob.file = Some(FileRef::blob("big.bin", bytes.len() as u64, digest));
        let served = backend.artifact_of(&blob).await.expect("served");
        assert_eq!(super::fetch_hook::take(), 1, "a blob is fetched once");
        assert_eq!(served.parts[0].content, a2a::PartContent::Raw(bytes));
    }

    /// A blob that is gone serves nothing: the artifact is left out, and the run is intact.
    #[tokio::test]
    async fn a_missing_blob_leaves_the_artifact_out() {
        use std::sync::Arc;

        use adam_core::{DynStore, MemoryStore};
        use adam_runtime::FileRef;

        let store: DynStore = Arc::new(MemoryStore::new());
        let runtime = Runtime::builder(store).build();
        let backend = RuntimeTaskBackend::new(runtime, BroadcastSink::default(), "a");
        let digest = adam_runtime::blob_id(b"gone");
        let mut blob = ArtifactRef::new("gone", None, serde_json::Value::Null);
        blob.file = Some(FileRef::blob("gone.bin", 4, digest));
        assert!(
            backend.artifact_of(&blob).await.is_none(),
            "a missing blob is not served as an empty file"
        );
    }
}
