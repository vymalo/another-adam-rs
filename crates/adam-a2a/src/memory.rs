//! [`InMemoryBackend`]: the reference [`TaskBackend`], for tests.
//!
//! It defines the semantics a durable backend must reproduce: a per-task
//! record (the folded event history) plus live subscribers, snapshot-first
//! subscriptions, ownership by caller, resumable `input-required`.
//!
//! Message text drives behaviour:
//!
//! | Marker in the text | Effect |
//! |---|---|
//! | (none) | `working`, an artifact `echo: <text>`, `completed` |
//! | `[input-required]` | `working`, then `input-required`; a follow-up with the same task id resumes to `completed` |
//! | `[hold]` | `working`, then blocks until [`InMemoryBackend::release`] (or cancel) |

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use a2a::{
    Artifact, Message, Part, Role, Task, TaskArtifactUpdateEvent, TaskState, TaskStatus,
    TaskStatusUpdateEvent,
};
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use tokio::sync::{Notify, mpsc};
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::backend::{BackendError, Caller, TaskBackend, TaskEvent, state_ends_stream};
use crate::page::{PageToken, TaskPage, TaskQuery};

/// Marker that sends a task to `input-required` after it starts working.
pub const INPUT_REQUIRED_MARKER: &str = "[input-required]";
/// Marker that keeps a task `working` until [`InMemoryBackend::release`].
pub const HOLD_MARKER: &str = "[hold]";

/// Tuning for [`InMemoryBackend`].
#[derive(Clone, Debug)]
pub struct InMemoryConfig {
    /// Pause before each step (start, artifact, completion). Leaves a
    /// streaming client time to subscribe before the work finishes, which
    /// keeps stream-order tests deterministic.
    pub step_delay: Duration,
}

impl Default for InMemoryConfig {
    fn default() -> Self {
        Self {
            step_delay: Duration::from_millis(25),
        }
    }
}

/// A [`TaskBackend`] that keeps everything in process memory.
#[derive(Clone)]
pub struct InMemoryBackend {
    inner: Arc<Inner>,
}

struct Inner {
    config: InMemoryConfig,
    tasks: Mutex<HashMap<String, Entry>>,
}

struct Entry {
    owner: String,
    task: Task,
    subscribers: Vec<mpsc::UnboundedSender<TaskEvent>>,
    gate: Arc<Notify>,
}

impl Default for InMemoryBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for InMemoryBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemoryBackend").finish_non_exhaustive()
    }
}

impl InMemoryBackend {
    /// A backend with the default [`InMemoryConfig`].
    pub fn new() -> Self {
        Self::with_config(InMemoryConfig::default())
    }

    /// A backend with explicit tuning.
    pub fn with_config(config: InMemoryConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                tasks: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Ids of every task held, in no particular order.
    pub fn task_ids(&self) -> Vec<String> {
        self.inner.lock().keys().cloned().collect()
    }

    /// Let a task held by the `[hold]` marker continue. Returns whether the
    /// task exists. Releasing before the task reaches its gate is remembered.
    pub fn release(&self, task_id: &str) -> bool {
        match self.inner.lock().get(task_id) {
            Some(entry) => {
                entry.gate.notify_one();
                true
            }
            None => false,
        }
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Entry>> {
        // A poisoned lock only means a test panicked mid-update; the map is
        // still structurally valid.
        self.tasks.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Set a task's status and tell subscribers. `false` if the task is gone
    /// or already terminal (e.g. canceled while the pipeline slept), which
    /// tells the pipeline to stop.
    fn set_status(&self, task_id: &str, state: TaskState, text: Option<&str>) -> bool {
        let mut tasks = self.lock();
        let Some(entry) = tasks.get_mut(task_id) else {
            return false;
        };
        if entry.task.status.state.is_terminal() {
            return false;
        }
        entry.apply_status(state, text);
        true
    }

    fn add_artifact(&self, task_id: &str, text: String) -> bool {
        let mut tasks = self.lock();
        let Some(entry) = tasks.get_mut(task_id) else {
            return false;
        };
        if entry.task.status.state.is_terminal() {
            return false;
        }
        let artifact = Artifact {
            artifact_id: a2a::new_artifact_id(),
            name: Some("echo".to_owned()),
            description: None,
            parts: vec![Part::text(text)],
            metadata: None,
            extensions: None,
        };
        entry
            .task
            .artifacts
            .get_or_insert_with(Vec::new)
            .push(artifact.clone());
        let event = TaskEvent::Artifact(TaskArtifactUpdateEvent {
            task_id: entry.task.id.clone(),
            context_id: entry.task.context_id.clone(),
            artifact,
            append: None,
            last_chunk: Some(true),
            metadata: None,
        });
        entry.broadcast(event);
        true
    }

    fn is_working(&self, task_id: &str) -> bool {
        self.lock()
            .get(task_id)
            .is_some_and(|e| e.task.status.state == TaskState::Working)
    }

    fn gate(&self, task_id: &str) -> Option<Arc<Notify>> {
        self.lock().get(task_id).map(|e| e.gate.clone())
    }
}

impl Entry {
    fn apply_status(&mut self, state: TaskState, text: Option<&str>) {
        let status = TaskStatus {
            state,
            message: text.map(|t| Message::new(Role::Agent, vec![Part::text(t)])),
            timestamp: Some(chrono::Utc::now()),
        };
        self.task.status = status.clone();
        let event = TaskEvent::Status(TaskStatusUpdateEvent {
            task_id: self.task.id.clone(),
            context_id: self.task.context_id.clone(),
            status,
            metadata: None,
        });
        self.broadcast(event);
    }

    /// Deliver to live subscribers; when the event leaves the task terminal or
    /// interrupted, close their streams.
    fn broadcast(&mut self, event: TaskEvent) {
        let ends = event.ends_stream();
        self.subscribers.retain(|tx| tx.send(event.clone()).is_ok());
        if ends {
            self.subscribers.clear();
        }
    }
}

/// The scripted agent: runs in its own task, independent of any subscriber.
async fn run_pipeline(inner: Arc<Inner>, task_id: String, text: String, resuming: bool) {
    let delay = inner.config.step_delay;
    tokio::time::sleep(delay).await;
    // A follow-up was already moved to `working` by `submit`.
    if !resuming && !inner.set_status(&task_id, TaskState::Working, None) {
        return;
    }

    if text.contains(HOLD_MARKER) && !resuming {
        let Some(gate) = inner.gate(&task_id) else {
            return;
        };
        gate.notified().await;
        if !inner.is_working(&task_id) {
            return; // canceled while held
        }
    }

    tokio::time::sleep(delay).await;
    if text.contains(INPUT_REQUIRED_MARKER) && !resuming {
        inner.set_status(
            &task_id,
            TaskState::InputRequired,
            Some("more input required"),
        );
        return;
    }

    if !inner.add_artifact(&task_id, format!("echo: {text}")) {
        return;
    }
    tokio::time::sleep(delay).await;
    inner.set_status(&task_id, TaskState::Completed, None);
}

#[async_trait]
impl TaskBackend for InMemoryBackend {
    #[tracing::instrument(skip_all, fields(subject = %caller.subject))]
    async fn submit(
        &self,
        caller: Caller,
        message: Message,
        task_id: Option<String>,
        context_id: Option<String>,
    ) -> Result<Task, BackendError> {
        let text = message.text().unwrap_or_default().to_owned();
        let (task, resuming) = {
            let mut tasks = self.inner.lock();
            match task_id {
                Some(id) => {
                    let entry = tasks
                        .get_mut(&id)
                        .filter(|e| e.owner == caller.subject)
                        .ok_or_else(|| BackendError::TaskNotFound(id.clone()))?;
                    if context_id.is_some_and(|c| c != entry.task.context_id) {
                        return Err(BackendError::InvalidParams(
                            "contextId does not match the task".to_owned(),
                        ));
                    }
                    if entry.task.status.state.is_terminal() {
                        return Err(BackendError::UnsupportedOperation(format!(
                            "task {id} is {:?} and cannot take a message",
                            entry.task.status.state
                        )));
                    }
                    if entry.task.status.state != TaskState::InputRequired {
                        return Err(BackendError::InvalidParams(format!(
                            "task {id} is {:?} and cannot take a follow-up",
                            entry.task.status.state
                        )));
                    }
                    entry
                        .task
                        .history
                        .get_or_insert_with(Vec::new)
                        .push(message);
                    entry.apply_status(TaskState::Working, None);
                    (entry.task.clone(), true)
                }
                None => {
                    let id = a2a::new_task_id();
                    let task = Task {
                        id: id.clone(),
                        context_id: context_id.unwrap_or_else(a2a::new_context_id),
                        status: TaskStatus {
                            state: TaskState::Submitted,
                            message: None,
                            timestamp: Some(chrono::Utc::now()),
                        },
                        artifacts: None,
                        history: Some(vec![message]),
                        metadata: None,
                    };
                    tasks.insert(
                        id,
                        Entry {
                            owner: caller.subject.clone(),
                            task: task.clone(),
                            subscribers: Vec::new(),
                            gate: Arc::new(Notify::new()),
                        },
                    );
                    (task, false)
                }
            }
        };

        tokio::spawn(run_pipeline(
            self.inner.clone(),
            task.id.clone(),
            text,
            resuming,
        ));
        Ok(task)
    }

    async fn get(&self, caller: &Caller, task_id: &str) -> Result<Option<Task>, BackendError> {
        Ok(self
            .inner
            .lock()
            .get(task_id)
            .filter(|e| e.owner == caller.subject)
            .map(|e| e.task.clone()))
    }

    #[tracing::instrument(skip(self, caller), fields(subject = %caller.subject))]
    async fn cancel(&self, caller: &Caller, task_id: &str) -> Result<Task, BackendError> {
        let mut tasks = self.inner.lock();
        let entry = tasks
            .get_mut(task_id)
            .filter(|e| e.owner == caller.subject)
            .ok_or_else(|| BackendError::TaskNotFound(task_id.to_owned()))?;
        match &entry.task.status.state {
            TaskState::Canceled => {}
            state if state.is_terminal() => {
                return Err(BackendError::NotCancelable {
                    task_id: task_id.to_owned(),
                    state: format!("{state:?}"),
                });
            }
            _ => {
                entry.apply_status(TaskState::Canceled, Some("canceled"));
                entry.gate.notify_one();
            }
        }
        Ok(entry.task.clone())
    }

    async fn list(&self, caller: &Caller, query: &TaskQuery) -> Result<TaskPage, BackendError> {
        let after = query
            .page_token
            .as_deref()
            .map(|t| PageToken::decode(t, caller, query))
            .transpose()?;
        // Last update, in milliseconds: the order, and the position a token stands for.
        let key = |task: &Task| {
            (
                task.status.timestamp.map_or(0, |t| t.timestamp_millis()),
                task.id.clone(),
            )
        };
        let mut matching: Vec<Task> = self
            .inner
            .lock()
            .values()
            .filter(|e| e.owner == caller.subject)
            .map(|e| &e.task)
            .filter(|t| query.context_id.as_ref().is_none_or(|c| &t.context_id == c))
            .filter(|t| query.status.as_ref().is_none_or(|s| &t.status.state == s))
            .filter(|t| {
                query
                    .status_timestamp_after
                    .is_none_or(|after| t.status.timestamp.is_some_and(|ts| ts >= after))
            })
            .cloned()
            .collect();
        matching.sort_by_key(|t| std::cmp::Reverse(key(t)));
        let total_size = matching.len();
        if let Some(after) = &after {
            let position = (after.updated_at.timestamp_millis(), after.id.clone());
            matching.retain(|t| key(t) < position);
        }
        let more = matching.len() > query.page_size;
        matching.truncate(query.page_size);
        let next = more.then(|| matching.last()).flatten().and_then(|last| {
            last.status
                .timestamp
                .map(|ts| PageToken::encode(caller, query, ts, &last.id))
        });
        for task in &mut matching {
            task.history = None;
        }
        Ok(TaskPage::new(matching, next, total_size))
    }

    fn subscribe(
        &self,
        caller: &Caller,
        task_id: &str,
    ) -> BoxStream<'static, Result<TaskEvent, BackendError>> {
        let mut tasks = self.inner.lock();
        let Some(entry) = tasks.get_mut(task_id).filter(|e| e.owner == caller.subject) else {
            let err = BackendError::TaskNotFound(task_id.to_owned());
            return futures::stream::once(async move { Err(err) }).boxed();
        };

        let snapshot = TaskEvent::Snapshot(entry.task.clone());
        if state_ends_stream(&entry.task.status.state) {
            return futures::stream::once(async move { Ok(snapshot) }).boxed();
        }
        let (tx, rx) = mpsc::unbounded_channel();
        entry.subscribers.push(tx);
        futures::stream::once(async move { Ok(snapshot) })
            .chain(UnboundedReceiverStream::new(rx).map(Ok))
            .boxed()
    }
}
