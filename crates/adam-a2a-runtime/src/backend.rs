//! [`RuntimeTaskBackend`]: the A2A [`TaskBackend`] over an `adam_runtime::Runtime`.

use std::sync::Arc;
use std::time::Duration;

use a2a::{Message, Task, TaskState};
use adam_a2a::{BackendError, Caller, STEER_EXTENSION, TaskBackend, TaskEvent};
use adam_core::{RunId, StoreError};
use adam_error::ErrorClass;
use adam_runtime::{AgentError, BroadcastSink, Classify, RunView, Runtime, RuntimeError};
use async_trait::async_trait;
use futures::stream::BoxStream;
use uuid::Uuid;

use crate::convert::{
    InboundFn, PromptFn, decode_conversation, default_inbound, default_prompt, encode_conversation,
    task_from_view,
};
use crate::ids::task_id_for;
use crate::subscribe;

/// Default interval at which a subscription re-reads the durable run.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(250);

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
/// | `SendMessage` with `taskId` | `Runtime::deliver` while `input-required`; and, with `steer/v1` activated, while `submitted` or `working` (see "A message for a running task") |
/// | `submitted` | runnable, nothing committed, no worker holds it |
/// | `working` | runnable and a worker holds the run's lease (`RunView::claimed`, from the first claim, before the first commit), or committed at least once, or parked on a timer |
/// | `input-required` | run parked with no timer (`RunView::waiting`); the question comes from [`PromptFn`] |
/// | `completed` | `Done`; `output.text` is the status message, `RunView::artifacts` are the task artifacts |
/// | `failed` | `Failed`; the error is the status message |
/// | `canceled` | `Failed` with `cancelled: ...` (what `Runtime::cancel` writes) |
/// | `CancelTask` | `Runtime::cancel` |
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
#[derive(Clone)]
pub struct RuntimeTaskBackend {
    pub(crate) runtime: Runtime,
    pub(crate) events: BroadcastSink,
    pub(crate) agent: String,
    pub(crate) poll: Duration,
    pub(crate) prompt: PromptFn,
    pub(crate) inbound: InboundFn,
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
        }
    }

    /// How often a subscription re-reads the durable run (default
    /// [`DEFAULT_POLL_INTERVAL`]). This bounds the latency of a state change
    /// that no live event announced, e.g. one made by another replica.
    #[must_use]
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll = interval.max(Duration::from_millis(1));
        self
    }

    /// Where the question of an `input-required` task comes from (default:
    /// [`default_prompt`](crate::default_prompt)).
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

    /// A new-task submission: `(run id, context id)` of the task that took the
    /// message.
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
    ) -> Result<(RunId, String), BackendError> {
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
            return Ok((run, context));
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
                    return Ok((run, context));
                }
                Ok(false) => {
                    // A repeat: the task exists, and its context is the one it
                    // was created with (a request without `contextId` got a
                    // generated one the first time).
                    let (_, _, context) =
                        self.owned(caller, &run.to_string()).await?.ok_or_else(|| {
                            BackendError::internal("a repeated submission found a foreign task")
                        })?;
                    return Ok((run, context));
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
                            Ok(()) => return Ok((open.id, context)),
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
        Ok(task_from_view(&view, context, &self.prompt))
    }

    async fn view_of(&self, run: RunId) -> Result<RunView, BackendError> {
        self.runtime
            .view(run)
            .await
            .map_err(map_err)?
            .ok_or_else(|| BackendError::TaskNotFound(run.to_string()))
    }
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
        let inbound = (self.inbound)(&message).map_err(BackendError::InvalidParams)?;

        let Some(task_id) = task_id else {
            let (run, context) = self
                .start_or_join(&caller, &message, inbound, context_id)
                .await?;
            let view = self.view_of(run).await?;
            let mut task = task_from_view(&view, &context, &self.prompt);
            let mut first = message;
            first.task_id = Some(task.id.clone());
            first.context_id = Some(context);
            task.history = Some(vec![first]);
            return Ok(task);
        };

        let (run, view, context) = self
            .owned(&caller, &task_id)
            .await?
            .ok_or_else(|| BackendError::TaskNotFound(task_id.clone()))?;
        let state = crate::task_state(&view);
        let steering = caller.has_extension(STEER_EXTENSION)
            && matches!(state, TaskState::Submitted | TaskState::Working);
        if context_id.is_some_and(|c| c != context) {
            return Err(if steering {
                BackendError::TaskNotFound(task_id)
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
            return self.steer(run, &context, inbound, &task_id).await;
        }
        if !view.waiting {
            return Err(BackendError::InvalidParams(format!(
                "task {task_id} is {state:?} and cannot take a follow-up"
            )));
        }
        self.runtime.deliver(run, inbound).await.map_err(map_err)?;

        // The delivery made the run runnable, so this reads `working`; make
        // that certain even if the record is read before the commit is visible.
        let view = self.view_of(run).await?;
        let mut task = task_from_view(&view, &context, &self.prompt);
        if task.status.state == TaskState::InputRequired {
            task.status.state = TaskState::Working;
            task.status.message = None;
        }
        Ok(task)
    }

    async fn get(&self, caller: &Caller, task_id: &str) -> Result<Option<Task>, BackendError> {
        Ok(self
            .owned(caller, task_id)
            .await?
            .map(|(_, view, context)| task_from_view(&view, &context, &self.prompt)))
    }

    #[tracing::instrument(skip(self, caller), fields(subject = %caller.subject))]
    async fn cancel(&self, caller: &Caller, task_id: &str) -> Result<Task, BackendError> {
        let (run, view, context) = self
            .owned(caller, task_id)
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
        }
        match state {
            TaskState::Canceled => Ok(task_from_view(&view, &context, &self.prompt)),
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
}
