//! Replayable subscriptions: durable polling merged with live events.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use a2a::{
    Artifact, Message, Part, Role, Task, TaskArtifactUpdateEvent, TaskState, TaskStatus,
    TaskStatusUpdateEvent,
};
use adam_a2a::{
    AccessTier, BackendError, CHILD_SESSIONS_EXTENSION, Caller, STEPS_EXTENSION,
    TEXT_STREAM_EXTENSION, TaskEvent, USAGE_EXTENSION,
};
use adam_runtime::{
    AGENT_TEXT_KIND, ArtifactRef, FileRef, RunEvent, RunSubscription, RunView, StepEvent, StepState,
};
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::Value;
use serde_json::json;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio::time::{Instant, MissedTickBehavior};
use tokio_stream::wrappers::ReceiverStream;

use crate::backend::{RuntimeTaskBackend, map_err};
use crate::child_sessions::{ContextKind, Round, kind_of};
use crate::convert::{ArtifactIds, StatusKey, artifact_slot, status_key, status_of};
use crate::session_events::{SessionsTracker, quiet, read_due};
use crate::steps::step_message;
use crate::text_stream;
use crate::tier;
use crate::usage;

/// Consecutive failed reads of the durable run before a subscription gives up.
const MAX_READ_FAILURES: u32 = 20;

/// Buffered events between the pump and a slow HTTP client: at most this many, and at most
/// [`CHANNEL_BYTES`] of payload.
const CHANNEL_CAPACITY: usize = 64;

/// The most bytes of event payload one subscription holds for a client that does not read: without
/// it, 64 events of a 4 MiB file artifact would be 256 MiB per subscriber. Past it, a **durable**
/// artifact event that would fit an empty queue is dropped for that subscriber (it is in the task:
/// `GetTask`, or a resubscription's snapshot, brings it back); a transient chunk of a stream, a
/// snapshot, a status and an error wait for room instead, because no read can bring them back. A
/// snapshot larger than this takes the whole budget and its excess is outside it, so a subscriber
/// holds at most one snapshot or one over-budget artifact, plus this much. The cap is per
/// subscriber: N subscribers on a stalled run hold N times it, not one shared copy.
const CHANNEL_BYTES: usize = 8 * 1024 * 1024;

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
    let (queue, rx) = Queue::new();
    tokio::spawn(async move {
        if let Err(e) = pump(&backend, &caller, &task_id, &queue).await {
            let _ = queue.push(Err(e)).await;
        }
    });
    ReceiverStream::new(rx).map(|(item, _permit)| item).boxed()
}

/// One queued event and the byte budget it holds, released when the subscriber takes it (or when
/// the queue is dropped).
type Queued = (
    Result<TaskEvent, BackendError>,
    Option<OwnedSemaphorePermit>,
);

/// The queue between the pump and one subscriber: bounded by [`CHANNEL_CAPACITY`] events and by
/// [`CHANNEL_BYTES`] of payload.
struct Queue {
    tx: mpsc::Sender<Queued>,
    budget: Arc<Semaphore>,
}

impl Queue {
    fn new() -> (Self, mpsc::Receiver<Queued>) {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        (
            Self {
                tx,
                budget: Arc::new(Semaphore::new(CHANNEL_BYTES)),
            },
            rx,
        )
    }

    /// Queue `events`, in order. `None` when the subscriber is gone, else whether the stream ends.
    async fn send(&self, events: Vec<TaskEvent>) -> Option<bool> {
        let mut ends = false;
        for event in events {
            ends |= event.ends_stream();
            if !self.push(Ok(event)).await {
                return None;
            }
        }
        Some(ends)
    }

    /// Queue one item; `false` when the subscriber is gone. A **durable** artifact that would fit an
    /// empty queue (`bytes < CHANNEL_BYTES`) and does not fit now is dropped: it is in the task, so
    /// `GetTask` and a resubscription's snapshot bring it back. An artifact larger than the whole
    /// budget waits for the queue to drain — it could not have fitted even an empty one, so dropping
    /// it would leave it out of every stream, though `GetTask` still has it. A transient chunk, a
    /// snapshot, a status and an error are never dropped either: they wait for room.
    async fn push(&self, item: Result<TaskEvent, BackendError>) -> bool {
        let bytes = item_bytes(&item);
        let want = u32::try_from(bytes.min(CHANNEL_BYTES)).unwrap_or(u32::MAX);
        // Only what could have fitted an empty queue may be dropped: an item clamped to the whole
        // budget is larger than it, and dropping it would lose it for good.
        let fits_empty = bytes < CHANNEL_BYTES;
        let permit = match self.budget.clone().try_acquire_many_owned(want) {
            Ok(permit) => Some(permit),
            Err(_) if fits_empty && droppable(&item) => {
                tracing::debug!(
                    bytes,
                    "dropping a durable artifact event for a slow subscriber"
                );
                return true;
            }
            Err(_) => {
                // What no read can bring back waits for the client to read, or leaves when it went
                // away.
                tokio::select! {
                    permit = self.budget.clone().acquire_many_owned(want) => match permit {
                        Ok(permit) => Some(permit),
                        Err(_) => return false,
                    },
                    () = self.tx.closed() => return false,
                }
            }
        };
        self.tx.send((item, permit)).await.is_ok()
    }

    /// The bytes of payload queued for this subscriber, for the test of the bound.
    #[cfg(test)]
    fn queued_bytes(&self) -> usize {
        CHANNEL_BYTES - self.budget.available_permits()
    }
}

/// Whether an item may be dropped for a subscriber that does not read. A **durable** artifact is
/// also in the task: `GetTask` reads it, and a resubscription's snapshot carries it, so losing it
/// from the stream costs a read, not the artifact. A **transient** chunk of a stream (a
/// `text-stream/v1` reply or its reasoning) is nowhere else — never in a `GetTask`, never replayed
/// from the record — so it is never dropped: it is small, and waiting for room is free. A snapshot,
/// a status and an error are the state that ends the stream and are never dropped either.
fn droppable(item: &Result<TaskEvent, BackendError>) -> bool {
    match item {
        Ok(TaskEvent::Artifact(update)) => !text_stream::is_chunk(update),
        _ => false,
    }
}

/// The bytes of payload an item holds, for the queue bound: the parts of its artifact or message (a
/// file's bytes themselves) and the metadata around them, with a fixed cost for the event's own
/// fields. A bound on what the queue holds, not a measurement of it.
///
/// A file artifact's raw part holds exactly the `FileRef::size` its head declared, so the charge
/// of a file event is that size; the queue measures the built event (after the fetch), it is not
/// given the ref beforehand. An artifact whose blob is gone has no event to charge.
fn item_bytes(item: &Result<TaskEvent, BackendError>) -> usize {
    let event = match item {
        Ok(TaskEvent::Snapshot(task)) => task_bytes(task),
        Ok(TaskEvent::Status(update)) => {
            message_bytes(update.status.message.as_ref()) + metadata_bytes(update.metadata.as_ref())
        }
        Ok(TaskEvent::Artifact(update)) => {
            artifact_bytes(&update.artifact) + metadata_bytes(update.metadata.as_ref())
        }
        Err(_) => 0,
    };
    // An event is never free: what it holds is bounded by the count of events as well, and a
    // status of no message still takes a slot.
    64 + event
}

fn task_bytes(task: &Task) -> usize {
    let mut bytes = 128 + task.id.len() + task.context_id.len();
    for artifact in task.artifacts.iter().flatten() {
        bytes += artifact_bytes(artifact);
    }
    for message in task.history.iter().flatten() {
        bytes += message_bytes(Some(message));
    }
    bytes + metadata_bytes(task.metadata.as_ref())
}

fn artifact_bytes(artifact: &Artifact) -> usize {
    let mut bytes = 128
        + artifact.artifact_id.len()
        + artifact.name.as_deref().map_or(0, str::len)
        + artifact.description.as_deref().map_or(0, str::len);
    for part in &artifact.parts {
        bytes += part_bytes(part);
    }
    for extension in artifact.extensions.iter().flatten() {
        bytes += extension.len();
    }
    bytes + metadata_bytes(artifact.metadata.as_ref())
}

fn message_bytes(message: Option<&Message>) -> usize {
    let Some(message) = message else {
        return 0;
    };
    let mut bytes = 128 + message.message_id.len();
    for part in &message.parts {
        bytes += part_bytes(part);
    }
    bytes + metadata_bytes(message.metadata.as_ref())
}

fn part_bytes(part: &Part) -> usize {
    128 + match &part.content {
        a2a::PartContent::Text(text) => text.len(),
        a2a::PartContent::Raw(bytes) => bytes.len(),
        a2a::PartContent::Url(url) => url.len(),
        a2a::PartContent::Data(value) => value_bytes(value),
    } + part.filename.as_deref().map_or(0, str::len)
        + part.media_type.as_deref().map_or(0, str::len)
        + metadata_bytes(part.metadata.as_ref())
}

fn metadata_bytes(metadata: Option<&HashMap<String, Value>>) -> usize {
    let Some(metadata) = metadata else {
        return 0;
    };
    metadata
        .iter()
        .map(|(key, value)| key.len() + value_bytes(value))
        .sum()
}

/// The bytes a JSON value holds, counted without serialising it: a bound on its memory, not on its
/// text (a string of 1 MiB is at least 1 MiB, and a node is more than its content).
fn value_bytes(value: &Value) -> usize {
    match value {
        Value::Null | Value::Bool(_) => 16,
        Value::Number(_) => 24,
        Value::String(text) => 32 + text.len(),
        Value::Array(items) => 24 + items.iter().map(value_bytes).sum::<usize>(),
        Value::Object(map) => {
            24 + map
                .iter()
                .map(|(key, value)| 32 + key.len() + value_bytes(value))
                .sum::<usize>()
        }
    }
}

/// What a poll compares to know the run has not changed since the last one: the store version
/// (bumped by every commit) and whether a worker holds the run (a lease changes without a commit).
/// The version alone detects every state change; `claimed` is the lease, which the record's version
/// does not carry. `updated_at` is deliberately not here: it cannot move without a commit.
type RunStamp = (u64, bool);

/// What the subscriber has been told so far.
struct Tracker {
    task_id: String,
    context_id: String,
    /// What this subscriber was last sent, by slot (see `artifact_slot`).
    seen_artifacts: HashMap<String, String>,
    /// The artifacts of the run as this subscriber maps them, so a poll derives only the ones the
    /// run added or replaced.
    ids: ArtifactIds,
    /// The run as the last sync read it: a poll that reads the same has nothing to say.
    seen: Option<RunStamp>,
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
    /// The request activated `usage/v1`: each completed model call is reported as a status update with
    /// no message. Without it nothing is said of them (the totals are on the task either way).
    usage: bool,
    /// The request activated `access-tier/v1`: the tier the subscriber was last told, which a status
    /// update says again when the run's tier changes. `None` when it did not activate it.
    tier: Option<AccessTier>,
    /// The latest status that did not end the stream: what an event of a session says the task's
    /// state is.
    open_status: TaskStatus,
}

/// The stream of a round of a session, told what its session says (contract section 5).
struct RoundStream {
    round: Round,
    /// The last read said the session is closing, so it has been told.
    closing_told: bool,
    checked_at: Instant,
    version: u64,
}

impl RoundStream {
    fn due(&self, version: u64) -> bool {
        read_due(version != self.version, false, self.checked_at.elapsed())
    }
}

fn waits(state: &TaskState) -> bool {
    state.is_terminal() || matches!(state, TaskState::InputRequired | TaskState::AuthRequired)
}

impl Tracker {
    /// A tracker for `view`, which the snapshot of `task` was built from: the artifacts it holds
    /// are mapped once, and a poll that reads the same run again has nothing to do.
    fn new(
        task: &Task,
        context_id: String,
        view: &RunView,
        caller: &Caller,
        tier: Option<AccessTier>,
    ) -> Self {
        let mut ids = ArtifactIds::default();
        let seen_artifacts = view
            .artifacts
            .iter()
            .enumerate()
            .map(|(at, artifact)| {
                let said = ids.mapped(at, artifact);
                (said.id, said.content)
            })
            .collect();
        Self {
            task_id: task.id.clone(),
            context_id,
            seen_artifacts,
            ids,
            seen: Some((view.version, view.claimed)),
            last: status_key(&task.status),
            steps: caller.has_extension(STEPS_EXTENSION),
            reported: HashMap::new(),
            text_stream: caller.has_extension(TEXT_STREAM_EXTENSION),
            usage: caller.has_extension(USAGE_EXTENSION),
            tier,
            open_status: quiet(&task.status),
        }
    }

    /// Events for whatever the durable `view` says that the subscriber does
    /// not know yet: new artifacts first, then a changed status.
    async fn sync(&mut self, view: &RunView, backend: &RuntimeTaskBackend) -> Vec<TaskEvent> {
        // A poll that reads the run it already read has nothing to say, and says it without
        // hashing or serialising an artifact and without building a status.
        let stamp = (view.version, view.claimed);
        if self.seen == Some(stamp) {
            return Vec::new();
        }
        self.seen = Some(stamp);
        let mut events = self.artifacts(&view.artifacts, backend).await;
        let status = status_of(view, &backend.prompt);
        let key = status_key(&status);
        if !waits(&status.state) {
            self.open_status = quiet(&status);
        }
        // A tier that changed is said once, on the status update that carries it: the one of a
        // change of state, or one of its own with the state as it is.
        let tier = match (self.tier, backend.tier_of(view)) {
            (Some(told), Some(now)) if told != now => {
                self.tier = Some(now);
                Some(tier::echo(now))
            }
            _ => None,
        };
        if key != self.last || tier.is_some() {
            self.last = key;
            events.push(TaskEvent::Status(TaskStatusUpdateEvent {
                task_id: self.task_id.clone(),
                context_id: self.context_id.clone(),
                status,
                metadata: tier,
            }));
        }
        events
    }

    /// Events for the artifacts this subscriber has not been sent, each mapped from the entry of
    /// the last poll when it is the same artifact.
    ///
    /// A file artifact's bytes are fetched here, once per blob (the runtime's cache answers a
    /// later poll); an artifact whose blob is gone is left out, and a later version offers it
    /// again.
    async fn artifacts(
        &mut self,
        artifacts: &[ArtifactRef],
        backend: &RuntimeTaskBackend,
    ) -> Vec<TaskEvent> {
        let mut events = Vec::new();
        for (at, artifact) in artifacts.iter().enumerate() {
            let said = self.ids.mapped(at, artifact);
            if self.seen_artifacts.get(&said.id) == Some(&said.content) {
                continue;
            }
            let Some(event) = backend
                .artifact_of_with(artifact, said.id.clone())
                .await
                .map(|served| self.artifact_event(served))
            else {
                continue;
            };
            self.seen_artifacts.insert(said.id, said.content);
            events.push(event);
        }
        events
    }

    /// The slot of `artifact` when it is news to this subscriber, and now known to it: its slot
    /// last held other content (or nothing).
    fn admit(&mut self, artifact: &ArtifactRef) -> Option<String> {
        let (slot, content) = artifact_slot(artifact);
        let told = self.seen_artifacts.insert(slot.clone(), content.clone());
        (told != Some(content)).then_some(slot)
    }

    fn artifact_event(&self, artifact: Artifact) -> TaskEvent {
        TaskEvent::Artifact(TaskArtifactUpdateEvent {
            task_id: self.task_id.clone(),
            context_id: self.context_id.clone(),
            artifact,
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
                id,
                name,
                mime_type,
                data,
                file,
            } => {
                let mut artifact = ArtifactRef::new(name, mime_type, data);
                artifact.file = file.map(|file| FileRef::inline(file.filename, file.bytes));
                artifact.id = id;
                match self.admit(&artifact) {
                    Some(id) => crate::convert::artifact_of_with(&artifact, id, None)
                        .map(|served| vec![self.artifact_event(served)])
                        .unwrap_or_default(),
                    None => Vec::new(),
                }
            }
            RunEvent::Usage(call) => {
                if !self.usage {
                    return Vec::new();
                }
                vec![usage::call_report(&self.task_id, &self.context_id, &call)]
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

/// The subscription: snapshot, then live events and polls until the task ends
/// or waits for its caller. `Ok(())` also when the client went away.
async fn pump(
    backend: &RuntimeTaskBackend,
    caller: &Caller,
    task_id: &str,
    queue: &Queue,
) -> Result<(), BackendError> {
    // Attach to live events first, so nothing that happens between the
    // snapshot and the first poll is missed by both. The subscription also starts
    // with the run's recent events, which covers the gap between `submit` and here.
    let Some((run, view, context_id)) = backend.locate(caller, task_id).await? else {
        return Err(BackendError::TaskNotFound(task_id.to_owned()));
    };
    let mut live: Option<RunSubscription> = Some(backend.events.subscribe_run(run));
    // Re-read after attaching: the snapshot must not predate the attach.
    let view = match backend.locate(caller, task_id).await? {
        Some((_, fresher, _)) => fresher,
        None => view,
    };

    let told = backend.tier_said_to(caller, &view);
    let task = backend.said(caller, &view, backend.task_of(&view, &context_id).await);
    // The sessions are read before the task says them, so one that starts in between is told twice
    // (here and as an event) and never missed.
    let kind = kind_of(&task.context_id);
    let mut sessions = None;
    if backend.sessions_activated(caller)
        && matches!(kind, ContextKind::Root)
        && !waits(&task.status.state)
    {
        sessions = SessionsTracker::seed(backend, caller, &task.context_id, view.version).await;
    }
    let task = backend.with_sessions(caller, task).await?;
    let mut round_stream = None;
    if backend.sessions_activated(caller)
        && matches!(kind, ContextKind::Child(..))
        && !waits(&task.status.state)
        && let Some(round) = backend
            .round_of(caller, task_id)
            .await
            .inspect_err(|e| {
                tracing::warn!(error = %e, "the round could not be read: its stream carries no session object");
            })
            .ok()
            .flatten()
    {
        let closing = task
            .metadata
            .as_ref()
            .and_then(|m| m.get(CHILD_SESSIONS_EXTENSION))
            .is_some_and(|o| o["closing"] == Value::Bool(true));
        round_stream = Some(RoundStream {
            round,
            closing_told: closing,
            checked_at: Instant::now(),
            version: view.version,
        });
    }
    let mut tracker = Tracker::new(&task, context_id, &view, caller, told);
    let ends = task.status.state.is_terminal()
        || matches!(
            task.status.state,
            TaskState::InputRequired | TaskState::AuthRequired
        );
    if queue.send(vec![TaskEvent::Snapshot(task)]).await.is_none() || ends {
        return Ok(());
    }

    let mut ticker = tokio::time::interval(backend.poll);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut failures = 0u32;
    loop {
        let mut poll = false;
        tokio::select! {
            () = queue.tx.closed() => return Ok(()),
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
                    if queue.send(events).await.is_none() {
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
        let mut events = tracker.sync(&view, backend).await;
        let ends = events.iter().any(TaskEvent::ends_stream);
        // The sessions are listed once more before the final event, so nothing that happened
        // since the last listing is left unsaid.
        if let Some(sessions) = sessions.as_mut()
            && (ends || sessions.due(view.version))
        {
            let mut said = sessions
                .refresh(
                    backend,
                    caller,
                    &tracker.task_id,
                    &tracker.context_id,
                    view.version,
                    &tracker.open_status,
                )
                .await;
            said.append(&mut events);
            events = said;
        }
        if let Some(stream) = round_stream.as_mut()
            && (ends || stream.due(view.version))
        {
            stream.checked_at = Instant::now();
            stream.version = view.version;
            if let Some(object) = backend.round_status(caller, &stream.round).await {
                let closing = object["closing"] == Value::Bool(true);
                let turned = closing && !stream.closing_told;
                stream.closing_told = closing;
                if ends {
                    let last = events.iter_mut().rev().find_map(|e| match e {
                        TaskEvent::Status(update) if waits(&update.status.state) => Some(update),
                        _ => None,
                    });
                    if let Some(update) = last {
                        update
                            .metadata
                            .get_or_insert_with(HashMap::new)
                            .insert(CHILD_SESSIONS_EXTENSION.to_owned(), object);
                    }
                } else if turned {
                    events.push(TaskEvent::Status(TaskStatusUpdateEvent {
                        task_id: tracker.task_id.clone(),
                        context_id: tracker.context_id.clone(),
                        status: tracker.open_status.clone(),
                        metadata: Some(HashMap::from([(
                            CHILD_SESSIONS_EXTENSION.to_owned(),
                            object,
                        )])),
                    }));
                }
            }
        }
        if ends {
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
        match queue.send(events).await {
            None | Some(true) => return Ok(()),
            Some(false) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use adam_core::{DynStore, MemoryStore, RunStatus};
    use adam_runtime::{ArtifactRef, BroadcastSink, Runtime, StepKind};
    use serde_json::json;

    use super::*;
    use crate::convert::{artifact_id, work};

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

    /// A backend over an in-memory store: `sync` reads nothing but its `prompt` and its tiers.
    fn backend() -> RuntimeTaskBackend {
        let store: DynStore = std::sync::Arc::new(MemoryStore::new());
        RuntimeTaskBackend::new(Runtime::builder(store).build(), BroadcastSink::new(8), "a")
    }

    fn view(version: u64, artifacts: Vec<ArtifactRef>) -> RunView {
        RunView {
            id: adam_core::RunId::new(),
            agent: "a".into(),
            conversation_id: Some("s:c".into()),
            status: RunStatus::Runnable,
            wake_at: None,
            waiting: false,
            claimed: false,
            output: None,
            error: None,
            attempt: 0,
            pending_inbox: 0,
            artifacts,
            state: json!({}),
            version,
            created_at: adam_core::store::now(),
            updated_at: adam_core::store::now(),
        }
    }

    async fn tracker(view: &RunView, backend: &RuntimeTaskBackend) -> Tracker {
        let task = backend.task_of(view, "ctx").await;
        Tracker::new(&task, "ctx".into(), view, &Caller::new("t"), None)
    }

    /// The ids of the artifacts a sync said.
    fn said(events: &[TaskEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                TaskEvent::Artifact(update) => Some(update.artifact.artifact_id.clone()),
                _ => None,
            })
            .collect()
    }

    /// A poll that reads the run it already read does no artifact work at all: no digest, no
    /// serialisation. The first sync of a changed run does both, so a counter that never counted
    /// cannot make the negative assertion green.
    #[tokio::test]
    async fn a_poll_of_an_unchanged_run_derives_nothing() {
        let backend = backend();
        let first = view(
            2,
            vec![
                ArtifactRef::file("logo", "image/svg+xml", "logo.svg", vec![7u8; 4096]).unwrap(),
                ArtifactRef::new("plan", Some("application/json".into()), json!({"v": 1}))
                    .with_id("plan-x"),
            ],
        );
        let mut tracker = tracker(&view(1, Vec::new()), &backend).await;

        let before = work::counts();
        let events = tracker.sync(&first, &backend).await;
        let after = work::counts();
        assert!(
            after.0 > before.0 && after.1 > before.1,
            "the first sync derives the artifacts: {before:?} -> {after:?}"
        );
        assert_eq!(said(&events).len(), 2, "{events:?}");

        // The same read again: nothing is asked about, nothing is derived and nothing is said.
        let asked = work::artifacts_asked();
        let events = tracker.sync(&first, &backend).await;
        assert!(events.is_empty(), "{events:?}");
        assert_eq!(work::counts(), after, "an unchanged poll derives nothing");
        assert_eq!(
            work::artifacts_asked(),
            asked,
            "an unchanged poll asks about no artifact"
        );

        // A run that moved (another turn committed) whose artifacts are what they were: every
        // artifact is asked about, and none is derived again.
        let mut later = first.clone();
        later.version = 3;
        let asked = work::artifacts_asked();
        let events = tracker.sync(&later, &backend).await;
        assert!(events.is_empty(), "{events:?}");
        assert_eq!(
            work::artifacts_asked() - asked,
            2,
            "both artifacts are asked about"
        );
        assert_eq!(
            work::counts(),
            after,
            "no artifact changed, so none is derived"
        );
    }

    /// A changed run derives only the artifacts it added or replaced, never the ones the last poll
    /// already mapped.
    #[tokio::test]
    async fn a_changed_run_derives_only_what_it_added_or_replaced() {
        let backend = backend();
        let first = view(
            2,
            vec![
                ArtifactRef::file("logo", "image/svg+xml", "logo.svg", vec![7u8; 4096]).unwrap(),
                ArtifactRef::new("plan", None, json!({"v": 1})).with_id("plan-x"),
            ],
        );
        let mut tracker = tracker(&first, &backend).await;

        // One artifact appended: it alone is derived.
        let mut more = first.clone();
        more.version = 3;
        more.artifacts
            .push(ArtifactRef::new("report", None, json!({"done": true})));
        let before = work::counts();
        let events = tracker.sync(&more, &backend).await;
        let after = work::counts();
        assert_eq!(
            after.0 - before.0,
            1,
            "one digest for the one artifact added"
        );
        assert_eq!(after.1 - before.1, 1, "one serialisation for it");
        assert_eq!(said(&events), vec![artifact_id(&more.artifacts[2])]);

        // One artifact replaced where it stands: it alone is derived again.
        let mut replaced = more.clone();
        replaced.version = 4;
        replaced.artifacts[1] = ArtifactRef::new("plan", None, json!({"v": 2})).with_id("plan-x");
        let before = work::counts();
        let events = tracker.sync(&replaced, &backend).await;
        let after = work::counts();
        assert_eq!(
            after.0 - before.0,
            1,
            "one digest for the one artifact replaced"
        );
        assert_eq!(after.1 - before.1, 1, "one serialisation for it");
        assert_eq!(said(&events), vec!["plan-x".to_owned()]);

        // A replaceable artifact renamed in place is said again: the cache compares the name, not
        // only the data, so a rename is not swallowed.
        let mut renamed = replaced.clone();
        renamed.version = 5;
        renamed.artifacts[1] =
            ArtifactRef::new("plan-renamed", None, json!({"v": 2})).with_id("plan-x");
        let events = tracker.sync(&renamed, &backend).await;
        assert_eq!(
            said(&events),
            vec!["plan-x".to_owned()],
            "a rename is said again"
        );
    }

    /// An artifact the run changed is derived again: one without an id gets a new content-derived
    /// id, and one with an id is said again under that id.
    #[tokio::test]
    async fn a_changed_artifact_gets_a_new_id() {
        let backend = backend();
        let report = ArtifactRef::new("report", None, json!({"v": 1}));
        let plan = ArtifactRef::new("plan", None, json!({"v": 1})).with_id("plan-x");
        let first = view(2, vec![report.clone(), plan]);
        let was = artifact_id(&report);
        let mut tracker = tracker(&first, &backend).await;

        let mut changed = first.clone();
        changed.version = 3;
        changed.artifacts[0] = ArtifactRef::new("report", None, json!({"v": 2}));
        changed.artifacts[1] = ArtifactRef::new("plan", None, json!({"v": 2})).with_id("plan-x");
        let events = tracker.sync(&changed, &backend).await;

        assert_eq!(
            said(&events),
            vec![artifact_id(&changed.artifacts[0]), "plan-x".to_owned()],
            "both changed, both said: {events:?}"
        );
        assert_ne!(said(&events)[0], was, "new content, a new id");
    }
}

#[cfg(test)]
mod queue_tests {
    //! The queue between the pump and one subscriber: bounded by events and by payload bytes, and
    //! what a slow subscriber loses (an artifact, never the state that ends the stream).
    use futures::FutureExt as _;

    use super::*;

    fn artifact(id: &str, bytes: usize) -> TaskEvent {
        TaskEvent::Artifact(TaskArtifactUpdateEvent {
            task_id: "t".into(),
            context_id: "c".into(),
            artifact: Artifact {
                artifact_id: id.into(),
                name: Some("file".into()),
                description: None,
                parts: vec![Part::raw(vec![7; bytes])],
                metadata: None,
                extensions: None,
            },
            append: None,
            last_chunk: Some(true),
            metadata: None,
        })
    }

    fn completed() -> TaskEvent {
        TaskEvent::Status(TaskStatusUpdateEvent {
            task_id: "t".into(),
            context_id: "c".into(),
            status: TaskStatus {
                state: TaskState::Completed,
                message: None,
                timestamp: None,
            },
            metadata: None,
        })
    }

    /// A subscriber that stops reading while 64 artifacts of 1 MiB are emitted holds at most
    /// [`CHANNEL_BYTES`] of them, and still hears the status that ends the stream.
    #[tokio::test]
    async fn a_subscriber_that_does_not_read_holds_at_most_the_byte_bound() {
        let (queue, mut rx) = Queue::new();
        for n in 0..64 {
            assert!(
                queue
                    .send(vec![artifact(&format!("a{n}"), 1024 * 1024)])
                    .await
                    .is_some()
            );
        }
        let held = queue.queued_bytes();
        assert!(held <= CHANNEL_BYTES, "{held} bytes queued");
        let mut queued = Vec::new();
        while let Ok((item, _permit)) = rx.try_recv() {
            queued.push(item.expect("an event"));
        }
        // The queue is used (an artifact was kept) and bounded (not all 64 were).
        assert!(queued.len() >= 6, "{} artifacts queued", queued.len());
        assert!(queued.len() < 64, "{} of 64 artifacts queued", queued.len());
        assert!(
            queued
                .iter()
                .all(|event| matches!(event, TaskEvent::Artifact(_)))
        );
        // What ends the stream is not dropped, even when artifacts were.
        assert!(queue.send(vec![completed()]).await.is_some());
        let (last, _permit) = rx.try_recv().expect("the status is queued");
        assert!(matches!(last, Ok(TaskEvent::Status(_))));
    }

    /// A subscriber that reads gets every artifact, in order.
    #[tokio::test]
    async fn a_reading_subscriber_receives_every_artifact_in_order() {
        let (queue, mut rx) = Queue::new();
        for n in 0..64 {
            assert!(
                queue
                    .send(vec![artifact(&format!("a{n}"), 4 * 1024)])
                    .await
                    .is_some()
            );
        }
        let mut ids = Vec::new();
        while let Ok((item, _permit)) = rx.try_recv() {
            match item.expect("an event") {
                TaskEvent::Artifact(update) => ids.push(update.artifact.artifact_id),
                other => panic!("unexpected {other:?}"),
            }
        }
        let expected: Vec<String> = (0..64).map(|n| format!("a{n}")).collect();
        assert_eq!(ids, expected);
    }

    /// The state that ends the stream waits for room rather than being dropped.
    #[tokio::test]
    async fn what_ends_the_stream_waits_for_room_instead_of_being_dropped() {
        let (queue, mut rx) = Queue::new();
        // One artifact of the whole budget: nothing is left for the status behind it.
        assert!(
            queue
                .send(vec![artifact("big", CHANNEL_BYTES)])
                .await
                .is_some()
        );
        assert_eq!(queue.queued_bytes(), CHANNEL_BYTES);
        let mut status = Box::pin(queue.send(vec![completed()]));
        assert!(
            status.as_mut().now_or_never().is_none(),
            "the status waits for room"
        );
        let _ = rx.recv().await.expect("the artifact");
        assert!(status.await.is_some(), "then it is queued");
        let (last, _permit) = rx.recv().await.expect("the status");
        assert!(matches!(last, Ok(TaskEvent::Status(_))));
    }

    /// A chunk of a stream is transient: it is never in a `GetTask` and is never replayed from the
    /// record, so it is never dropped, even when the budget is exhausted. A durable artifact that
    /// would have fitted an empty queue may be dropped: `GetTask` (or a resubscription) has it.
    #[tokio::test]
    async fn a_transient_chunk_waits_for_room_and_a_durable_artifact_may_be_dropped() {
        let (queue, mut rx) = Queue::new();
        // One artifact takes the whole budget: nothing else fits until it is read.
        assert!(
            queue
                .send(vec![artifact("big", CHANNEL_BYTES)])
                .await
                .is_some()
        );
        assert_eq!(queue.queued_bytes(), CHANNEL_BYTES);

        let piece = TaskEvent::Artifact(
            text_stream::chunk("t", "c", "stream", 0, "hello".into(), false, false)
                .expect("a chunk"),
        );
        let mut sending = Box::pin(queue.send(vec![piece]));
        assert!(
            sending.as_mut().now_or_never().is_none(),
            "the chunk waits for room"
        );

        // A durable artifact of the same task is dropped: the task still has it.
        let before = queue.queued_bytes();
        assert!(queue.send(vec![artifact("small", 1024)]).await.is_some());
        assert_eq!(
            queue.queued_bytes(),
            before,
            "the durable artifact was dropped"
        );

        // The big artifact is read, the budget frees, and the chunk arrives.
        let _ = rx.recv().await.expect("the big artifact");
        assert!(sending.await.is_some(), "the chunk is queued");
        let (item, _permit) = rx.recv().await.expect("the chunk");
        match item.expect("an event") {
            TaskEvent::Artifact(update) => assert_eq!(update.artifact.artifact_id, "stream"),
            other => panic!("unexpected {other:?}"),
        }
        assert!(rx.try_recv().is_err(), "the dropped artifact is not queued");
    }

    /// An artifact larger than the whole budget could not have fitted even an empty queue, so it is
    /// never dropped: it waits for the queue to drain and then arrives.
    #[tokio::test]
    async fn an_over_budget_artifact_waits_for_the_queue_to_drain() {
        let (queue, mut rx) = Queue::new();
        // A status in flight holds some permits, so the artifact does not fit at once.
        assert!(queue.send(vec![completed()]).await.is_some());
        assert!(queue.queued_bytes() > 0);

        let mut sending = Box::pin(queue.send(vec![artifact("huge", CHANNEL_BYTES + 1)]));
        assert!(
            sending.as_mut().now_or_never().is_none(),
            "the artifact waits instead of being dropped"
        );

        // The status is read; the artifact takes the whole budget and arrives.
        let _ = rx.recv().await.expect("the status");
        assert!(sending.await.is_some(), "the artifact is queued");
        let (item, _permit) = rx.recv().await.expect("the artifact");
        match item.expect("an event") {
            TaskEvent::Artifact(update) => assert_eq!(update.artifact.artifact_id, "huge"),
            other => panic!("unexpected {other:?}"),
        }
    }
}
