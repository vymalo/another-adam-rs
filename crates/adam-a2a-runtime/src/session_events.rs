//! The events of `child-sessions/v1` on a parent task's stream (contract section 3).
//!
//! A subscriber that activated the extension is told when a session of its parent context changes:
//! `opened`, `round`, `reported`, `closed`. The runtime has no events for it, so the subscription
//! lists the sessions (on a change of the parent's record and at least every [`SESSIONS_POLL`])
//! and turns what changed since the last listing into events ([`diff`], a pure function). An event
//! is a latency optimisation: the sessions object of the task is the truth, so a listing that
//! failed is retried at the next poll and says nothing.

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

/// The longest a subscription goes without listing the sessions of its parent context (and, for the
/// stream of a round, without reading its session): a session changes with no change of the
/// parent's own record when a person acts on it.
pub(crate) const SESSIONS_POLL: Duration = Duration::from_secs(1);

/// The least time between two listings when the parent's record changed: a busy parent commits far
/// more often than a client needs to hear of its sessions, and each listing pages the sessions
/// and reads a view per open one.
pub(crate) const SESSIONS_MIN_INTERVAL: Duration = Duration::from_millis(200);

/// Whether a subscription reads its sessions again: [`SESSIONS_POLL`] after the last read, or
/// [`SESSIONS_MIN_INTERVAL`] after it when the parent's record changed. A read that failed waits
/// the whole [`SESSIONS_POLL`], whatever changed.
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

/// What the stream of a parent task knows of its sessions.
pub(crate) struct SessionsTracker {
    seen: HashMap<RunId, Seen>,
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
        let seen = backend.sessions_seen(caller, context).await?;
        Some(Self {
            seen: seen.into_iter().map(|s| (s.session, s)).collect(),
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
        let Some(now) = backend.sessions_seen(caller, context_id).await else {
            self.failed = true;
            return Vec::new();
        };
        self.failed = false;
        let status = &quiet(status);
        let mut events = Vec::new();
        for now in now {
            let prev = self.seen.get(&now.session).cloned();
            // A round that was working and is gone from view (a newer one started) ended unseen:
            // read how, so its report is not lost.
            let mut chain = Vec::new();
            if let Some(prev) = &prev
                && ended_unseen(prev, &now)
                && let Some(settled) = backend.settled(prev).await
            {
                chain.push(settled);
            }
            chain.push(now.clone());
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
            self.seen.insert(now.session, now);
        }
        events
    }
}

impl RuntimeTaskBackend {
    /// The sessions of parent context `context` as [`Seen`]; `None` when they could not be listed
    /// (a warning, at most one per interval: the caller backs off). A session whose latest round
    /// is not readable is left out, so nothing is said of it yet.
    ///
    /// The listing is paged and the sessions can move between pages while it is read, so a session
    /// may be missed by one listing and told `opened` by a later one.
    async fn sessions_seen(&self, caller: &Caller, context: &str) -> Option<Vec<Seen>> {
        let listed = match self.listed_sessions(caller, context).await {
            Ok(listed) => listed,
            Err(e) => {
                tracing::warn!(error = %e, "the sessions of a context could not be listed");
                return None;
            }
        };
        let mut out = Vec::with_capacity(listed.len());
        for s in &listed {
            out.extend(self.seen_of(context, s).await);
        }
        Some(out)
    }

    async fn seen_of(&self, context: &str, s: &SessionSummary) -> Option<Seen> {
        let state = match s.state {
            // Only an open round can wait for the person, and only its view says so.
            SessionState::Working => match self.runtime.view(s.latest_run).await {
                Ok(Some(view)) => RoundState::of_task(&task_state(&view)),
                _ => RoundState::Working,
            },
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
            closed: wire_state(s.state, s.closing) == "closed",
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
}
