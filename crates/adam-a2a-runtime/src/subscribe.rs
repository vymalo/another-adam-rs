//! Replayable subscriptions: durable polling merged with live events.

use std::collections::HashSet;
use std::time::Duration;

use a2a::{
    Message, Part, Role, TaskArtifactUpdateEvent, TaskState, TaskStatus, TaskStatusUpdateEvent,
};
use adam_a2a::{BackendError, Caller, TaskEvent};
use adam_runtime::{RunEvent, RunSubscription, RunView};
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;
use tokio_stream::wrappers::ReceiverStream;

use crate::backend::{RuntimeTaskBackend, map_err};
use crate::convert::{artifact_id, artifact_of, status_key, status_of, task_from_view};

/// Consecutive failed reads of the durable run before a subscription gives up.
const MAX_READ_FAILURES: u32 = 20;

/// Buffered events between the pump and a slow HTTP client.
const CHANNEL_CAPACITY: usize = 64;

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
    last: (TaskState, Option<String>),
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
            RunEvent::Custom { kind, payload } => {
                let part = Part::data(json!({ "kind": kind, "payload": payload }));
                vec![working(Message::new(Role::Agent, vec![part]))]
            }
            RunEvent::Artifact {
                name,
                mime_type,
                data,
            } => {
                let artifact = adam_runtime::Artifact {
                    name,
                    mime_type,
                    data,
                };
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
    // snapshot and the first poll is missed by both.
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
