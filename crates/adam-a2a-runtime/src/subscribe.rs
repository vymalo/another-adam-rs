//! Replayable subscriptions: durable polling merged with live events.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use a2a::{
    Message, Part, Role, TaskArtifactUpdateEvent, TaskState, TaskStatus, TaskStatusUpdateEvent,
};
use adam_a2a::{BackendError, Caller, STEPS_EXTENSION, TEXT_STREAM_EXTENSION, TaskEvent};
use adam_runtime::{AGENT_TEXT_KIND, RunEvent, RunSubscription, RunView, StepEvent, StepState};
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_stream::wrappers::ReceiverStream;

use crate::backend::{RuntimeTaskBackend, map_err};
use crate::convert::{StatusKey, artifact_id, artifact_of, status_key, status_of, task_from_view};
use crate::steps::step_message;
use crate::text_stream;

/// Consecutive failed reads of the durable run before a subscription gives up.
const MAX_READ_FAILURES: u32 = 20;

/// Buffered events between the pump and a slow HTTP client.
const CHANNEL_CAPACITY: usize = 64;

/// How often a step reports the same state again to a client that activated `steps/v1`: the
/// contract asks agents for at most one update per step per second, and the orchestration layer
/// keeps only a few. A change of state, and the start and the end of a step, always go out.
const STEP_UPDATE_INTERVAL: Duration = Duration::from_secs(1);

/// The most steps whose last update a subscription remembers: past it the memory starts again
/// (the next update of each goes out, which costs a line, not correctness).
const MAX_TRACKED_STEPS: usize = 1024;

pub(crate) fn subscribe(
    backend: RuntimeTaskBackend,
    caller: Caller,
    task_id: String,
) -> BoxStream<'static, Result<TaskEvent, BackendError>> {
    let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
    tokio::spawn(async move {
        if let Err(e) = pump(&backend, &caller, &task_id, &tx).await {
            let _ = tx.send(Err(e)).await;
        }
    });
    ReceiverStream::new(rx).boxed()
}

/// What the subscriber has been told so far.
struct Tracker {
    task_id: String,
    context_id: String,
    seen_artifacts: HashSet<String>,
    last: StatusKey,
    /// The request activated `steps/v1`: a step goes out with its report, at most one update a
    /// second. Without it a step is a line of text, and every line goes out, as progress always did.
    steps: bool,
    /// The state each open step last reported to this subscriber, and when (only with `steps`).
    reported: HashMap<String, (StepState, Instant)>,
    /// The request activated `text-stream/v1`: the model's words go out as chunks as they are
    /// written, and the words before a tool call as a status of their own. Without it the whole
    /// reply arrives with the turn, as it always did.
    text_stream: bool,
}

impl Tracker {
    /// Events for whatever the durable `view` says that the subscriber does
    /// not know yet: new artifacts first, then a changed status.
    fn sync(&mut self, view: &RunView, backend: &RuntimeTaskBackend) -> Vec<TaskEvent> {
        let mut events = Vec::new();
        for artifact in &view.artifacts {
            if self.seen_artifacts.insert(artifact_id(artifact)) {
                events.push(self.artifact_event(artifact));
            }
        }
        let status = status_of(view, &backend.prompt);
        let key = status_key(&status);
        if key != self.last {
            self.last = key;
            events.push(TaskEvent::Status(TaskStatusUpdateEvent {
                task_id: self.task_id.clone(),
                context_id: self.context_id.clone(),
                status,
                metadata: None,
            }));
        }
        events
    }

    fn artifact_event(&self, artifact: &adam_runtime::Artifact) -> TaskEvent {
        TaskEvent::Artifact(TaskArtifactUpdateEvent {
            task_id: self.task_id.clone(),
            context_id: self.context_id.clone(),
            artifact: artifact_of(artifact),
            append: None,
            last_chunk: Some(true),
            metadata: None,
        })
    }

    /// Events for one live run event. Status changes are not mapped from the
    /// event (the durable record is the truth); they only trigger a re-read.
    fn live(&mut self, event: RunEvent) -> Vec<TaskEvent> {
        let working = |message: Message| {
            TaskEvent::Status(TaskStatusUpdateEvent {
                task_id: self.task_id.clone(),
                context_id: self.context_id.clone(),
                status: TaskStatus {
                    state: TaskState::Working,
                    message: Some(message),
                    timestamp: Some(chrono::Utc::now()),
                },
                metadata: None,
            })
        };
        match event {
            RunEvent::Progress { message } => {
                vec![working(Message::new(
                    Role::Agent,
                    vec![Part::text(message)],
                ))]
            }
            RunEvent::Step(step) => {
                if self.steps && !admit(&mut self.reported, &step, Instant::now()) {
                    return Vec::new();
                }
                vec![working(step_message(&step, self.steps))]
            }
            RunEvent::TextDelta {
                stream,
                offset,
                text,
                last,
                abandoned,
            } => {
                if !self.text_stream {
                    return Vec::new();
                }
                text_stream::chunk(
                    &self.task_id,
                    &self.context_id,
                    &stream,
                    offset,
                    text,
                    last,
                    abandoned,
                )
                .map(|chunk| vec![TaskEvent::Artifact(chunk)])
                .unwrap_or_default()
            }
            RunEvent::ReasoningDelta {
                stream,
                offset,
                text,
                last,
                abandoned,
            } => {
                // Only to a client that activated the extension, and only as a chunk: reasoning is
                // not the answer, so no status message states it whole, and a client that did not
                // activate it reads nothing of it.
                if !self.text_stream {
                    return Vec::new();
                }
                text_stream::reasoning_chunk(
                    &self.task_id,
                    &self.context_id,
                    &stream,
                    offset,
                    text,
                    last,
                    abandoned,
                )
                .map(|chunk| vec![TaskEvent::Artifact(chunk)])
                .unwrap_or_default()
            }
            RunEvent::Custom { kind, payload } => {
                // The words of a turn that were streamed, said whole: a status of their own, with the
                // stream's id, so the chunks they are the text of are known by it.
                if self.text_stream
                    && kind == AGENT_TEXT_KIND
                    && let Some((stream, text)) = text_stream::words_of(&payload)
                {
                    return vec![working(text_stream::words_message(stream, text.to_owned()))];
                }
                let part = Part::data(json!({ "kind": kind, "payload": payload }));
                vec![working(Message::new(Role::Agent, vec![part]))]
            }
            RunEvent::Artifact {
                name,
                mime_type,
                data,
                file,
            } => {
                let mut artifact = adam_runtime::Artifact::new(name, mime_type, data);
                artifact.file = file;
                if self.seen_artifacts.insert(artifact_id(&artifact)) {
                    vec![self.artifact_event(&artifact)]
                } else {
                    Vec::new()
                }
            }
            RunEvent::Status { .. } => Vec::new(),
        }
    }
}

/// Whether a client that activated `steps/v1` is sent this report: always at the start, at the end
/// and on a change of state, and an update of the state it is in only once per
/// [`STEP_UPDATE_INTERVAL`]. `reported` is what this subscriber has been told of each open step.
fn admit(
    reported: &mut HashMap<String, (StepState, Instant)>,
    step: &StepEvent,
    now: Instant,
) -> bool {
    if step.state.is_end() {
        reported.remove(&step.id);
        return true;
    }
    let quiet = reported.get(&step.id).is_some_and(|(state, at)| {
        *state == step.state && now.saturating_duration_since(*at) < STEP_UPDATE_INTERVAL
    });
    if quiet {
        return false;
    }
    if reported.len() >= MAX_TRACKED_STEPS {
        reported.clear();
    }
    reported.insert(step.id.clone(), (step.state, now));
    true
}

async fn send(
    tx: &mpsc::Sender<Result<TaskEvent, BackendError>>,
    events: Vec<TaskEvent>,
) -> Option<bool> {
    let mut ends = false;
    for event in events {
        ends |= event.ends_stream();
        tx.send(Ok(event)).await.ok()?;
    }
    Some(ends)
}

/// The subscription: snapshot, then live events and polls until the task ends
/// or waits for its caller. `Ok(())` also when the client went away.
async fn pump(
    backend: &RuntimeTaskBackend,
    caller: &Caller,
    task_id: &str,
    tx: &mpsc::Sender<Result<TaskEvent, BackendError>>,
) -> Result<(), BackendError> {
    // Attach to live events first, so nothing that happens between the
    // snapshot and the first poll is missed by both. The subscription also starts
    // with the run's recent events, which covers the gap between `submit` and here.
    let Some((run, view, context_id)) = backend.owned(caller, task_id).await? else {
        return Err(BackendError::TaskNotFound(task_id.to_owned()));
    };
    let mut live: Option<RunSubscription> = Some(backend.events.subscribe_run(run));
    // Re-read after attaching: the snapshot must not predate the attach.
    let view = match backend.owned(caller, task_id).await? {
        Some((_, fresher, _)) => fresher,
        None => view,
    };

    let task = task_from_view(&view, &context_id, &backend.prompt);
    let mut tracker = Tracker {
        task_id: task.id.clone(),
        context_id,
        seen_artifacts: view.artifacts.iter().map(artifact_id).collect(),
        last: status_key(&task.status),
        steps: caller.has_extension(STEPS_EXTENSION),
        reported: HashMap::new(),
        text_stream: caller.has_extension(TEXT_STREAM_EXTENSION),
    };
    let ends = task.status.state.is_terminal()
        || matches!(
            task.status.state,
            TaskState::InputRequired | TaskState::AuthRequired
        );
    if send(tx, vec![TaskEvent::Snapshot(task)]).await.is_none() || ends {
        return Ok(());
    }

    let mut ticker = tokio::time::interval(backend.poll);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut failures = 0u32;
    loop {
        let mut poll = false;
        tokio::select! {
            () = tx.closed() => return Ok(()),
            _ = ticker.tick() => poll = true,
            event = async {
                match live.as_mut() {
                    Some(sub) => sub.recv().await,
                    None => std::future::pending().await,
                }
            } => match event {
                Some(RunEvent::Status { .. }) => poll = true,
                Some(event) => {
                    let events = tracker.live(event);
                    if send(tx, events).await.is_none() {
                        return Ok(());
                    }
                }
                // The sink is gone: keep going on the durable record alone.
                None => live = None,
            },
        }
        if !poll {
            continue;
        }
        let view = match backend.runtime.view(run).await {
            Ok(Some(view)) => {
                failures = 0;
                view
            }
            Ok(None) => return Err(BackendError::TaskNotFound(task_id.to_owned())),
            Err(e) => {
                failures += 1;
                tracing::warn!(%run, error = %e, failures, "reading the run failed; retrying");
                if failures >= MAX_READ_FAILURES {
                    return Err(map_err(e));
                }
                continue;
            }
        };
        let mut events = tracker.sync(&view, backend);
        if events.iter().any(TaskEvent::ends_stream) {
            // Progress that was queued before the final commit still belongs
            // before the end of the stream.
            let mut queued = Vec::new();
            if let Some(sub) = live.as_mut() {
                while let Ok(Some(event)) = tokio::time::timeout(Duration::ZERO, sub.recv()).await {
                    queued.extend(tracker.live(event));
                }
            }
            queued.append(&mut events);
            events = queued;
        }
        match send(tx, events).await {
            None | Some(true) => return Ok(()),
            Some(false) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use adam_runtime::StepKind;

    use super::*;

    fn step(id: &str, state: StepState) -> StepEvent {
        StepEvent::new(id, StepKind::Command, "npm test", state)
    }

    /// `n` seconds after `t0`.
    fn at(t0: Instant, n: u64) -> Instant {
        t0 + Duration::from_secs(n)
    }

    #[test]
    fn a_step_reports_the_same_state_once_a_second_and_every_change_of_state() {
        let t0 = Instant::now();
        let mut reported = HashMap::new();
        let mut admitted = |id: &str, state, when| admit(&mut reported, &step(id, state), when);
        // The start goes out; the same state again within the second does not ...
        assert!(admitted("a", StepState::Running, t0));
        assert!(!admitted("a", StepState::Running, at(t0, 0)));
        assert!(!admitted(
            "a",
            StepState::Running,
            t0 + Duration::from_millis(999)
        ));
        // ... and goes out once the second is over, from which the next second counts.
        assert!(admitted("a", StepState::Running, at(t0, 1)));
        assert!(!admitted(
            "a",
            StepState::Running,
            at(t0, 1) + Duration::from_millis(500)
        ));
        // A change of state is never held back, even at once: a step that asks for a person has
        // to say it is waiting.
        assert!(admitted(
            "a",
            StepState::Waiting,
            at(t0, 1) + Duration::from_millis(500)
        ));
        assert!(!admitted("a", StepState::Waiting, at(t0, 2)));
        assert!(admitted("a", StepState::Running, at(t0, 2)));
        // Another step has its own second.
        assert!(admitted("b", StepState::Running, at(t0, 2)));
        // The end always goes out, and a step that starts again (a retry) starts afresh.
        assert!(admitted("a", StepState::Failed, at(t0, 2)));
        assert!(admitted("a", StepState::Running, at(t0, 2)));
    }

    #[test]
    fn what_is_remembered_is_bounded() {
        let t0 = Instant::now();
        let mut reported = HashMap::new();
        for n in 0..MAX_TRACKED_STEPS {
            assert!(admit(
                &mut reported,
                &step(&format!("s{n}"), StepState::Running),
                t0
            ));
        }
        assert_eq!(reported.len(), MAX_TRACKED_STEPS);
        // One more starts the memory again: the cost is a line that might have been held back.
        assert!(admit(
            &mut reported,
            &step("one-more", StepState::Running),
            t0
        ));
        assert_eq!(reported.len(), 1);
    }
}
