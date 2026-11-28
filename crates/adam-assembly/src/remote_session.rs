//! [`RemoteRoundAgent`]: the agent whose runs are the rounds of a session on a remote (A2A)
//! subagent (ADR 0045, slice 5).
//!
//! A round is a run like any other round of a session, so the session tools, the listing, the
//! notices, the closing round and the turn-end wait work on it unchanged. What it does differs: it
//! has no model. It sends the round's message to the remote agent, looks at the task with `GetTask`
//! on a timer, and reports the answer, as the tool of a one-shot remote subagent does
//! ([`RemoteSubagentTool`](crate::remote::RemoteSubagentTool)) but with the context of the
//! conversation carried from round to round.
//!
//! Every call to the remote is a journaled step, and the task id is committed in the round's state
//! as soon as it is known, so a restart looks at the task again and a replay sends nothing twice
//! (the message id is derived from the round). The files of an answer are described in a line and
//! not shared, and the tokens the remote spent are not counted. Nothing here logs or journals the
//! bearer token.

use std::sync::Arc;
use std::time::Duration;

use a2a::{
    CancelTaskRequest, GetTaskRequest, Message, Part, Role, SendMessageConfiguration,
    SendMessageRequest, SendMessageResponse, Task, TaskState,
};
use adam_llm_agent::{MESSAGE_KIND, ToolError};
use adam_runtime::{
    Agent, AgentError, AgentStarter, Ctx, Inbound, RuntimeError, SESSION_CLOSE_KIND,
    SESSION_CLOSE_ROUND_KEY, SESSION_STATE_KEY, SessionHeader, SessionState, Transition,
    child_run_id, closing_round_payload,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::remote::{Client, Outcome, RemoteEndpoint, STEER_EXTENSION_URI, message_text};

/// How long a cancel of the remote task may take before it is given up on.
const CANCEL_TIMEOUT: Duration = Duration::from_secs(10);

/// The first look at a task comes this soon after the send; each look that finds it still going
/// doubles the wait, up to [`RemoteRoundAgent::with_wait_poll`].
const FIRST_LOOK: Duration = Duration::from_secs(2);

/// The most message ids a round remembers having read (what a retried `subagent_send` checks).
const MAX_READ_IDS: usize = 64;

/// The default longest wait between two looks at a task.
const DEFAULT_WAIT_POLL: Duration = Duration::from_secs(60);

/// A message the round has not sent yet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Queued {
    id: String,
    text: String,
}

/// The state of one round, stored as JSON in the run.
///
/// The members the session tools read are `session` (the header) and `read_ids` and
/// `queues_messages`; the rest is the round's own.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RoundState {
    /// What the round says about its session; the runtime reads it from here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session: Option<SessionHeader>,
    /// The ids of the messages the round has taken, newest last, at most [`MAX_READ_IDS`].
    #[serde(default)]
    read_ids: Vec<String>,
    /// A message sent to the working round waits for the remote's task to end (`true`), or is
    /// handed to the working task at once (`false`, the card lists `steer/v1`). `true` until the
    /// round has read the card, because until then a message joins the first send.
    #[serde(default)]
    queues_messages: bool,
    /// The remote's conversation, adopted from its first answer and carried to the next round.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    context_id: Option<String>,
    /// The last task that ended, which the next message names as the one it follows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_task: Option<String>,
    /// A task that waits for an answer (`input-required`): the next message is sent to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    answer_task: Option<String>,
    /// The task that works now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    task: Option<String>,
    /// A task the round before left working (it was cancelled or failed without hearing from the
    /// remote): this round cancels it first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prior_open_task: Option<String>,
    /// A send the round before may have made without learning the task (it was cancelled while the
    /// remote answered): this round repeats it, under the same message id, to find the task and
    /// cancel it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prior_sending: Option<SendMessageRequest>,
    /// The send about to be made, committed before the call: a cancel that lands while the remote
    /// answers leaves the request here, so the task can be found again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sending: Option<SendMessageRequest>,
    /// How many of `queued` the `sending` request carries.
    #[serde(default)]
    sending_count: usize,
    /// When the round first stepped: the round gives up `max_wait` after it, whatever started it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    started: Option<adam_core::time::Timestamp>,
    /// Messages taken from the inbox that no step has dealt with yet.
    #[serde(default)]
    unhandled: Vec<Queued>,
    /// What the next send says: the round's message and the messages that waited.
    #[serde(default)]
    queued: Vec<Queued>,
    /// Set on a **timer run**, never on a round: when the session this round ended may have
    /// expired, a run parked until then ends the remote task of a session nobody reads again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reap: Option<Reap>,
    /// What the remote answered, in order.
    #[serde(default)]
    answers: Vec<String>,
    /// Looks at the working task that found it still going.
    #[serde(default)]
    looks: u32,
    /// Messages sent to the remote by this round.
    #[serde(default)]
    sent: u32,
    /// The card has been read in this round.
    #[serde(default)]
    card_read: bool,
    /// The last thing the round reported, which a round that only closes the session repeats.
    #[serde(default)]
    last_text: String,
    /// What the remote asked and waits to be answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    question: Option<String>,
    /// Why the round failed, once the remote said so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    failure: Option<String>,
}

/// How far a remote task has got.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Stage {
    Working,
    Completed,
    InputRequired,
    Failed,
}

/// What a call to the remote found, in the plain form a journal keeps.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Seen {
    task: Option<String>,
    context: Option<String>,
    stage: Stage,
    text: String,
}

/// A call to the remote that did not work, as a journal keeps it.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct CallFailure {
    transient: bool,
    text: String,
}

impl CallFailure {
    fn of(error: ToolError) -> Self {
        match error {
            ToolError::Transient(text) | ToolError::Retryable { message: text, .. } => Self {
                transient: true,
                text,
            },
            ToolError::Permanent(text) => Self {
                transient: false,
                text,
            },
            other => Self {
                transient: false,
                text: other.to_string(),
            },
        }
    }
}

/// What the round does next, from its state alone.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    /// A round that only closes its session: answer with the last report and call nothing.
    Close,
    /// Cancel the task the round before left working.
    CancelPrior,
    /// Read the agent card, to know whether a working task takes messages.
    ReadCard,
    /// Deal with a message taken from the inbox: steer it into the working task or queue it.
    Handle(Queued),
    /// Send what is queued as a new task.
    Send,
    /// Look at the working task.
    Poll(String),
    /// Nothing is in flight and nothing waits: report.
    Finish,
}

/// [`Inbound::kind`] of the message that starts a timer run of a session (see [`Reap`]).
const REAP_KIND: &str = "adam.session.reap";

/// What a timer run watches: round `round` of `session`, the latest when the timer was set, and
/// the instant the session expires if nobody writes to it.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Reap {
    session: adam_core::RunId,
    round: adam_core::RunId,
    due: adam_core::time::Timestamp,
}

impl RoundState {
    fn closing(&self) -> bool {
        self.session.as_ref().is_some_and(|header| header.closing)
    }

    fn next(&self) -> Action {
        // What the round before left working is cancelled first, a closing round included.
        if self.prior_open_task.is_some() || self.prior_sending.is_some() {
            return Action::CancelPrior;
        }
        if self.closing() {
            return Action::Close;
        }
        if !self.card_read {
            return Action::ReadCard;
        }
        // A task that failed ends the round: what waits is not sent.
        if self.failure.is_some() && self.task.is_none() {
            return Action::Finish;
        }
        if let Some(message) = self.unhandled.first() {
            return Action::Handle(message.clone());
        }
        if self.task.is_none() && !self.queued.is_empty() {
            return Action::Send;
        }
        match &self.task {
            Some(task) => Action::Poll(task.clone()),
            None => Action::Finish,
        }
    }

    /// Take what the inbox holds: a close request, and the messages not read before.
    fn absorb_inbox(&mut self, inbox: Vec<Inbound>) {
        for inbound in inbox {
            if inbound.kind == SESSION_CLOSE_KIND {
                if let Some(header) = &mut self.session {
                    header.closed = true;
                }
            } else if inbound.kind == MESSAGE_KIND && !self.read_ids.contains(&inbound.id) {
                let text = text_of(&inbound.payload).unwrap_or_default();
                self.read_ids.push(inbound.id.clone());
                if self.read_ids.len() > MAX_READ_IDS {
                    self.read_ids.remove(0);
                }
                self.unhandled.push(Queued {
                    id: inbound.id,
                    text,
                });
            }
            // Anything else (a report of a round, a notice) is a wake-up and is dropped.
        }
    }

    /// Put the first of `unhandled` in the queue.
    fn queue_first(&mut self) {
        if !self.unhandled.is_empty() {
            let message = self.unhandled.remove(0);
            self.queued.push(message);
        }
    }

    /// Record what the remote said about a task.
    fn record(&mut self, seen: Seen) {
        if let Some(context) = seen.context {
            self.context_id = Some(context);
        }
        self.task = None;
        match seen.stage {
            Stage::Working => {
                self.task = seen.task;
            }
            Stage::Completed => {
                self.answers.push(seen.text);
                self.last_task = seen.task.or(self.last_task.take());
            }
            Stage::InputRequired => {
                self.last_task = seen.task.clone().or(self.last_task.take());
                self.answer_task = seen.task;
                self.question = Some(seen.text);
            }
            Stage::Failed => {
                self.failure = Some(seen.text);
                self.last_task = seen.task.or(self.last_task.take());
            }
        }
    }

    /// A send went through: drop the messages it carried (not those that joined the queue since it
    /// was committed) and take what the remote said.
    fn sent_ok(&mut self, seen: Seen) {
        self.sent += 1;
        let taken = self.sending_count.min(self.queued.len());
        self.queued.drain(..taken);
        self.sending = None;
        self.sending_count = 0;
        self.answer_task = None;
        self.question = None;
        self.looks = 0;
        self.record(seen);
    }

    /// What the round reports when nothing is left to do.
    fn finish(&mut self) -> Result<String, String> {
        if let Some(failure) = &self.failure {
            return Err(failure.clone());
        }
        let text = match &self.question {
            Some(question) => format!(
                "{question}\n\n(the remote agent waits for an answer: send it with \
                 `subagent_send`)"
            ),
            None => self.answers.join("\n\n"),
        };
        self.last_text.clone_from(&text);
        Ok(text)
    }
}

/// The text of a message payload: `{"text": ".."}` or a bare string.
fn text_of(payload: &Value) -> Option<String> {
    match payload {
        Value::String(text) => Some(text.clone()),
        Value::Object(map) => map.get("text").and_then(Value::as_str).map(str::to_owned),
        _ => None,
    }
}

/// The agent that works the rounds of a session on one remote subagent, registered as
/// `<root>/<subagent>` beside the agents of the assembly.
///
/// [`Assembly::register`](crate::Assembly::register) registers them; a composition root that
/// registers the assembly's agents one by one adds
/// [`Assembly::remote_round_agents`](crate::Assembly::remote_round_agents) too. **A process that
/// registers only its starter cannot cancel the remote task of a round it cancels** (the cancel
/// is sent by the agent, in the process that commits it).
#[derive(Clone, Debug)]
pub struct RemoteRoundAgent {
    name: String,
    endpoint: Arc<RemoteEndpoint>,
    wait_poll: Duration,
    /// The longest a round works, from its first step: the round's own limit, for a round the
    /// runtime was given none for.
    max_wait: Duration,
}

impl RemoteRoundAgent {
    pub(crate) fn new(name: String, endpoint: Arc<RemoteEndpoint>, max_wait: Duration) -> Self {
        Self {
            name,
            endpoint,
            max_wait,
            wait_poll: DEFAULT_WAIT_POLL,
        }
    }

    /// The longest wait between two looks at a working task (default 60 seconds). The first look
    /// is soon after the send and each look that finds the task going doubles the wait.
    #[must_use]
    pub fn with_wait_poll(mut self, every: Duration) -> Self {
        self.wait_poll = every.max(Duration::from_millis(1));
        self
    }

    /// One `ctx.step` that calls the remote: the call's outcome is journaled, a transient failure
    /// retries the transition, a permanent one is the round's end.
    async fn remote_step<T, F, Fut>(
        ctx: &mut Ctx,
        name: &str,
        call: F,
    ) -> Result<Result<T, String>, AgentError>
    where
        T: Serialize + serde::de::DeserializeOwned,
        F: FnOnce() -> Fut + Send,
        Fut: std::future::Future<Output = Result<T, CallFailure>> + Send,
    {
        let outcome: Result<T, CallFailure> = ctx.step(name, call).await?;
        match outcome {
            Ok(value) => Ok(Ok(value)),
            Err(failure) if failure.transient => Err(AgentError::transient(failure.text)),
            Err(failure) => Ok(Err(failure.text)),
        }
    }

    fn message(run: adam_core::RunId, st: &RoundState, text: &str) -> SendMessageRequest {
        let message_id = if st.sent == 0 {
            run.to_string()
        } else {
            child_run_id(run, &format!("queued:{}", st.sent)).to_string()
        };
        let mut message = Message::new(Role::User, vec![Part::text(text)]);
        message.message_id = message_id;
        message.context_id.clone_from(&st.context_id);
        message.task_id.clone_from(&st.answer_task);
        if st.answer_task.is_none() && st.context_id.is_some() {
            message.reference_task_ids = st.last_task.clone().map(|task| vec![task]);
        }
        request_of(message)
    }

    async fn read_card(
        &self,
        ctx: &mut Ctx,
        st: &mut RoundState,
    ) -> Result<Option<String>, AgentError> {
        let endpoint = self.endpoint.clone();
        let steers = Self::remote_step(ctx, "card", move || async move {
            endpoint
                .refresh()
                .await
                .map(|(_, steers)| steers)
                .map_err(CallFailure::of)
        })
        .await?;
        Ok(match steers {
            Ok(steers) => {
                st.card_read = true;
                st.queues_messages = !steers;
                None
            }
            Err(text) => Some(text),
        })
    }

    async fn cancel_prior(&self, ctx: &mut Ctx, st: &mut RoundState) -> Result<(), AgentError> {
        let (endpoint, name) = (self.endpoint.clone(), self.name.clone());
        let (task, probe) = (st.prior_open_task.clone(), st.prior_sending.clone());
        let sent: Result<(), CallFailure> = ctx
            .step("cancel-prior", move || async move {
                stop_tasks(&endpoint, &name, task, probe, "the round before").await;
                Ok(())
            })
            .await?;
        let _ = sent;
        st.prior_open_task = None;
        st.prior_sending = None;
        Ok(())
    }

    /// Hand the first unhandled message to the working task, or queue it.
    async fn handle(
        &self,
        ctx: &mut Ctx,
        st: &mut RoundState,
        message: Queued,
    ) -> Result<(), AgentError> {
        let Some(task) = st.task.clone().filter(|_| !st.queues_messages) else {
            st.queue_first();
            return Ok(());
        };
        let mut steer = Message::new(Role::User, vec![Part::text(message.text.as_str())]);
        steer.message_id = child_run_id(ctx.run_id(), &message.id).to_string();
        steer.context_id.clone_from(&st.context_id);
        steer.task_id = Some(task);
        steer.extensions = Some(vec![STEER_EXTENSION_URI.to_owned()]);
        let request = request_of(steer);
        let endpoint = self.endpoint.clone();
        let sent = Self::remote_step(ctx, &format!("steer:{}", message.id), move || async move {
            let client = endpoint.client().await.map_err(CallFailure::of)?;
            client
                .send_message(&request)
                .await
                .map(|_| ())
                .map_err(|e| CallFailure::of(endpoint.failure("send the message to", &e)))
        })
        .await?;
        match sent {
            Ok(()) => {
                st.unhandled.remove(0);
            }
            // The remote cannot take it now (the task ended, or it does not steer after all): it
            // is sent as a task of its own.
            Err(_) => st.queue_first(),
        }
        Ok(())
    }

    /// Send what is queued. Returns the failure text when the round cannot go on.
    async fn send(&self, ctx: &mut Ctx, st: &mut RoundState) -> Result<Option<String>, AgentError> {
        // The request is committed before it is made, so a cancel that lands while the remote
        // answers leaves what is needed to find the task again.
        let Some(request) = st.sending.clone() else {
            let text = st
                .queued
                .iter()
                .map(|queued| queued.text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            st.sending = Some(Self::message(ctx.run_id(), st, &text));
            st.sending_count = st.queued.len();
            return Ok(None);
        };
        let endpoint = self.endpoint.clone();
        let seen = Self::remote_step(ctx, &format!("send:{}", st.sent), move || async move {
            let client = endpoint.client().await.map_err(CallFailure::of)?;
            let response = client
                .send_message(&request)
                .await
                .map_err(|e| CallFailure::of(endpoint.failure("send the message to", &e)))?;
            Ok(seen_of_response(&endpoint, response))
        })
        .await?;
        let seen = match seen {
            Ok(seen) => seen,
            Err(text) => return Ok(Some(text)),
        };
        st.sent_ok(seen);
        Ok(None)
    }

    /// The round has worked past its limit: stop the task and fail.
    async fn give_up(&self, ctx: &mut Ctx, task: String) -> Result<String, AgentError> {
        let (endpoint, name) = (self.endpoint.clone(), self.name.clone());
        let sent: Result<(), CallFailure> = ctx
            .step("cancel-limit", move || async move {
                stop_tasks(&endpoint, &name, Some(task), None, "the time limit").await;
                Ok(())
            })
            .await?;
        let _ = sent;
        Ok(format!(
            "time limit: the remote task did not finish within {} s",
            self.max_wait.as_secs()
        ))
    }

    /// Look at the working task. `Ok(Some(text))`: the round cannot go on.
    async fn poll(
        &self,
        ctx: &mut Ctx,
        st: &mut RoundState,
        task: String,
    ) -> Result<Option<String>, AgentError> {
        let endpoint = self.endpoint.clone();
        let id = task.clone();
        let seen = Self::remote_step(
            ctx,
            &format!("poll:{task}:{}", st.looks),
            move || async move {
                let client = endpoint.client().await.map_err(CallFailure::of)?;
                let found = client
                    .get_task(&GetTaskRequest {
                        id,
                        history_length: Some(0),
                        tenant: None,
                    })
                    .await
                    .map_err(|e| CallFailure::of(endpoint.failure("look at the task on", &e)))?;
                Ok(seen_of_task(&endpoint, &found))
            },
        )
        .await?;
        match seen {
            Ok(seen) => {
                st.record(seen);
                Ok(None)
            }
            Err(text) => Ok(Some(text)),
        }
    }

    /// The instant of the next look at the task.
    async fn next_look(
        &self,
        ctx: &mut Ctx,
        looks: u32,
    ) -> Result<adam_core::time::Timestamp, AgentError> {
        let wait = look_delay(looks, self.wait_poll);
        let now = ctx.now_journaled().await?;
        Ok(now + adam_core::time::span_of(wait))
    }

    /// A round of a session that may stay open has ended: start the timer run that, if nobody
    /// writes to the session by the time it expires, starts the closing round whose first act is to
    /// cancel what the remote still runs. Best effort and journaled: a failure is a warning, and
    /// the next read of the session cleans up as well. The timer is a run of this agent in a
    /// conversation of the session's parent, parked until the expiry, so a worker wakes it with no process
    /// of its own.
    async fn schedule_reap(&self, ctx: &mut Ctx, st: &RoundState) -> Result<(), AgentError> {
        let Some(header) = &st.session else {
            return Ok(());
        };
        if header.closing || header.closed {
            return Ok(());
        }
        let starter = ctx.child_starter();
        let ttl = adam_core::time::span_of(starter.session_idle_ttl());
        let round = ctx.run_id();
        let due = ctx.now_journaled().await? + ttl;
        let input = Inbound::new(
            REAP_KIND,
            serde_json::to_value(Reap {
                session: header.id,
                round,
                due,
            })
            .map_err(|e| AgentError::permanent(format!("the timer message: {e}")))?,
        );
        let (id, name) = (child_run_id(round, "reap"), self.name.clone());
        let started: Result<(), String> = ctx
            .step("schedule-reap", move || async move {
                starter
                    .start_in_parent_conversation(id, &name, input)
                    .await
                    .map(drop)
                    .map_err(|e| e.to_string())
            })
            .await?;
        if let Err(error) = started {
            tracing::warn!(agent = %self.name, %error, "the expiry timer of a session was not set");
        }
        Ok(())
    }

    /// The timer run: wait for the expiry, then, if the session is as the round left it (no other
    /// round, none working, no closing round), start the closing round, which a worker steps and
    /// which cancels the remote task. Any activity since makes it a no-op, because everything is
    /// read again here.
    async fn reap(
        ctx: &mut Ctx,
        st: RoundState,
        agent: &str,
    ) -> Result<Transition<RoundState>, AgentError> {
        let done = |st: RoundState, what: &str| {
            Ok(Transition::Done {
                state: st,
                output: json!({ "reaped": what }),
            })
        };
        let Some(reap) = st.reap.clone() else {
            return done(st, "nothing");
        };
        let now = ctx.now();
        if now < reap.due {
            return Ok(Transition::Park {
                wake_at: Some(reap.due),
                state: st,
            });
        }
        let starter = ctx.child_starter();
        let Some(view) = starter
            .session(reap.session)
            .await
            .map_err(AgentError::transient)?
        else {
            return done(st, "gone");
        };
        let Some(header) = view.header.clone() else {
            return done(st, "no header");
        };
        if view.latest_run != reap.round || view.state == SessionState::Working || header.closing {
            return done(st, "activity since");
        }
        if !(view.idle_expired || view.closed) {
            // The clocks differ by a moment: look again.
            return Ok(Transition::Park {
                wake_at: Some(now + jiff::SignedDuration::from_secs(1)),
                state: st,
            });
        }
        let mut header = header;
        header.round = view.round + 1;
        header.closed = true;
        header.closing = true;
        let input = Inbound::new(MESSAGE_KIND, closing_round_payload(&header));
        match starter
            .start_round(
                reap.session,
                child_run_id(ctx.run_id(), "close"),
                agent,
                input,
                Some(view.latest_run),
                None,
            )
            .await
        {
            // Busy: a reader got there first, and that round cancels as well.
            Ok(_) | Err(RuntimeError::ConversationBusy { .. }) => done(st, "closed"),
            Err(e) => Err(AgentError::transient(e)),
        }
    }

    async fn run(
        &self,
        ctx: &mut Ctx,
        mut st: RoundState,
    ) -> Result<Transition<RoundState>, AgentError> {
        if st.reap.is_some() {
            return Self::reap(ctx, st, &self.name).await;
        }
        ctx.reopen_on_arrival();
        let started = *st.started.get_or_insert_with(|| ctx.now());
        st.absorb_inbox(ctx.take_inbox());
        ctx.read_already(&st.read_ids);
        let failed = match st.next() {
            Action::Close => {
                let output = json!({ "text": st.last_text, "closed": true });
                return Ok(Transition::Done { state: st, output });
            }
            Action::CancelPrior => {
                self.cancel_prior(ctx, &mut st).await?;
                None
            }
            Action::ReadCard => self.read_card(ctx, &mut st).await?,
            Action::Handle(message) => {
                self.handle(ctx, &mut st, message).await?;
                None
            }
            Action::Send => self.send(ctx, &mut st).await?,
            Action::Poll(task) if past_limit(ctx.now(), started, self.max_wait) => {
                st.task = None;
                Some(self.give_up(ctx, task).await?)
            }
            Action::Poll(task) => {
                let failed = self.poll(ctx, &mut st, task).await?;
                if failed.is_none() && st.task.is_some() {
                    let wake_at = self.next_look(ctx, st.looks).await?;
                    st.looks = st.looks.saturating_add(1);
                    return Ok(Transition::Park {
                        state: st,
                        wake_at: Some(wake_at),
                    });
                }
                failed
            }
            Action::Finish => {
                let finished = st.finish();
                if finished.is_ok() {
                    self.schedule_reap(ctx, &st).await?;
                }
                return Ok(match finished {
                    Ok(text) => Transition::Done {
                        state: st,
                        output: json!({ "text": text }),
                    },
                    Err(error) => Transition::Fail { state: st, error },
                });
            }
        };
        Ok(match failed {
            Some(error) => Transition::Fail { state: st, error },
            None => Transition::Continue(st),
        })
    }
}

/// The wait before look number `looks` (from 0): [`FIRST_LOOK`], doubled by each look, up to `cap`.
fn look_delay(looks: u32, cap: Duration) -> Duration {
    FIRST_LOOK.saturating_mul(1_u32 << looks.min(10)).min(cap)
}

/// Whether a round that first stepped at `started` has worked `max_wait`.
fn past_limit(
    now: adam_core::time::Timestamp,
    started: adam_core::time::Timestamp,
    max_wait: Duration,
) -> bool {
    now - started > adam_core::time::span_of(max_wait)
}

fn request_of(message: Message) -> SendMessageRequest {
    SendMessageRequest {
        message,
        configuration: Some(SendMessageConfiguration {
            accepted_output_modes: None,
            task_push_notification_config: None,
            history_length: Some(0),
            return_immediately: Some(true),
        }),
        metadata: None,
        tenant: None,
    }
}

fn seen_of_response(endpoint: &RemoteEndpoint, response: SendMessageResponse) -> Seen {
    match response {
        SendMessageResponse::Task(task) => seen_of_task(endpoint, &task),
        SendMessageResponse::Message(reply) => {
            let output = RemoteEndpoint::reply_with(&reply, None);
            Seen {
                task: None,
                context: reply.context_id.clone(),
                stage: Stage::Completed,
                text: output.content,
            }
        }
    }
}

fn seen_of_task(endpoint: &RemoteEndpoint, task: &Task) -> Seen {
    let (stage, text) = match &task.status.state {
        TaskState::InputRequired => {
            let said = task.status.message.as_ref().map(message_text);
            let said = said.filter(|said| !said.trim().is_empty());
            (
                Stage::InputRequired,
                said.unwrap_or_else(|| "(the remote agent asked for more input)".to_owned()),
            )
        }
        state => match endpoint.outcome_with(task, None) {
            Outcome::Working => (Stage::Working, String::new()),
            Outcome::Done(output) if *state == TaskState::Completed => {
                (Stage::Completed, output.content)
            }
            Outcome::Done(output) => (Stage::Failed, output.content),
        },
    };
    Seen {
        task: Some(task.id.clone()),
        context: Some(task.context_id.clone()),
        stage,
        text,
    }
}

/// Ask the remote to stop `task`: best effort, bounded, and said in the log either way (never the
/// token, never the remote's words).
async fn cancel_task(endpoint: &RemoteEndpoint, agent: &str, task: &str, why: &str) {
    let attempt = async {
        let client: Arc<Client> = endpoint.client().await.map_err(|e| format!("{e}"))?;
        client
            .cancel_task(&CancelTaskRequest {
                id: task.to_owned(),
                metadata: None,
                tenant: None,
            })
            .await
            .map(|_| ())
            .map_err(|e| format!("the remote answered error {}", e.code))
    };
    match tokio::time::timeout(CANCEL_TIMEOUT, attempt).await {
        Ok(Ok(())) => tracing::info!(agent, task, why, "cancelled the remote task"),
        Ok(Err(reason)) => {
            tracing::warn!(agent, task, why, %reason, "could not cancel the remote task");
        }
        Err(_) => tracing::warn!(agent, task, why, "cancelling the remote task timed out"),
    }
}

/// Repeat a send that may have been made, under its own message id: a remote that deduplicates
/// returns the task it made, and its id is what to cancel. A remote that made none now makes one,
/// which is then cancelled.
async fn find_task(endpoint: &RemoteEndpoint, request: &SendMessageRequest) -> Option<String> {
    let attempt = async {
        let client = endpoint.client().await.ok()?;
        match client.send_message(request).await.ok()? {
            SendMessageResponse::Task(task) => Some(task.id),
            SendMessageResponse::Message(_) => None,
        }
    };
    tokio::time::timeout(CANCEL_TIMEOUT, attempt)
        .await
        .ok()
        .flatten()
}

/// Cancel the working `task`, and the task a send that may have been made started. One try each.
async fn stop_tasks(
    endpoint: &RemoteEndpoint,
    agent: &str,
    task: Option<String>,
    probe: Option<SendMessageRequest>,
    why: &str,
) {
    let found = match (&task, &probe) {
        (None, Some(request)) => find_task(endpoint, request).await,
        _ => None,
    };
    for task in task.into_iter().chain(found) {
        cancel_task(endpoint, agent, &task, why).await;
    }
}

/// The state a round starts with, from its message (or the close): never from the endpoint, so
/// the front that starts the round and the worker that steps it make the same state.
fn start_round(input: &Inbound) -> Result<RoundState, AgentError> {
    let payload = &input.payload;
    if input.kind == REAP_KIND {
        let reap: Reap = serde_json::from_value(payload.clone())
            .map_err(|e| AgentError::permanent(format!("unusable timer message: {e}")))?;
        return Ok(RoundState {
            reap: Some(reap),
            ..RoundState::default()
        });
    }
    let mut header: Option<SessionHeader> = payload
        .get(SESSION_STATE_KEY)
        .and_then(|value| serde_json::from_value(value.clone()).ok());
    let closes = payload
        .get(SESSION_CLOSE_ROUND_KEY)
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if closes && let Some(header) = &mut header {
        header.closing = true;
    }
    let mut state = RoundState {
        session: header,
        queues_messages: true,
        ..RoundState::default()
    };
    if !closes {
        match text_of(payload).filter(|text| !text.trim().is_empty()) {
            Some(text) => state.queued.push(Queued {
                id: String::new(),
                text,
            }),
            None => {
                return Err(AgentError::permanent(
                    "unusable start message: it has no text for the remote agent",
                ));
            }
        }
    }
    Ok(state)
}

/// The state of a later round: `start_round`'s, with what the prior round knew carried over.
fn carry_round(input: &Inbound, prior: &RoundState) -> Result<RoundState, AgentError> {
    let mut state = start_round(input)?;
    if state.session.is_none() {
        state.session.clone_from(&prior.session);
    }
    state.context_id.clone_from(&prior.context_id);
    state.last_task.clone_from(&prior.last_task);
    state.answer_task.clone_from(&prior.answer_task);
    state.last_text.clone_from(&prior.last_text);
    state.prior_open_task = prior.task.clone().or_else(|| prior.prior_open_task.clone());
    // A task that waits for an answer is still open at the remote: a round that closes the session
    // cancels it too, and nobody is left to answer it.
    if state.closing()
        && state.prior_open_task.is_none()
        && let Some(waiting) = state.answer_task.take()
    {
        state.prior_open_task = Some(waiting);
    }
    state.prior_sending = prior
        .sending
        .clone()
        .or_else(|| prior.prior_sending.clone());
    // What waited and was never sent goes first: the person's words are not dropped with a failure.
    if state.queued.len() == 1 && !state.closing() {
        let mut carried: Vec<Queued> = prior
            .unhandled
            .iter()
            .chain(&prior.queued)
            .cloned()
            .collect();
        carried.append(&mut state.queued);
        state.queued = carried;
    }
    Ok(state)
}

/// The start-only agent of a session on a remote subagent, registered as `<root>/<subagent>` on a
/// process that serves the A2A front and does not step rounds (a control plane): it makes the same
/// state as [`RemoteRoundAgent`], so a worker with the full agent steps what it starts. It knows
/// no endpoint, holds no credential and cannot cancel the remote task of a round.
/// [`AgentDef::session_starters`](crate::AgentDef::session_starters) registers it.
#[derive(Clone, Debug)]
pub struct RemoteRoundStarter {
    name: String,
}

impl RemoteRoundStarter {
    /// The starter of the round agent registered as `name` (`<root>/<subagent>`).
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

impl AgentStarter for RemoteRoundStarter {
    type State = RoundState;

    fn name(&self) -> &str {
        &self.name
    }

    fn init(&self, input: Inbound) -> Result<RoundState, AgentError> {
        start_round(&input)
    }

    fn init_continuing(
        &self,
        input: Inbound,
        prior: &RoundState,
        _prior_run: adam_core::RunId,
    ) -> Result<RoundState, AgentError> {
        carry_round(&input, prior)
    }
}

#[async_trait]
impl Agent for RemoteRoundAgent {
    type State = RoundState;

    fn name(&self) -> &str {
        &self.name
    }

    fn init(&self, input: Inbound) -> Result<RoundState, AgentError> {
        start_round(&input)
    }

    fn init_continuing(
        &self,
        input: Inbound,
        prior: &RoundState,
        _prior_run: adam_core::RunId,
    ) -> Result<RoundState, AgentError> {
        carry_round(&input, prior)
    }

    async fn step(
        &self,
        ctx: &mut Ctx,
        state: RoundState,
    ) -> Result<Transition<RoundState>, AgentError> {
        self.run(ctx, state).await
    }

    async fn on_cancelled(&self, run: adam_core::RunId, state: Value) {
        let Ok(state) = serde_json::from_value::<RoundState>(state) else {
            return;
        };
        tracing::debug!(agent = %self.name, %run, "the round was cancelled");
        let why = "the round was cancelled";
        stop_tasks(&self.endpoint, &self.name, state.task, state.sending, why).await;
        stop_tasks(
            &self.endpoint,
            &self.name,
            state.prior_open_task,
            state.prior_sending,
            why,
        )
        .await;
    }
}

impl AgentStarter for RemoteRoundAgent {
    type State = RoundState;

    fn name(&self) -> &str {
        &self.name
    }

    fn init(&self, input: Inbound) -> Result<RoundState, AgentError> {
        start_round(&input)
    }

    fn init_continuing(
        &self,
        input: Inbound,
        prior: &RoundState,
        _prior_run: adam_core::RunId,
    ) -> Result<RoundState, AgentError> {
        carry_round(&input, prior)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(stage: Stage, task: &str, text: &str) -> Seen {
        Seen {
            task: Some(task.into()),
            context: Some("ctx".into()),
            stage,
            text: text.into(),
        }
    }

    fn message(id: &str, text: &str) -> Inbound {
        Inbound::new(MESSAGE_KIND, json!({ "text": text })).with_id(id)
    }

    fn started() -> RoundState {
        let mut state = RoundState {
            queues_messages: true,
            card_read: true,
            ..RoundState::default()
        };
        state.queued.push(Queued {
            id: String::new(),
            text: "first".into(),
        });
        state
    }

    #[test]
    fn what_a_round_does_next_follows_from_its_state_alone() {
        let mut state = RoundState::default();
        assert_eq!(state.next(), Action::ReadCard, "the card comes first");
        state.card_read = true;
        assert_eq!(state.next(), Action::Finish);
        state.queued.push(Queued {
            id: String::new(),
            text: "go".into(),
        });
        assert_eq!(state.next(), Action::Send);
        state.record(seen(Stage::Working, "t1", ""));
        state.queued.clear();
        assert_eq!(state.next(), Action::Poll("t1".into()));
        state.absorb_inbox(vec![message("m1", "more")]);
        assert!(
            matches!(state.next(), Action::Handle(m) if m.id == "m1"),
            "a message is dealt with before the next look"
        );
        state.session = Some(SessionHeader {
            id: adam_core::RunId::new(),
            agent: "a".into(),
            description: String::new(),
            mode: adam_runtime::SessionMode::Open,
            closed: true,
            closing: true,
            round: 2,
            root: None,
            workspace: None,
            visible: false,
            config: None,
        });
        assert_eq!(state.next(), Action::Close, "a closing round calls nothing");
        // But it cancels what the round before left working first.
        state.prior_open_task = Some("old".into());
        assert_eq!(state.next(), Action::CancelPrior);
    }

    #[test]
    fn a_message_is_read_once_and_a_wake_up_is_dropped() {
        let mut state = started();
        state.absorb_inbox(vec![
            message("m1", "one"),
            message("m1", "one again"),
            Inbound::new(adam_runtime::CHILD_REPORTED_KIND, json!({})),
            Inbound::new(SESSION_CLOSE_KIND, json!({})),
        ]);
        assert_eq!(state.unhandled.len(), 1);
        assert_eq!(state.read_ids, ["m1"]);
        // The close request is only noted when the round has a header.
        assert!(state.session.is_none());
        for n in 0..(MAX_READ_IDS + 5) {
            state.absorb_inbox(vec![message(&format!("x{n}"), "x")]);
        }
        assert_eq!(state.read_ids.len(), MAX_READ_IDS);
    }

    #[test]
    fn a_task_that_ends_is_recorded_by_how_it_ended() {
        let mut state = started();
        state.record(seen(Stage::Working, "t1", ""));
        assert_eq!(state.task.as_deref(), Some("t1"));
        assert_eq!(state.context_id.as_deref(), Some("ctx"));
        state.record(seen(Stage::Completed, "t1", "answer"));
        assert_eq!(
            (state.task.as_deref(), state.last_task.as_deref()),
            (None, Some("t1"))
        );
        assert_eq!(state.finish().unwrap(), "answer");

        let mut asking = started();
        asking.record(seen(Stage::InputRequired, "t2", "which one?"));
        assert_eq!(asking.answer_task.as_deref(), Some("t2"));
        assert!(asking.finish().unwrap().contains("which one?"));

        let mut failing = started();
        failing.record(seen(Stage::Failed, "t3", "out of budget"));
        assert_eq!(
            failing.next(),
            Action::Finish,
            "a failed round sends nothing more"
        );
        assert_eq!(
            failing.queued.len(),
            1,
            "what waited is kept for the next round"
        );
        assert_eq!(failing.finish().unwrap_err(), "out of budget");
    }

    #[test]
    fn debug_hides_the_token_and_the_state_holds_none() {
        use crate::def::{RemoteSettings, Secrets};
        use crate::error::Origin;
        use crate::remote::RemoteEndpoint;
        use adam_agent_fs::{RemoteAgent, RemoteAuth};
        use secrecy::SecretString;
        let agent = RemoteAgent {
            name: "billing".into(),
            description: "d".into(),
            url: "https://billing.example.com/card".into(),
            auth: Some(RemoteAuth::Bearer { env: "T".into() }),
            files: false,
            note: String::new(),
            path: "agent/subagents/billing.md".into(),
        };
        let secrets = Secrets {
            env: Arc::new(
                [(
                    "T".to_owned(),
                    SecretString::from("s3cret-token".to_owned()),
                )]
                .into(),
            ),
            ..Secrets::default()
        };
        let endpoint = RemoteEndpoint::bind(
            &Origin::new("coder/billing", "billing.md"),
            &agent,
            &RemoteSettings::default(),
            &secrets,
        )
        .unwrap();
        let round = RemoteRoundAgent::new(
            "coder/billing".into(),
            Arc::new(endpoint),
            DEFAULT_WAIT_POLL,
        );
        let debug = format!("{round:?}");
        assert!(
            debug.contains("[REDACTED]") && !debug.contains("s3cret"),
            "{debug}"
        );
        let json = serde_json::to_string(&started()).unwrap();
        assert!(!json.contains("s3cret"));
    }

    #[test]
    fn a_round_starts_from_a_message_and_a_continuation_carries_the_remotes_context() {
        let first = start_round(&Inbound::new("start", json!({"text": "invoice 7"}))).unwrap();
        assert!(first.queues_messages, "until the card says otherwise");
        assert_eq!(first.queued[0].text, "invoice 7");
        assert!(start_round(&Inbound::new("start", json!({"text": "  "}))).is_err());

        let mut before = first.clone();
        before.context_id = Some("ctx".into());
        before.last_task = Some("t1".into());
        before.task = Some("t2".into());
        before.last_text = "answer".into();
        let next = carry_round(&Inbound::new("start", json!({"text": "next"})), &before).unwrap();
        assert_eq!(next.context_id.as_deref(), Some("ctx"));
        assert_eq!(next.last_task.as_deref(), Some("t1"));
        assert_eq!(next.prior_open_task.as_deref(), Some("t2"));
        assert!(!next.card_read, "the card is read once per round");
        let texts: Vec<&str> = next.queued.iter().map(|q| q.text.as_str()).collect();
        assert_eq!(
            texts,
            ["invoice 7", "next"],
            "what was never sent goes first"
        );
    }

    fn round_agent() -> RemoteRoundAgent {
        use crate::def::{RemoteSettings, Secrets};
        use crate::error::Origin;
        use crate::remote::RemoteEndpoint;
        use adam_agent_fs::RemoteAgent;
        let agent = RemoteAgent {
            name: "billing".into(),
            description: "d".into(),
            url: "https://billing.example.com/card".into(),
            auth: None,
            files: false,
            note: String::new(),
            path: "agent/subagents/billing.md".into(),
        };
        let endpoint = RemoteEndpoint::bind(
            &Origin::new("coder/billing", "billing.md"),
            &agent,
            &RemoteSettings::default(),
            &Secrets::default(),
        )
        .unwrap();
        RemoteRoundAgent::new(
            "coder/billing".into(),
            Arc::new(endpoint),
            DEFAULT_WAIT_POLL,
        )
    }

    #[test]
    fn what_waited_goes_first_in_the_order_it_was_taken_and_what_may_have_been_sent_is_carried() {
        let mut before = start_round(&Inbound::new("start", json!({"text": "ignored"}))).unwrap();
        before.queued = vec![Queued {
            id: "q".into(),
            text: "queued".into(),
        }];
        before.unhandled = vec![Queued {
            id: "u".into(),
            text: "unhandled".into(),
        }];
        let next = carry_round(&Inbound::new("start", json!({"text": "next"})), &before).unwrap();
        let texts: Vec<&str> = next.queued.iter().map(|q| q.text.as_str()).collect();
        assert_eq!(texts, ["unhandled", "queued", "next"]);

        // A send the round before may have made, and one it inherited, are not lost by a round that
        // never got to cancel them.
        let probe = request_of(Message::new(Role::User, vec![Part::text("p")]));
        let mut third = start_round(&Inbound::new("start", json!({"text": "x"}))).unwrap();
        third.prior_sending = Some(probe);
        let fourth = carry_round(&Inbound::new("start", json!({"text": "y"})), &third).unwrap();
        assert!(
            fourth.prior_sending.is_some(),
            "an inherited probe is carried on"
        );
    }

    #[test]
    fn a_reply_with_no_task_keeps_the_task_the_round_followed() {
        let mut state = started();
        state.last_task = Some("t1".into());
        state.record(Seen {
            task: None,
            context: None,
            stage: Stage::Completed,
            text: "plain reply".into(),
        });
        assert_eq!(state.last_task.as_deref(), Some("t1"));
    }

    #[test]
    fn a_send_that_went_through_drops_only_the_messages_it_carried() {
        let mut state = started();
        state.sending = Some(request_of(Message::new(
            Role::User,
            vec![Part::text("first")],
        )));
        state.sending_count = 1;
        // A message joined the queue after the request was committed.
        state.queued.push(Queued {
            id: "late".into(),
            text: "late".into(),
        });
        state.sent_ok(seen(Stage::Working, "t1", ""));
        let left: Vec<&str> = state.queued.iter().map(|q| q.text.as_str()).collect();
        assert_eq!(left, ["late"]);
        assert!(state.sending.is_none());
        assert_eq!(state.sent, 1);
    }

    #[test]
    fn the_wait_between_looks_doubles_up_to_the_cap() {
        let cap = Duration::from_secs(60);
        assert_eq!(look_delay(0, cap), FIRST_LOOK);
        assert_eq!(look_delay(1, cap), FIRST_LOOK * 2);
        assert_eq!(look_delay(3, cap), FIRST_LOOK * 8);
        assert_eq!(look_delay(10, cap), cap);
        assert_eq!(
            look_delay(2, Duration::from_secs(5)),
            Duration::from_secs(5)
        );
    }

    /// The front's starter and the worker's agent each make the state a round starts from, closing
    /// and carried messages included, checked against what that state must be and not against
    /// each other: the worker steps what the front started.
    #[test]
    fn the_starter_and_the_round_agent_each_make_the_state_a_round_needs() {
        let starter = RemoteRoundStarter::new("coder/billing");
        let agent = round_agent();
        assert_eq!(AgentStarter::name(&starter), "coder/billing");
        assert_eq!(AgentStarter::name(&agent), AgentStarter::name(&starter));

        let open = Inbound::new("start", json!({"text": "invoice 7"}));
        let mut want = RoundState {
            queues_messages: true,
            ..RoundState::default()
        };
        want.queued.push(Queued {
            id: String::new(),
            text: "invoice 7".into(),
        });
        let want = serde_json::to_value(&want).unwrap();
        assert_eq!(want["queued"][0]["text"], "invoice 7");
        for state in [
            AgentStarter::init(&starter, open.clone()).unwrap(),
            AgentStarter::init(&agent, open.clone()).unwrap(),
        ] {
            assert_eq!(serde_json::to_value(&state).unwrap(), want);
            assert!(!state.closing());
        }
        let blank = Inbound::new("start", json!({"text": " "}));
        assert!(AgentStarter::init(&starter, blank.clone()).is_err());
        assert!(AgentStarter::init(&agent, blank).is_err());

        // A close names the session and needs no text.
        let header = SessionHeader {
            id: adam_core::RunId::new(),
            agent: "coder/billing".into(),
            description: "billing".into(),
            mode: adam_runtime::SessionMode::Open,
            closed: false,
            closing: false,
            round: 1,
            root: None,
            workspace: None,
            visible: false,
            config: None,
        };
        let close = Inbound::new(
            SESSION_CLOSE_KIND,
            json!({ SESSION_STATE_KEY: header, SESSION_CLOSE_ROUND_KEY: true }),
        );
        for state in [
            AgentStarter::init(&starter, close.clone()).unwrap(),
            AgentStarter::init(&agent, close.clone()).unwrap(),
        ] {
            assert!(state.closing());
            assert!(state.queued.is_empty(), "a close has nothing to send");
            assert_eq!(state.session.as_ref().map(|h| h.id), Some(header.id));
        }

        // A later round carries the context, the task to cancel and what was never sent.
        let mut before = AgentStarter::init(&agent, message("m1", "first")).unwrap();
        before.context_id = Some("ctx".into());
        before.task = Some("t1".into());
        before.unhandled = vec![Queued {
            id: "u".into(),
            text: "unhandled".into(),
        }];
        let run = adam_core::RunId::new();
        for next in [
            AgentStarter::init_continuing(&starter, message("m2", "next"), &before, run).unwrap(),
            AgentStarter::init_continuing(&agent, message("m2", "next"), &before, run).unwrap(),
        ] {
            assert_eq!(next.context_id.as_deref(), Some("ctx"));
            assert_eq!(next.prior_open_task.as_deref(), Some("t1"));
            let texts: Vec<&str> = next.queued.iter().map(|q| q.text.as_str()).collect();
            assert_eq!(
                texts,
                ["unhandled", "first", "next"],
                "what waited goes first"
            );
        }
    }

    /// A closing round cancels the task that waits for an answer, which no round works on.
    #[test]
    fn a_closing_round_cancels_the_task_that_waits_for_an_answer() {
        let agent = round_agent();
        let mut before = AgentStarter::init(&agent, message("m1", "first")).unwrap();
        before.answer_task = Some("t9".into());
        let header = SessionHeader {
            id: adam_core::RunId::new(),
            agent: "coder/billing".into(),
            description: String::new(),
            mode: adam_runtime::SessionMode::Open,
            closed: true,
            closing: true,
            round: 2,
            root: None,
            workspace: None,
            visible: false,
            config: None,
        };
        let close = Inbound::new(
            SESSION_CLOSE_KIND,
            json!({ SESSION_STATE_KEY: header, SESSION_CLOSE_ROUND_KEY: true }),
        );
        let next =
            AgentStarter::init_continuing(&agent, close, &before, adam_core::RunId::new()).unwrap();
        assert_eq!(next.prior_open_task.as_deref(), Some("t9"));
        assert_eq!(next.answer_task, None);
        // A round that sends keeps answering it.
        let next = AgentStarter::init_continuing(
            &agent,
            message("m2", "yes"),
            &before,
            adam_core::RunId::new(),
        )
        .unwrap();
        assert_eq!(next.prior_open_task, None);
        assert_eq!(next.answer_task.as_deref(), Some("t9"));
    }

    /// A task the round before left working is the one a closing round cancels; the task that
    /// waits for an answer is not swapped for it.
    #[test]
    fn a_closing_round_cancels_the_working_task_before_the_one_that_waits() {
        let agent = round_agent();
        let mut before = AgentStarter::init(&agent, message("m1", "first")).unwrap();
        before.task = Some("t1".into());
        before.answer_task = Some("t9".into());
        let header = SessionHeader {
            id: adam_core::RunId::new(),
            agent: "coder/billing".into(),
            description: String::new(),
            mode: adam_runtime::SessionMode::Open,
            closed: true,
            closing: true,
            round: 2,
            root: None,
            workspace: None,
            visible: false,
            config: None,
        };
        let close = Inbound::new(
            SESSION_CLOSE_KIND,
            json!({ SESSION_STATE_KEY: header, SESSION_CLOSE_ROUND_KEY: true }),
        );
        let next =
            AgentStarter::init_continuing(&agent, close, &before, adam_core::RunId::new()).unwrap();
        assert_eq!(next.prior_open_task.as_deref(), Some("t1"));
        assert_eq!(next.answer_task.as_deref(), Some("t9"));
    }
}
