//! The events of `child-sessions/v1` on a parent task's stream (contract section 3).
//!
//! A subscriber that activated the extension is told when a session of its parent context changes:
//! `opened`, `round`, `reported`, `closed`. The runtime has no events for it, so the subscription
//! lists the sessions when it polls — the fallback poll, or a `Status` event of the task it
//! streams — once [`SESSIONS_POLL`] has passed since the last listing, and turns what changed since
//! then into events ([`diff`], a pure function). An event is a latency optimisation: the sessions
//! object of the task is the truth, so a listing that failed is retried at the next poll and says
//! nothing.

use std::collections::HashMap;
use std::time::Duration;

use a2a::{TaskState, TaskStatus, TaskStatusUpdateEvent};
use adam_a2a::{CHILD_SESSIONS_EXTENSION, Caller, TaskEvent};
use adam_core::RunId;
use adam_runtime::{RoundOutcome, SessionState, SessionSummary, session_conversation_id};
use serde_json::{Value, json};
use tokio::time::Instant;

use crate::backend::RuntimeTaskBackend;
use crate::child_sessions::{a2a_state_word, wire_state};
use crate::task_state;

/// The longest a subscription's sessions listing is allowed to be stale **when the subscription
/// polls**: the tracker asks to be listed this often, and the subscription asks only on its own
/// poll — the fallback ([`RuntimeTaskBackend::with_poll_interval`](crate::RuntimeTaskBackend::with_poll_interval),
/// 2 s by default) or a `Status` event of the task it streams. The real bound is therefore
/// `max(SESSIONS_POLL, poll)`: 2 s with the defaults, not 1 s.
///
/// That is deliberate. A listing arm of its own every second would pay a whole `list_sessions`
/// (two counts and a page, plus a view per changed open round) per subscriber per second, for a
/// change nobody is blocked on; the fallback poll already bounds it.
pub(crate) const SESSIONS_POLL: Duration = Duration::from_secs(1);

/// The least time between two listings when the parent's record changed: a busy parent commits far
/// more often than a client needs to hear of its sessions, and each listing pages the sessions
/// and reads a view per open one.
pub(crate) const SESSIONS_MIN_INTERVAL: Duration = Duration::from_millis(200);

/// Whether a subscription reads its sessions again, when it polls: [`SESSIONS_POLL`] after the
/// last read, or [`SESSIONS_MIN_INTERVAL`] after it when the parent's record changed. A read that
/// failed waits the whole [`SESSIONS_POLL`], whatever changed.
///
/// The subscription asks this only on a poll (`subscribe::pump`), so the effective period is
/// `max(SESSIONS_POLL, poll)` — 2 s with the default fallback — unless the parent's record changes
/// first, which polls the pump at once.
pub(crate) fn read_due(changed: bool, failed: bool, since_last: Duration) -> bool {
    since_last >= SESSIONS_POLL || (changed && !failed && since_last >= SESSIONS_MIN_INTERVAL)
}

/// `status` as an event of a session says it: the state, with no message (the message was the
/// task's, for the moment it was sent).
pub(crate) fn quiet(status: &TaskStatus) -> TaskStatus {
    TaskStatus {
        message: None,
        ..status.clone()
    }
}

/// Who acted: the parent's model, or a person.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum By {
    Agent,
    Person,
}

impl By {
    fn word(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Person => "person",
        }
    }
}

/// How a round ended, as `reported` says it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Report {
    Done,
    Failed,
    Canceled,
    /// The round waits for the person (`input-required`).
    Blocked,
}

impl Report {
    fn word(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
            Self::Blocked => "blocked",
        }
    }
}

/// One event of a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    Opened,
    Round,
    Reported(Report),
    Closed,
}

/// Where a session's latest round is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RoundState {
    Working,
    /// Waiting for the person.
    Blocked,
    Done,
    Failed,
    Canceled,
}

impl RoundState {
    /// The report of a round that is not working, `None` while it works.
    fn report(self) -> Option<Report> {
        match self {
            Self::Working => None,
            Self::Blocked => Some(Report::Blocked),
            Self::Done => Some(Report::Done),
            Self::Failed => Some(Report::Failed),
            Self::Canceled => Some(Report::Canceled),
        }
    }

    /// The round task's A2A state.
    fn task_state(self) -> TaskState {
        match self {
            Self::Working => TaskState::Working,
            Self::Blocked => TaskState::InputRequired,
            Self::Done => TaskState::Completed,
            Self::Failed => TaskState::Failed,
            Self::Canceled => TaskState::Canceled,
        }
    }

    fn of_task(state: &TaskState) -> Self {
        match state {
            TaskState::InputRequired | TaskState::AuthRequired => Self::Blocked,
            TaskState::Completed => Self::Done,
            TaskState::Failed | TaskState::Rejected => Self::Failed,
            TaskState::Canceled => Self::Canceled,
            TaskState::Unspecified | TaskState::Submitted | TaskState::Working => Self::Working,
        }
    }

    /// How a round that is not open ended; `None` when the listing did not say (the round is not
    /// readable yet, and nothing is reported of it until it is).
    fn of_outcome(outcome: Option<RoundOutcome>) -> Option<Self> {
        match outcome? {
            RoundOutcome::Done => Some(Self::Done),
            RoundOutcome::Cancelled => Some(Self::Canceled),
            RoundOutcome::Failed => Some(Self::Failed),
        }
    }
}

/// A session as the last listing saw it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Seen {
    pub session: RunId,
    pub context: String,
    pub agent: String,
    pub description: String,
    pub keep_open: bool,
    pub by: By,
    pub round: u32,
    pub task: RunId,
    pub state: RoundState,
    /// The latest round only closes the session: it reports nothing.
    pub closing_round: bool,
    /// Closed on the wire (see `wire_state`).
    pub closed: bool,
}

impl Seen {
    fn event_object(&self, event: Event) -> Value {
        let round = json!({
            "number": self.round,
            "taskId": self.task,
            "state": a2a_state_word(&self.state.task_state()),
        });
        let mut object = json!({
            "session": self.session,
            "contextId": self.context,
            "agent": self.agent,
            "description": self.description,
            "keepOpen": self.keep_open,
            "by": self.by.word(),
        });
        let (name, round, report) = match event {
            Event::Opened => ("opened", Some(round), None),
            Event::Round => ("round", Some(round), None),
            Event::Reported(report) => ("reported", Some(round), Some(report)),
            Event::Closed => ("closed", None, None),
        };
        object["event"] = json!(name);
        if let Some(round) = round {
            object["round"] = round;
        }
        if let Some(report) = report {
            object["report"] = json!(report.word());
        }
        object
    }
}

/// The events that take a session from what was seen of it (`None`: not seen before) to `now`, in
/// the contract's order: `opened`, `round`, `reported`, `closed`. A step that was missed (a session
/// first seen with its round already over) is filled in, in that order.
///
/// A round that only closes the session reports nothing: `closed` is its only event.
pub(crate) fn diff(prev: Option<&Seen>, now: &Seen) -> Vec<Event> {
    let mut out = Vec::new();
    let new_round = match prev {
        None => {
            out.push(Event::Opened);
            true
        }
        Some(prev) if now.round > prev.round => {
            if !now.closing_round {
                out.push(Event::Round);
            }
            true
        }
        Some(_) => false,
    };
    if !now.closing_round
        && let Some(report) = now.state.report()
        && (new_round || prev.is_some_and(|p| p.state != now.state))
    {
        out.push(Event::Reported(report));
    }
    if now.closed && !prev.is_some_and(|p| p.closed) {
        out.push(Event::Closed);
    }
    out
}

/// What the tracker knows of one session: the listing row its `Seen` was read from, and that
/// `Seen`.
///
/// The whole row is the key because it is the only "version" a listing gives, and every member of
/// a `Seen` derives from it: the record's metadata (a commit bumps `updated_at` and moves
/// `latest_run` or `round`) and what the listing decides from the record and its clock (`state`,
/// `closed`, `idle_expired`). A later listing that returns the same row would build the same
/// `Seen`, so its `view` is skipped; anything that changed — a commit, a new round, the idle TTL
/// closing the session — changes the row and is read again.
#[derive(Clone)]
struct Cached {
    summary: SessionSummary,
    seen: Seen,
}

/// What the stream of a parent task knows of its sessions.
pub(crate) struct SessionsTracker {
    known: HashMap<RunId, Cached>,
    listed_at: Instant,
    version: u64,
    /// The last listing failed: the next waits a whole interval.
    failed: bool,
}

impl SessionsTracker {
    /// The sessions of `context` as they are now: nothing is said of them. `None` when they could
    /// not be listed.
    pub(crate) async fn seed(
        backend: &RuntimeTaskBackend,
        caller: &Caller,
        context: &str,
        version: u64,
    ) -> Option<Self> {
        let known = backend
            .sessions_seen(caller, context, &HashMap::new())
            .await?;
        Some(Self {
            known: known.into_iter().map(|c| (c.seen.session, c)).collect(),
            listed_at: Instant::now(),
            version,
            failed: false,
        })
    }

    /// Whether the sessions are worth listing again.
    pub(crate) fn due(&self, version: u64) -> bool {
        read_due(
            version != self.version,
            self.failed,
            self.listed_at.elapsed(),
        )
    }

    /// List the sessions and say what changed, each event a status update of the parent task that
    /// carries `status` (the task's state as the subscriber last read it).
    pub(crate) async fn refresh(
        &mut self,
        backend: &RuntimeTaskBackend,
        caller: &Caller,
        task_id: &str,
        context_id: &str,
        version: u64,
        status: &TaskStatus,
    ) -> Vec<TaskEvent> {
        // Noted before the read, so a read that fails waits for the next interval.
        self.listed_at = Instant::now();
        self.version = version;
        let Some(now) = backend.sessions_seen(caller, context_id, &self.known).await else {
            self.failed = true;
            return Vec::new();
        };
        self.failed = false;
        let status = &quiet(status);
        let mut events = Vec::new();
        for now in now {
            let prev = self.known.get(&now.seen.session).map(|c| c.seen.clone());
            // A round that was working and is gone from view (a newer one started) ended unseen:
            // read how, so its report is not lost.
            let mut chain = Vec::new();
            if let Some(prev) = &prev
                && ended_unseen(prev, &now.seen)
                && let Some(settled) = backend.settled(prev).await
            {
                chain.push(settled);
            }
            chain.push(now.seen.clone());
            let mut before = prev;
            for step in &chain {
                for event in diff(before.as_ref(), step) {
                    events.push(TaskEvent::Status(TaskStatusUpdateEvent {
                        task_id: task_id.to_owned(),
                        context_id: context_id.to_owned(),
                        status: status.clone(),
                        metadata: Some(HashMap::from([(
                            CHILD_SESSIONS_EXTENSION.to_owned(),
                            step.event_object(event),
                        )])),
                    }));
                }
                before = Some(step.clone());
            }
            self.known.insert(now.seen.session, now);
        }
        events
    }
}

impl RuntimeTaskBackend {
    /// The sessions of parent context `context` as [`Cached`]; `None` when they could not be
    /// listed (a warning, at most one per interval: the caller backs off). A session whose latest
    /// round is not readable is left out, so nothing is said of it yet.
    ///
    /// `known` is what the last listing read: a session whose listing row is unchanged is reused
    /// without a [`view`](adam_runtime::Runtime::view), which is the read this exists to save (an
    /// open round's view is two queries and a full state read).
    ///
    /// The listing is paged and the sessions can move between pages while it is read, so a session
    /// may be missed by one listing and told `opened` by a later one.
    async fn sessions_seen(
        &self,
        caller: &Caller,
        context: &str,
        known: &HashMap<RunId, Cached>,
    ) -> Option<Vec<Cached>> {
        let listed = match self.listed_sessions(caller, context).await {
            Ok(listed) => listed,
            Err(e) => {
                tracing::warn!(error = %e, "the sessions of a context could not be listed");
                return None;
            }
        };
        let mut out = Vec::with_capacity(listed.len());
        for s in &listed {
            if let Some(cached) = known.get(&s.id)
                && cached.summary == *s
            {
                out.push(cached.clone());
                continue;
            }
            if let Some(seen) = self.seen_of(context, s).await {
                out.push(Cached {
                    summary: s.clone(),
                    seen,
                });
            }
        }
        Some(out)
    }

    async fn seen_of(&self, context: &str, s: &SessionSummary) -> Option<Seen> {
        let state = match s.state {
            // Only an open round can wait for the person, and only its view says so. A view that
            // cannot be read leaves the session out of this listing rather than guessing `working`:
            // a guessed `Seen` would be cached under the same row, and a parked round never commits,
            // so its `blocked` report would be lost until something else changed. A session with no
            // cache entry is always read again.
            SessionState::Working => {
                let view = self.runtime.view(s.latest_run).await.ok()??;
                RoundState::of_task(&task_state(&view))
            }
            SessionState::Idle | SessionState::Closed => RoundState::of_outcome(s.outcome)?,
        };
        Some(Seen {
            session: s.id,
            context: session_conversation_id(context, s.id),
            agent: self.session_agent_name(&s.agent),
            description: s.description.clone(),
            keep_open: s.keep_open,
            by: if s.started_outside_a_run() {
                By::Person
            } else {
                By::Agent
            },
            round: s.round,
            task: s.latest_run,
            state,
            closing_round: s.closing,
            closed: wire_state(s.state, s.closes()) == "closed",
        })
    }

    /// How `prev`'s round ended, read from its run: `prev` as it stood when that round was over.
    async fn settled(&self, prev: &Seen) -> Option<Seen> {
        let view = self.runtime.view(prev.task).await.ok()??;
        settle(prev, RoundState::of_task(&task_state(&view)))
    }
}

/// Whether the round `prev` saw may have ended before `now` was listed: a newer round started
/// while it worked or waited for the person.
fn ended_unseen(prev: &Seen, now: &Seen) -> bool {
    now.round > prev.round && prev.state.report().is_none_or(|r| r == Report::Blocked)
}

/// `prev` as it stood when its round ended in `state`; `None` while that round still works or
/// waits.
fn settle(prev: &Seen, state: RoundState) -> Option<Seen> {
    if state.report().is_none() || state == RoundState::Blocked {
        return None;
    }
    let mut settled = prev.clone();
    settled.state = state;
    // A session that is not kept open closes with its round.
    settled.closed = prev.closed || !prev.keep_open;
    Some(settled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(round: u32, state: RoundState, closed: bool) -> Seen {
        Seen {
            session: RunId::new(),
            context: "c/s/x".into(),
            agent: "planner".into(),
            description: "d".into(),
            keep_open: true,
            by: By::Agent,
            round,
            task: RunId::new(),
            state,
            closing_round: false,
            closed,
        }
    }

    use Event::{Closed, Opened, Reported, Round};
    use RoundState::{Blocked, Canceled, Done, Failed, Working};

    #[test]
    fn the_diff_of_two_listings_is_the_events_in_the_contracts_order() {
        let closing = Seen {
            closing_round: true,
            ..seen(3, Working, true)
        };
        let closing_done = Seen {
            closing_round: true,
            ..seen(3, Done, true)
        };
        #[allow(clippy::type_complexity)]
        let table: Vec<(&str, Option<Seen>, Seen, Vec<Event>)> = vec![
            ("new, working", None, seen(1, Working, false), vec![Opened]),
            (
                "new, already over (missed steps filled in order)",
                None,
                seen(1, Done, true),
                vec![Opened, Reported(Report::Done), Closed],
            ),
            (
                "new, failed and kept open",
                None,
                seen(1, Failed, false),
                vec![Opened, Reported(Report::Failed)],
            ),
            (
                "new, waiting for the person",
                None,
                seen(1, Blocked, false),
                vec![Opened, Reported(Report::Blocked)],
            ),
            (
                "unchanged",
                Some(seen(1, Working, false)),
                seen(1, Working, false),
                vec![],
            ),
            (
                "the round ended",
                Some(seen(1, Working, false)),
                seen(1, Done, false),
                vec![Reported(Report::Done)],
            ),
            (
                "the round was cancelled and the session closed",
                Some(seen(1, Working, false)),
                seen(1, Canceled, true),
                vec![Reported(Report::Canceled), Closed],
            ),
            (
                "the round asks the person, then ends",
                Some(seen(1, Working, false)),
                seen(1, Blocked, false),
                vec![Reported(Report::Blocked)],
            ),
            (
                "answered and ended",
                Some(seen(1, Blocked, false)),
                seen(1, Done, false),
                vec![Reported(Report::Done)],
            ),
            (
                "answered and working again",
                Some(seen(1, Blocked, false)),
                seen(1, Working, false),
                vec![],
            ),
            (
                "a closed session stays closed",
                Some(seen(1, Done, true)),
                seen(1, Done, true),
                vec![],
            ),
            (
                "a later round starts",
                Some(seen(1, Done, false)),
                seen(2, Working, false),
                vec![Round],
            ),
            (
                "a later round starts and ends unseen",
                Some(seen(1, Done, false)),
                seen(2, Failed, false),
                vec![Round, Reported(Report::Failed)],
            ),
            (
                "two rounds at once are one event",
                Some(seen(1, Done, false)),
                seen(3, Working, false),
                vec![Round],
            ),
            (
                "a close asked of an idle session: a round that reports nothing",
                Some(seen(2, Done, false)),
                closing.clone(),
                vec![Closed],
            ),
            (
                "the closing round ends: nothing more",
                Some(closing),
                closing_done,
                vec![],
            ),
        ];
        for (name, prev, now, want) in table {
            assert_eq!(diff(prev.as_ref(), &now), want, "{name}");
        }
    }

    #[test]
    fn an_event_says_exactly_what_the_contract_lists() {
        let mut now = seen(2, Done, false);
        now.by = By::Person;
        let object = now.event_object(Event::Reported(Report::Done));
        let mut keys: Vec<&str> = object
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "agent",
                "by",
                "contextId",
                "description",
                "event",
                "keepOpen",
                "report",
                "round",
                "session"
            ]
        );
        assert_eq!(object["round"]["state"], "TASK_STATE_COMPLETED");
        assert_eq!(object["by"], "person");
        let closed = now.event_object(Event::Closed);
        assert!(closed.get("round").is_none() && closed.get("report").is_none());
        let opened = now.event_object(Event::Opened);
        assert!(opened.get("round").is_some() && opened.get("report").is_none());
    }

    #[test]
    fn a_round_state_maps_to_its_report_and_its_task_state() {
        for (state, report, task) in [
            (Working, None, TaskState::Working),
            (Blocked, Some(Report::Blocked), TaskState::InputRequired),
            (Done, Some(Report::Done), TaskState::Completed),
            (Failed, Some(Report::Failed), TaskState::Failed),
            (Canceled, Some(Report::Canceled), TaskState::Canceled),
        ] {
            assert_eq!(state.report(), report);
            assert_eq!(state.task_state(), task);
        }
        let now = seen(1, Blocked, false);
        assert_eq!(
            now.event_object(Event::Reported(Report::Blocked))["round"]["state"],
            "TASK_STATE_INPUT_REQUIRED"
        );
    }

    #[test]
    fn a_task_state_and_an_outcome_map_to_a_round_state() {
        for (task, want) in [
            (TaskState::InputRequired, Blocked),
            (TaskState::AuthRequired, Blocked),
            (TaskState::Completed, Done),
            (TaskState::Failed, Failed),
            (TaskState::Rejected, Failed),
            (TaskState::Canceled, Canceled),
            (TaskState::Submitted, Working),
            (TaskState::Working, Working),
        ] {
            assert_eq!(RoundState::of_task(&task), want, "{task:?}");
        }
        for (outcome, want) in [
            (Some(RoundOutcome::Done), Some(Done)),
            (Some(RoundOutcome::Failed), Some(Failed)),
            (Some(RoundOutcome::Cancelled), Some(Canceled)),
            (None, None),
        ] {
            assert_eq!(RoundState::of_outcome(outcome), want, "{outcome:?}");
        }
    }

    #[test]
    fn a_read_waits_for_the_interval_a_busy_parent_for_the_minimum_and_a_failed_read_for_the_interval()
     {
        let ms = Duration::from_millis;
        // Nothing changed: once a second.
        assert!(!read_due(false, false, ms(999)));
        assert!(read_due(false, false, ms(1000)));
        // The parent's record changed: after the minimum, not at once.
        assert!(!read_due(true, false, ms(199)));
        assert!(read_due(true, false, ms(200)));
        // A read that failed is not retried on a change.
        assert!(!read_due(true, true, ms(500)));
        assert!(read_due(true, true, ms(1000)));
    }

    #[test]
    fn an_event_status_keeps_the_state_and_drops_the_message() {
        let status = TaskStatus {
            state: TaskState::Working,
            message: Some(a2a::Message::new(
                a2a::Role::Agent,
                vec![a2a::Part::text("hi")],
            )),
            timestamp: None,
        };
        let quiet = quiet(&status);
        assert_eq!(quiet.state, TaskState::Working);
        assert!(quiet.message.is_none());
    }

    #[test]
    fn only_a_round_that_worked_or_waited_can_have_ended_unseen() {
        let unseen = |prev: Seen, round: u32| ended_unseen(&prev, &seen(round, Working, false));
        assert!(unseen(seen(1, Working, false), 2));
        assert!(unseen(seen(1, Blocked, false), 2));
        assert!(!unseen(seen(1, Done, false), 2));
        assert!(!unseen(seen(1, Failed, false), 2));
        assert!(!unseen(seen(1, Working, false), 1));
    }

    #[test]
    fn a_settled_round_closes_a_session_that_is_not_kept_open() {
        let prev = Seen {
            keep_open: false,
            ..seen(1, Working, false)
        };
        let settled = settle(&prev, Done).unwrap();
        assert!(settled.closed && settled.state == Done);
        let kept = seen(1, Working, false);
        assert!(!settle(&kept, Failed).unwrap().closed);
        assert!(settle(&seen(1, Working, true), Canceled).unwrap().closed);
        assert!(settle(&kept, Working).is_none());
        assert!(settle(&kept, Blocked).is_none());
    }

    // ---------------------------------------------------------------------------------------
    // The listing cache: a session whose row did not change is not read again.
    // ---------------------------------------------------------------------------------------

    use std::sync::Arc;

    use adam_core::{DynStore, MemoryStore, NewRun, RunStatus};
    use adam_runtime::{
        AgentError, AgentStarter, BroadcastSink, Inbound, ManualClock, Runtime, RuntimeBuilder,
        SESSION_STATE_KEY, SessionHeader, session_conversation_id,
    };
    use adam_store_testkit::fault::{FaultyStore, Method};

    use crate::child_sessions::{ChildSessionAgent, ChildSessions};
    use crate::convert::encode_conversation;

    /// The registered helper the listing scans for; a front never steps it, so it only starts.
    struct Helper;

    impl AgentStarter for Helper {
        type State = Value;

        fn name(&self) -> &str {
            "helper"
        }

        fn init(&self, _input: Inbound) -> Result<Value, AgentError> {
            Ok(json!({}))
        }
    }

    /// A session with one round of `status`, as `list_sessions` reads it: round 1 is the run whose
    /// id is `session`, its conversation is the session's, its parent is some run, and its agent
    /// state is `agent` (inside the runtime's version 1 envelope).
    async fn a_session_with(
        store: &DynStore,
        parent: &str,
        session: RunId,
        status: RunStatus,
        agent: Value,
    ) {
        store
            .create_run(
                NewRun::new("helper", json!({"v": 1, "agent": agent}))
                    .with_id(session)
                    .conversation(session_conversation_id(parent, session))
                    .parent(RunId::new())
                    .status(status),
            )
            .await
            .expect("create");
    }

    /// [`a_session_with`] for a session whose round carries a header (the usual case).
    async fn a_session(store: &DynStore, parent: &str, status: RunStatus) -> RunId {
        let session = RunId::new();
        let header = SessionHeader {
            id: session,
            agent: "helper".into(),
            description: "Plan it".into(),
            keep_open: true,
            closed: false,
            closing: false,
            round: 1,
            root: None,
            visible: false,
        };
        a_session_with(
            store,
            parent,
            session,
            status,
            json!({ SESSION_STATE_KEY: header }),
        )
        .await;
        session
    }

    /// A backend over `builder`'s runtime that serves `child-sessions/v1` with one helper.
    fn backend(builder: RuntimeBuilder) -> RuntimeTaskBackend {
        let runtime = builder.starter(Helper).build();
        let sessions =
            ChildSessions::new([ChildSessionAgent::new("planner", "helper", "Plans.")], 16)
                .expect("child sessions");
        RuntimeTaskBackend::new(runtime, BroadcastSink::default(), "parent")
            .with_child_sessions(sessions)
    }

    fn working() -> TaskStatus {
        TaskStatus {
            state: TaskState::Working,
            message: None,
            timestamp: None,
        }
    }

    /// Ten listings of three open sessions read each session **once**: their row did not change,
    /// so the view (two queries and a head read per open round) is skipped. Without the
    /// cache it was one view per session per listing: 3 at the seed and 30 over the ten.
    #[tokio::test]
    async fn a_listing_does_not_read_a_session_whose_row_did_not_change() {
        let faulty = Arc::new(FaultyStore::new(Arc::new(MemoryStore::new())));
        let store: DynStore = faulty.clone();
        let backend = backend(Runtime::builder(store.clone()));
        let caller = Caller::new("alice");
        let context = "ctx-1";
        let conversation = encode_conversation("alice", context);
        for _ in 0..3 {
            a_session(&store, &conversation, RunStatus::Runnable).await;
        }

        let mut tracker = SessionsTracker::seed(&backend, &caller, context, 1)
            .await
            .expect("seed");
        assert_eq!(
            faulty.calls(Method::LoadHead),
            3,
            "the seed reads one view per session"
        );
        assert_eq!(
            faulty.calls(Method::LoadRun),
            0,
            "a view reads the head, not the whole state"
        );
        for listing in 0..10 {
            let events = tracker
                .refresh(&backend, &caller, "task-1", context, 1, &working())
                .await;
            assert!(events.is_empty(), "listing {listing} said {events:?}");
        }
        assert_eq!(
            faulty.calls(Method::LoadHead),
            3,
            "ten listings of three unchanged sessions: 3 views, not 33"
        );
        assert_eq!(
            faulty.calls(Method::LeaseUntil),
            3,
            "a view reads the lease too, and nothing else did"
        );
    }

    /// The cache is the listing row, so a session the idle TTL closes is read again and told
    /// `closed`: it changes with no commit, and a version-only cache would swallow the event.
    #[tokio::test]
    async fn a_session_the_idle_ttl_closes_is_read_again_and_told() {
        let store: DynStore = Arc::new(MemoryStore::new());
        let clock = ManualClock::new();
        let backend = backend(
            Runtime::builder(store.clone())
                .clock(clock.clone())
                .session_idle_ttl(Duration::from_secs(10)),
        );
        let caller = Caller::new("alice");
        let context = "ctx-1";
        a_session(
            &store,
            &encode_conversation("alice", context),
            RunStatus::Done,
        )
        .await;

        let mut tracker = SessionsTracker::seed(&backend, &caller, context, 1)
            .await
            .expect("seed");
        // The session is idle, not closed: its round ended and it is kept open.
        clock.advance(Duration::from_secs(11));
        let events = tracker
            .refresh(&backend, &caller, "task-1", context, 1, &working())
            .await;
        let closed = events.iter().any(|e| match e {
            TaskEvent::Status(update) => update
                .metadata
                .as_ref()
                .and_then(|m| m.get(CHILD_SESSIONS_EXTENSION))
                .is_some_and(|o| o["event"] == "closed"),
            _ => false,
        });
        assert!(closed, "the closed event is still told: {events:?}");
    }

    /// A view that cannot be read leaves the session out of the listing instead of caching a
    /// guessed `working`: the next listing reads it again, so a parked round's `blocked` report is
    /// not lost. (A parked round never commits, so a wrong cached `Seen` would never be corrected
    /// by a later row.)
    #[tokio::test]
    async fn a_view_that_fails_leaves_the_session_out_and_is_retried() {
        let faulty = Arc::new(FaultyStore::new(Arc::new(MemoryStore::new())));
        let store: DynStore = faulty.clone();
        let backend = backend(Runtime::builder(store.clone()));
        let caller = Caller::new("alice");
        let context = "ctx-1";
        let session = RunId::new();
        a_session_with(
            &store,
            &encode_conversation("alice", context),
            session,
            RunStatus::Parked,
            json!({}),
        )
        .await;
        // The seed's view of the session's round fails once: the store is unavailable.
        faulty.fail_run(Method::LoadHead, session, 1);

        let mut tracker = SessionsTracker::seed(&backend, &caller, context, 1)
            .await
            .expect("seed");
        // The next listing reads it again, and the parked round is reported as blocked.
        let events = tracker
            .refresh(&backend, &caller, "task-1", context, 1, &working())
            .await;
        let said: Vec<Value> = events
            .iter()
            .filter_map(|e| match e {
                TaskEvent::Status(update) => update
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get(CHILD_SESSIONS_EXTENSION))
                    .cloned(),
                _ => None,
            })
            .collect();
        let names: Vec<String> = said
            .iter()
            .map(|o| match o["report"].as_str() {
                Some(report) => format!("{}:{report}", o["event"].as_str().unwrap_or("?")),
                None => o["event"].as_str().unwrap_or("?").to_owned(),
            })
            .collect();
        assert_eq!(names, ["opened", "reported:blocked"], "{said:?}");
    }
}
