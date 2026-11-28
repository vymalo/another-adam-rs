//! Child runs: how a finished run tells its parent, and how a parent reads a child.
//!
//! A run started with [`Runtime::start_child`](crate::Runtime::start_child) records its parent. When
//! it reaches a terminal state, the runtime delivers an [`Inbound`] of kind [`RUN_FINISHED_KIND`] to
//! the parent whose payload is a [`ChildStatus`]. That message is a hint that saves the parent a
//! wait: it is sent after the child's commit, so a crash in between loses it, and the parent
//! therefore keeps a timer and reads the child with [`Ctx::child_status`](crate::Ctx::child_status)
//! when the timer fires. See "Child runs" in `docs/architecture.md`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use adam_core::{RunId, RunRecord, RunStatus};

use crate::agent::Inbound;
use crate::runtime::{Runtime, RuntimeError};
use crate::session::{SESSION_CLOSE_KIND, SESSION_EDIT_KIND};

/// [`Inbound::kind`] of the message a finished child sends its parent.
///
/// Its [`Inbound::id`] is the child's run id, so a parent that sees the message twice (the notice is
/// at-least-once) recognises the second by the id, and its payload is a [`ChildStatus`].
pub const RUN_FINISHED_KIND: &str = "adam.run.finished";

/// [`Inbound::kind`] of the message the round of a child *session* sends its parent when it ends,
/// instead of [`RUN_FINISHED_KIND`] (ADR 0045). A child run has a conversation exactly when it is
/// a session round, so the runtime picks the kind by that.
///
/// It has the same id (the round's run id) and payload ([`ChildStatus`]) and the same standing:
/// a hint that wakes the parent, which reads the round from the store (Rule 5). It is a kind of
/// its own because a session reports once per round, not once, and the parent must not read it as
/// the end of a call it is waiting on.
pub const CHILD_REPORTED_KIND: &str = "adam.child.reported";

/// [`Inbound::kind`] of a message that only **wakes** a parked run and says nothing to it: a resume
/// of a run parked on the model ([ADR 0067]), which makes the call it waits to make again. A run
/// that is working or finishing has nothing to read in it, so one that lands during a step neither
/// reopens a finishing run nor counts as an arrival ([`Ctx::arrived`](crate::Ctx::arrived)); one
/// delivered to a parked run wakes it, as any message does. Its payload is not read.
///
/// [ADR 0067]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0067-a-model-call-through-an-injector-names-its-run-and-a-refusal-parks-the-run.md
pub const RESUME_KIND: &str = "adam.resume";

/// [`Inbound::kind`] of the message a child run sends its parent when it **parks** ([ADR 0067]
/// decision 5): it waits for a message, a question's answer or a model that refused it.
///
/// Its [`Inbound::id`] is the child's run id and its payload is not read: it is a hint that the
/// child now waits, and the parent reads what it waits on from the child's record
/// ([`Ctx::child_look`](crate::Ctx::child_look)). The runtime sends it after the child's own
/// commit, like [`RUN_FINISHED_KIND`], and **not** when its own look at a parked child parks it
/// again ([`Runtime::run_worker`](crate::Runtime::run_worker)): nothing changed then.
///
/// [ADR 0067]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0067-a-model-call-through-an-injector-names-its-run-and-a-refusal-parks-the-run.md
pub const CHILD_PARKED_KIND: &str = "adam.child.parked";

/// How many of the messages in `inbox` after its first `base` a run that is finishing has to read:
/// all of them but the [`RUN_FINISHED_KIND`] notices, the [`CHILD_REPORTED_KIND`] ones (a report
/// is read from the store, by a tool or by the agent loop; a turn that a report keeps going is
/// decided from the store at its end, not from a notice), the [`CHILD_PARKED_KIND`] ones (a park
/// is read from the store too, and a run that finishes has no wait left), the [`SESSION_CLOSE_KIND`] requests (a round finishes its work and says so in its header; one that
/// lands while the round finishes stays in its inbox, where a reader of the session finds it), the
/// [`SESSION_EDIT_KIND`] ones (for the same reason: a round that ended before reading its edit
/// leaves it where a reader of the session finds it, and the next round carries it) and the
/// [`RESUME_KIND`] wake-ups (they say nothing). A notice only saves a waiting parent its
/// timer. A run that finishes has no wait left (it settled its waits from the notice or from the
/// store), and the same notice delivered just after its commit is refused, so one that lands during
/// the step must not keep the run going either.
///
/// Neither is a message whose id is in `read`: the run has read one with that id, and the copy a
/// sender delivered again (its first try was not acknowledged) says nothing new. An empty id is
/// nobody's.
pub(crate) fn news_since(inbox: &[Inbound], base: usize, read: &[String]) -> usize {
    inbox
        .iter()
        .skip(base)
        .filter(|i| {
            i.kind != RUN_FINISHED_KIND
                && i.kind != CHILD_REPORTED_KIND
                && i.kind != CHILD_PARKED_KIND
                && i.kind != SESSION_CLOSE_KIND
                && i.kind != SESSION_EDIT_KIND
                && i.kind != RESUME_KIND
                && (i.id.is_empty() || !read.contains(&i.id))
        })
        .count()
}

/// The id of the child a parent starts for `key` (typically a tool call id), the same every time.
///
/// The parent starts its child with [`Runtime::start_child`](crate::Runtime::start_child) under this
/// id, so a step that runs again (a replay after a crash, a lost lease or a transient retry) finds
/// the child it already started instead of starting a second one. The id is a UUID (version 8,
/// "custom") from a SHA-256 over the length-prefixed parent and key, so two parents, or two keys,
/// never share a child.
///
/// ```
/// use adam_core::RunId;
/// use adam_runtime::child_run_id;
///
/// let parent = RunId::new();
/// assert_eq!(child_run_id(parent, "call_1"), child_run_id(parent, "call_1"));
/// assert_ne!(child_run_id(parent, "call_1"), child_run_id(parent, "call_2"));
/// assert_ne!(child_run_id(parent, "call_1"), child_run_id(RunId::new(), "call_1"));
/// ```
pub fn child_run_id(parent: RunId, key: &str) -> RunId {
    let mut hash = Sha256::new();
    for part in [
        b"adam-runtime/child-run/v1".as_slice(),
        parent.0.as_bytes().as_slice(),
        key.as_bytes(),
    ] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part);
    }
    let digest = hash.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    RunId(uuid::Builder::from_custom_bytes(bytes).into_uuid())
}

/// The right of one run to start its own children, as an owned, cloneable handle.
///
/// [`Ctx::child_starter`](crate::Ctx::child_starter) makes one from the runtime that is stepping the
/// run, so code that runs inside a step (a tool of an `LlmAgent`, which cannot hold the `Ctx`) starts
/// children on that very runtime without being given one. It can only start children *of that run*:
/// [`start`](Self::start) is [`Runtime::start_child`] with the parent fixed.
///
/// It is transient: a step gets a new one, and holding on to it past the step keeps the runtime alive.
#[derive(Clone)]
pub struct ChildStarter {
    runtime: Runtime,
    parent: RunId,
    /// The conversation the sessions of this run live in: the run's own, or one made from its id
    /// when it has none.
    conversation: String,
}

impl std::fmt::Debug for ChildStarter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildStarter")
            .field("parent", &self.parent)
            .finish_non_exhaustive()
    }
}

impl ChildStarter {
    pub(crate) fn new(runtime: Runtime, parent: RunId, conversation: Option<&str>) -> Self {
        // A timer run of a session speaks for the conversation it was made unique from.
        let conversation = conversation.map_or_else(
            || format!("run:{parent}"),
            |c| crate::session::parse_timer_conversation_id(c).unwrap_or_else(|| c.to_owned()),
        );
        Self {
            runtime,
            parent,
            conversation,
        }
    }

    /// The run whose children this starts.
    pub fn parent(&self) -> RunId {
        self.parent
    }

    /// Start a run of `agent` as a child of [`parent`](Self::parent) under `id`: exactly
    /// [`Runtime::start_child`], so it is idempotent (`false` when the id exists) and the agent must
    /// be registered on this runtime, as an agent or as a starter.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::UnknownAgent`] for an agent this runtime does not know, and the store's
    /// errors.
    pub async fn start(
        &self,
        id: RunId,
        agent: &str,
        input: Inbound,
    ) -> Result<bool, RuntimeError> {
        self.runtime
            .start_child(self.parent, id, agent, input)
            .await
    }

    /// [`start`](Self::start) with a time limit: [`Runtime::start_child_within`]. `None` is no limit.
    pub async fn start_within(
        &self,
        id: RunId,
        agent: &str,
        input: Inbound,
        within: Option<std::time::Duration>,
    ) -> Result<bool, RuntimeError> {
        self.runtime
            .start_child_within(self.parent, id, agent, input, within)
            .await
    }
}

/// The sessions of a run's conversation (ADR 0045): each method reads or acts on a session of the
/// conversation of [`parent`](ChildStarter::parent) and on nothing else, so a tool cannot reach the
/// rounds of another conversation, and a session of the conversation can be reached by a later
/// run of it.
impl ChildStarter {
    /// The conversation the sessions of this run belong to: the run's own conversation id, or, for
    /// a run that has none, `run:<run id>`, so that such a run still has sessions of its own.
    pub fn session_conversation(&self) -> &str {
        &self.conversation
    }

    /// Start a round of `session` as a child of [`parent`](Self::parent) under `id`:
    /// [`Runtime::start_child_continuing_within`] with the session's conversation id.
    ///
    /// # Errors
    ///
    /// As [`Runtime::start_child_continuing_within`].
    pub async fn start_round(
        &self,
        session: RunId,
        id: RunId,
        agent: &str,
        input: Inbound,
        prior: Option<RunId>,
        within: Option<std::time::Duration>,
    ) -> Result<bool, RuntimeError> {
        let conversation = crate::session::session_conversation_id(&self.conversation, session);
        self.runtime
            .start_child_continuing_within(
                self.parent,
                id,
                agent,
                input,
                &conversation,
                prior,
                within,
            )
            .await
    }

    /// How long a kept-open session may sit idle before it reads closed: the runtime's
    /// [`session_idle_ttl`](Runtime::session_idle_ttl).
    pub fn session_idle_ttl(&self) -> std::time::Duration {
        self.runtime.session_idle_ttl()
    }

    /// Start a run of `agent` as a child of [`parent`](Self::parent) under `id`, in the conversation
    /// the sessions of this run belong to, from a round of one of them: the round's conversation is
    /// the session's, and the new run's is its parent's, made unique by its id: a conversation holds
    /// one open run per agent, and a timer that is parked until the session may have expired must
    /// not keep the timer of another round or session from being set. A timer run reads the
    /// session with [`session`](Self::session) when it wakes (its own
    /// [`Ctx::child_starter`](crate::Ctx::child_starter) speaks for the parent conversation).
    /// Idempotent, as [`Runtime::start_child`].
    ///
    /// # Errors
    ///
    /// [`RuntimeError::InvalidSession`] when this run is not a round of a session, and the errors
    /// of [`Runtime::start_child_in`].
    pub async fn start_in_parent_conversation(
        &self,
        id: RunId,
        agent: &str,
        input: Inbound,
    ) -> Result<bool, RuntimeError> {
        let Some((parent, _)) = crate::session::parse_session_conversation_id(&self.conversation)
        else {
            return Err(RuntimeError::InvalidSession {
                reason: "this run is not a round of a session".into(),
            });
        };
        let conversation = crate::session::timer_conversation_id(&parent, id);
        self.runtime
            .start_child_in(self.parent, id, agent, input, &conversation)
            .await
    }

    /// The session `session` of this conversation: [`Runtime::session_view`].
    ///
    /// # Errors
    ///
    /// As [`Runtime::session_view`].
    pub async fn session(
        &self,
        session: RunId,
    ) -> Result<Option<crate::SessionView>, RuntimeError> {
        self.runtime.session_view(&self.conversation, session).await
    }

    /// The sessions of this conversation: [`Runtime::list_sessions`].
    ///
    /// # Errors
    ///
    /// As [`Runtime::list_sessions`].
    pub async fn sessions(
        &self,
        agents: &[&str],
        offset: usize,
        limit: usize,
    ) -> Result<crate::SessionList, RuntimeError> {
        self.runtime
            .list_sessions(&self.conversation, agents, offset, limit)
            .await
    }

    /// Deliver `inbound` to the latest round of `session`, which a working round reads at its next
    /// step ([`Runtime::deliver`]).
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Finished`] when the round has ended meanwhile, and the errors of
    /// [`Runtime::deliver`].
    pub async fn steer(
        &self,
        session: &crate::SessionView,
        inbound: Inbound,
    ) -> Result<(), RuntimeError> {
        self.runtime.deliver(session.latest_run, inbound).await
    }

    /// Cancel the latest round of `session` ([`Runtime::cancel`]): a no-op for a round that has
    /// ended.
    ///
    /// # Errors
    ///
    /// As [`Runtime::cancel`].
    pub async fn cancel_round(
        &self,
        session: &crate::SessionView,
        reason: &str,
    ) -> Result<(), RuntimeError> {
        self.runtime.cancel(session.latest_run, reason).await
    }

    /// Delete a session whose latest round is working ([`Runtime::delete_session_round`]): the
    /// round is cancelled and the session's tombstone is written in the same commit, so no listing
    /// and no report sees the round between the two (ADR 0054 decision 10, amended).
    ///
    /// The tombstone belongs to the latest round, so the session is read again first: a view taken
    /// before another round started would hide nothing, and the caller gets
    /// [`RuntimeError::Contended`] (class `Conflict`) to look at the session as it is now.
    ///
    /// # Errors
    ///
    /// As [`Runtime::delete_session_round`], and [`RuntimeError::Contended`] when the session's
    /// latest round changed since `session` was read.
    pub async fn delete_round(
        &self,
        session: &crate::SessionView,
        reason: &str,
    ) -> Result<(), RuntimeError> {
        match self.session(session.id).await? {
            Some(now) if now.latest_run == session.latest_run => {
                self.runtime.delete_session_round(session, reason).await
            }
            _ => Err(RuntimeError::Contended(format!(
                "session {} changed while it was deleted",
                session.id
            ))),
        }
    }

    /// Cancel the latest round of a session of this conversation's listing ([`Runtime::cancel`]).
    ///
    /// # Errors
    ///
    /// As [`Runtime::cancel`].
    pub async fn cancel_listed(
        &self,
        session: &crate::SessionSummary,
        reason: &str,
    ) -> Result<(), RuntimeError> {
        self.runtime.cancel(session.latest_run, reason).await
    }

    /// The agent state a round of a session of this conversation last committed (what its agent
    /// serialized): for a parent that keeps what a round kept, the tokens its model calls took.
    /// `None` when the run is not there; **not journaled**, a live read like
    /// [`Ctx::child_state`](crate::Ctx::child_state), which answers for children of the run only
    /// where this answers for the rounds of its conversation.
    ///
    /// A v5 round's state is its head and the body generation it names, so the round's agent must
    /// be registered on this runtime — as an agent or a starter — for its `join` to rebuild the
    /// state (ADR 0055).
    ///
    /// # Errors
    ///
    /// [`RuntimeError::InvalidSession`] when `round` is not a round of a session of this
    /// conversation, [`RuntimeError::Corrupt`] when its record does not decode, and the store's
    /// errors.
    pub async fn round_state(
        &self,
        round: RunId,
    ) -> Result<Option<serde_json::Value>, RuntimeError> {
        Ok(self.round_record(round).await?.map(|(_, state)| state))
    }

    /// [`round_state`](Self::round_state) with the round's status: what a parent needs to count the
    /// tokens of a round only once it has ended (`Done` or `Failed`, a cancel included). Not journaled.
    ///
    /// # Errors
    ///
    /// As [`round_state`](Self::round_state).
    pub async fn round_record(
        &self,
        round: RunId,
    ) -> Result<Option<(adam_core::RunStatus, serde_json::Value)>, RuntimeError> {
        let Some(rec) = self.runtime.store().load_run(round).await? else {
            return Ok(None);
        };
        let ours = rec
            .conversation_id
            .as_deref()
            .and_then(|c| crate::session::session_of(&self.conversation, c))
            .is_some();
        if !ours {
            return Err(RuntimeError::InvalidSession {
                reason: format!("run {round} is not a round of a session of this conversation"),
            });
        }
        Ok(Some((
            rec.status,
            self.runtime.inner.claimed(&rec).await?.env.agent,
        )))
    }
}

/// Where a child run stands, as far as its parent needs to know.
///
/// It is the payload of the [`RUN_FINISHED_KIND`] message (`{"status": "done", "output": ...}` or
/// `{"status": "failed", "error": "..."}`) and what [`Ctx::child_status`](crate::Ctx::child_status)
/// reads back, so both routes to the answer give the same thing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChildStatus {
    /// The child's status. `Runnable` and `Parked` mean it is still working.
    pub status: RunStatus,
    /// The child's result once it is `Done`. A `null` output reads as absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    /// The child's reason once it is `Failed` (a cancel reads `cancelled: <reason>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ChildStatus {
    /// Whether the child is `Done` or `Failed`, so nothing more will change.
    pub fn is_finished(&self) -> bool {
        self.status.is_terminal()
    }

    /// The status of the child that is not there any more (purged after it finished).
    pub fn vanished() -> Self {
        Self {
            status: RunStatus::Failed,
            output: None,
            error: Some("the child run no longer exists".to_owned()),
        }
    }

    /// Read a child's record. Never fails: a finished run whose state cannot be read is reported as
    /// failed, so a parent is not left waiting on something that will never change.
    pub(crate) fn from_record(rec: &RunRecord) -> Self {
        if rec.status.is_open() {
            return Self {
                status: rec.status,
                output: None,
                error: None,
            };
        }
        match crate::head::Stored::decode(rec.id, &rec.state) {
            Ok(stored) => Self::from_parts(rec.status, stored.output(), stored.error()),
            Err(e) => Self {
                status: RunStatus::Failed,
                output: None,
                error: Some(format!("the run's state is unreadable: {e}")),
            },
        }
    }

    /// A child whose record did not verify (ADR 0068): `Failed`, with the reason and none of what
    /// the record says.
    pub(crate) fn refused(e: &RuntimeError) -> Self {
        Self {
            status: RunStatus::Failed,
            output: None,
            error: Some(format!("the child run's record is refused: {e}")),
        }
    }

    /// The status a commit this process just made reports, from the values it wrote: a store
    /// returns the record without its state ([`Store::commit_run`](adam_core::Store::commit_run)),
    /// so the caller reports from what it holds instead of decoding what it just sent.
    pub(crate) fn from_parts(status: RunStatus, output: &Value, error: Option<&str>) -> Self {
        match status {
            RunStatus::Done => Self {
                status,
                output: (!output.is_null()).then(|| output.clone()),
                error: None,
            },
            RunStatus::Failed => Self {
                status,
                output: None,
                error: Some(error.unwrap_or("the run failed").to_owned()),
            },
            RunStatus::Runnable | RunStatus::Parked => Self {
                status,
                output: None,
                error: None,
            },
        }
    }

    /// The message that tells the parent of `child` about this status.
    pub(crate) fn notice(&self, child: RunId, kind: &str) -> Inbound {
        let payload = serde_json::to_value(self).unwrap_or(Value::Null);
        Inbound::new(kind, payload).with_id(child.to_string())
    }

    /// Read a [`RUN_FINISHED_KIND`] message: the child it is about and its final status. `None`
    /// when `inbound` is of another kind, its id is not a run id, its payload is not a
    /// [`ChildStatus`] or the status it reports is not final.
    ///
    /// ```
    /// use adam_core::RunId;
    /// use adam_runtime::{ChildStatus, Inbound, RUN_FINISHED_KIND};
    /// use serde_json::json;
    ///
    /// let child = RunId::new();
    /// let notice = Inbound::new(RUN_FINISHED_KIND, json!({"status": "done", "output": "42"}))
    ///     .with_id(child.to_string());
    /// let (run, status) = ChildStatus::from_notice(&notice).expect("a finished notice");
    /// assert_eq!(run, child);
    /// assert_eq!(status.output, Some(json!("42")));
    ///
    /// assert!(ChildStatus::from_notice(&Inbound::new("message", json!({}))).is_none());
    /// ```
    pub fn from_notice(inbound: &Inbound) -> Option<(RunId, Self)> {
        Self::from_message(inbound, RUN_FINISHED_KIND)
    }

    /// [`from_notice`](Self::from_notice) for a [`CHILD_REPORTED_KIND`] message: the round it is
    /// about and its final status.
    pub fn from_report(inbound: &Inbound) -> Option<(RunId, Self)> {
        Self::from_message(inbound, CHILD_REPORTED_KIND)
    }

    fn from_message(inbound: &Inbound, kind: &str) -> Option<(RunId, Self)> {
        if inbound.kind != kind {
            return None;
        }
        let run = Uuid::parse_str(&inbound.id).ok().map(RunId)?;
        let status: Self = serde_json::from_value(inbound.payload.clone()).ok()?;
        status.is_finished().then_some((run, status))
    }
}

/// A child run as its parent reads it in **one** read ([ADR 0067] decision 5): the status and
/// result a notice carries, and the head of the child's state, so that a parent sees what its
/// child waits on without reading the child's history.
///
/// [`Ctx::child_look`](crate::Ctx::child_look) answers it for the calling run's own children. The
/// head is the child's agent state without its transcript for a v5 run; `None` when the record does
/// not decode, in which case the status says so for a terminal child, as a [`ChildStatus`] read from
/// the same record would.
///
/// [ADR 0067]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0067-a-model-call-through-an-injector-names-its-run-and-a-refusal-parks-the-run.md
#[derive(Clone, Debug, PartialEq)]
pub struct ChildLook {
    /// The child's status and result.
    pub status: ChildStatus,
    /// The head of the child's state: its agent state without the transcript for a v5 run.
    pub head: Option<Value>,
}

impl ChildLook {
    /// A child known only by its final status (a notice the inbox already holds).
    pub fn finished(status: ChildStatus) -> Self {
        Self { status, head: None }
    }

    /// Read a child's record: one decode of its state gives the status, the result and the head.
    pub(crate) fn from_record(rec: &RunRecord) -> Self {
        match crate::head::Stored::decode(rec.id, &rec.state) {
            Ok(stored) => {
                let status = if rec.status.is_open() {
                    ChildStatus {
                        status: rec.status,
                        output: None,
                        error: None,
                    }
                } else {
                    ChildStatus::from_parts(rec.status, stored.output(), stored.error())
                };
                Self {
                    status,
                    head: Some(stored.agent().clone()),
                }
            }
            Err(e) => Self {
                status: if rec.status.is_open() {
                    ChildStatus {
                        status: rec.status,
                        output: None,
                        error: None,
                    }
                } else {
                    ChildStatus {
                        status: RunStatus::Failed,
                        output: None,
                        error: Some(format!("the run's state is unreadable: {e}")),
                    }
                },
                head: None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// What a finishing run has to read: messages, not the wake-ups and notices that say nothing to
    /// it (a resume, ADR 0067, among them), nor a copy of one it read.
    #[test]
    fn only_news_counts_and_a_resume_is_none() {
        let inbox = vec![
            Inbound::new("message", json!({"text": "old"})).with_id("m-0"),
            Inbound::new(RESUME_KIND, json!({})).with_id("r-1"),
            Inbound::new(RUN_FINISHED_KIND, json!({})).with_id("c-1"),
            Inbound::new(CHILD_REPORTED_KIND, json!({})).with_id("c-2"),
            Inbound::new(CHILD_PARKED_KIND, json!({})).with_id("c-3"),
            Inbound::new(SESSION_CLOSE_KIND, json!({})).with_id("s-1"),
            Inbound::new("message", json!({"text": "read"})).with_id("m-1"),
        ];
        assert_eq!(news_since(&inbox, 1, &["m-1".to_owned()]), 0);
        assert_eq!(news_since(&inbox, 1, &[]), 1);
        assert_eq!(news_since(&inbox, 0, &[]), 2);
    }

    #[test]
    fn a_child_id_depends_on_both_inputs_and_nothing_else() {
        let (a, b) = (RunId::new(), RunId::new());
        assert_eq!(child_run_id(a, "x"), child_run_id(a, "x"));
        assert_ne!(child_run_id(a, "x"), child_run_id(a, "y"));
        assert_ne!(child_run_id(a, "x"), child_run_id(b, "x"));
        // A journal relies on the derivation, so it is pinned: this id must never change.
        let fixed = RunId(Uuid::nil());
        assert_eq!(
            child_run_id(fixed, "call_1").to_string(),
            "9a76d7bc-1e62-8362-9521-a05f6a837e5b"
        );
        assert_eq!(child_run_id(fixed, "call_1").0.get_version_num(), 8);
    }

    #[test]
    fn the_payload_has_the_documented_shape() {
        let done = ChildStatus {
            status: RunStatus::Done,
            output: Some(json!({"text": "hi"})),
            error: None,
        };
        assert_eq!(
            serde_json::to_value(&done).unwrap(),
            json!({"status": "done", "output": {"text": "hi"}})
        );
        let failed = ChildStatus {
            status: RunStatus::Failed,
            output: None,
            error: Some("boom".into()),
        };
        assert_eq!(
            serde_json::to_value(&failed).unwrap(),
            json!({"status": "failed", "error": "boom"})
        );
    }

    #[test]
    fn a_notice_round_trips_and_carries_the_child_id() {
        let child = RunId::new();
        let status = ChildStatus {
            status: RunStatus::Failed,
            output: None,
            error: Some("boom".into()),
        };
        let notice = status.notice(child, RUN_FINISHED_KIND);
        assert_eq!(notice.kind, RUN_FINISHED_KIND);
        assert_eq!(notice.id, child.to_string());
        assert_eq!(
            ChildStatus::from_notice(&notice),
            Some((child, status.clone()))
        );
        assert!(ChildStatus::from_report(&notice).is_none());

        let report = status.notice(child, CHILD_REPORTED_KIND);
        assert_eq!(report.kind, CHILD_REPORTED_KIND);
        assert_eq!(report.payload, notice.payload, "same payload, other kind");
        assert_eq!(ChildStatus::from_report(&report), Some((child, status)));
        assert!(ChildStatus::from_notice(&report).is_none());
    }

    #[test]
    fn what_is_not_a_finished_notice_is_refused() {
        let child = RunId::new().to_string();
        let make = |kind: &str, id: &str, payload| Inbound::new(kind, payload).with_id(id);
        // Another kind, an id that is not a run, garbage, and a status that is not final.
        assert!(
            ChildStatus::from_notice(&make("message", &child, json!({"status": "done"}))).is_none()
        );
        assert!(
            ChildStatus::from_notice(&make(RUN_FINISHED_KIND, "nope", json!({"status": "done"})))
                .is_none()
        );
        assert!(
            ChildStatus::from_notice(&make(RUN_FINISHED_KIND, &child, json!("done"))).is_none()
        );
        assert!(
            ChildStatus::from_notice(&make(
                RUN_FINISHED_KIND,
                &child,
                json!({"status": "parked"})
            ))
            .is_none()
        );
    }
}
