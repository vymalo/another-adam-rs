//! The SDK's [`RequestHandler`], implemented directly on [`TaskBackend`].
//!
//! The SDK's `DefaultRequestHandler` keeps live executions in process memory,
//! so `SubscribeToTask` only works in the process that started the task. This
//! handler holds no task state at all: `SubscribeToTask` delegates to
//! [`TaskBackend::subscribe`], which a durable backend serves from shared
//! storage, on any replica, across restarts.

use std::sync::Arc;

use a2a::{
    A2AError, AgentCard, CancelTaskRequest, DeleteTaskPushNotificationConfigRequest,
    GetExtendedAgentCardRequest, GetTaskPushNotificationConfigRequest, GetTaskRequest,
    ListTaskPushNotificationConfigsRequest, ListTaskPushNotificationConfigsResponse,
    ListTasksRequest, ListTasksResponse, SendMessageRequest, SendMessageResponse, StreamResponse,
    SubscribeToTaskRequest, Task, TaskPushNotificationConfig,
};
use a2a_server::{RequestHandler, ServiceParams};
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::{StreamExt, future};

use crate::activation::{self, HEADER};
use crate::auth::CALLER_HEADER;
use crate::backend::{BackendError, Caller, DynTaskBackend, TaskEvent};

/// Serves the A2A 1.0 JSON-RPC methods on top of a [`TaskBackend`](crate::TaskBackend).
pub(crate) struct BackendHandler {
    backend: DynTaskBackend,
    /// The URIs the card declares: what a request may activate.
    declared: Vec<String>,
}

impl BackendHandler {
    pub(crate) fn new(backend: DynTaskBackend, declared: Vec<String>) -> Arc<Self> {
        Arc::new(Self { backend, declared })
    }

    /// The caller of this request: who the auth middleware vouched for, with the extensions the
    /// request activated (`message` is the extensions a message it sends names).
    fn caller(&self, params: &ServiceParams, message: &[String]) -> Result<Caller, A2AError> {
        let caller = caller(params)?;
        let header = params.get(HEADER).into_iter().flatten().map(String::as_str);
        let extensions =
            activation::activated(&self.declared, header, message.iter().map(String::as_str));
        Ok(caller.with_extensions(extensions))
    }
}

/// The caller the auth middleware vouched for. Missing or ambiguous identity
/// means the middleware did not run: refuse rather than guess.
fn caller(params: &ServiceParams) -> Result<Caller, A2AError> {
    match params.get(CALLER_HEADER.as_str()).map(Vec::as_slice) {
        Some([subject]) if !subject.is_empty() => Ok(Caller::new(subject.clone())),
        _ => {
            tracing::error!("request reached the handler without a trusted caller identity");
            Err(A2AError::internal("internal error"))
        }
    }
}

/// Keep only the last `history_length` messages (`None` keeps everything,
/// `0` drops the history).
fn trim_history(mut task: Task, history_length: Option<i32>) -> Task {
    if let (Some(limit), Some(history)) = (history_length, task.history.as_mut()) {
        let limit = usize::try_from(limit).unwrap_or(0);
        if history.len() > limit {
            history.drain(..history.len() - limit);
        }
    }
    task
}

fn to_stream_response(event: TaskEvent) -> StreamResponse {
    match event {
        TaskEvent::Snapshot(task) => StreamResponse::Task(task),
        TaskEvent::Status(update) => StreamResponse::StatusUpdate(update),
        TaskEvent::Artifact(update) => StreamResponse::ArtifactUpdate(update),
    }
}

/// Map a backend event stream to SDK stream items, ending it after the first
/// event that leaves the task terminal or waiting on its caller, and after the
/// first error.
fn a2a_stream(
    events: BoxStream<'static, Result<TaskEvent, BackendError>>,
) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
    Box::pin(events.scan(false, |finished, item| {
        if *finished {
            return future::ready(None);
        }
        let mapped = match item {
            Ok(event) => {
                *finished = event.ends_stream();
                Ok(to_stream_response(event))
            }
            Err(err) => {
                *finished = true;
                Err(A2AError::from(err))
            }
        };
        future::ready(Some(mapped))
    }))
}

/// The extensions the message of a send request names.
fn message_extensions(request: &SendMessageRequest) -> &[String] {
    request.message.extensions.as_deref().unwrap_or_default()
}

/// A message must say something, and only users send messages to agents.
fn validate(request: &SendMessageRequest) -> Result<(), A2AError> {
    if request.message.parts.is_empty() {
        return Err(A2AError::invalid_params("message.parts must not be empty"));
    }
    if request.message.role != a2a::Role::User {
        return Err(A2AError::invalid_params("message.role must be ROLE_USER"));
    }
    Ok(())
}

impl BackendHandler {
    async fn submit(&self, caller: Caller, request: SendMessageRequest) -> Result<Task, A2AError> {
        validate(&request)?;
        let message = request.message;
        let task_id = message.task_id.clone();
        let context_id = message.context_id.clone();
        Ok(self
            .backend
            .submit(caller, message, task_id, context_id)
            .await?)
    }
}

#[async_trait]
impl RequestHandler for BackendHandler {
    #[tracing::instrument(skip_all, fields(method = "SendMessage"))]
    async fn send_message(
        &self,
        params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError> {
        let caller = self.caller(params, message_extensions(&req))?;
        let config = req.configuration.clone();
        let history_length = config.as_ref().and_then(|c| c.history_length);
        let return_immediately = config
            .as_ref()
            .and_then(|c| c.return_immediately)
            .unwrap_or(false);

        let task = self.submit(caller.clone(), req).await?;
        if return_immediately {
            return Ok(SendMessageResponse::Task(trim_history(
                task,
                history_length,
            )));
        }

        // Block until the task is terminal or waiting on the caller. Dropping
        // this future (client went away) drops only the subscription.
        let mut events = a2a_stream(self.backend.subscribe(&caller, &task.id));
        while let Some(item) = events.next().await {
            item?;
        }
        let task = self
            .backend
            .get(&caller, &task.id)
            .await?
            .ok_or_else(|| A2AError::task_not_found(&task.id))?;
        Ok(SendMessageResponse::Task(trim_history(
            task,
            history_length,
        )))
    }

    #[tracing::instrument(skip_all, fields(method = "SendStreamingMessage"))]
    async fn send_streaming_message(
        &self,
        params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let caller = self.caller(params, message_extensions(&req))?;
        let task = self.submit(caller.clone(), req).await?;
        // The subscription opens with an atomic snapshot, so events between
        // `submit` returning and this call are folded into it, never lost.
        Ok(a2a_stream(self.backend.subscribe(&caller, &task.id)))
    }

    #[tracing::instrument(skip_all, fields(method = "GetTask", task_id = %req.id))]
    async fn get_task(
        &self,
        params: &ServiceParams,
        req: GetTaskRequest,
    ) -> Result<Task, A2AError> {
        let caller = self.caller(params, &[])?;
        let task = self
            .backend
            .get(&caller, &req.id)
            .await?
            .ok_or_else(|| A2AError::task_not_found(&req.id))?;
        Ok(trim_history(task, req.history_length))
    }

    async fn list_tasks(
        &self,
        params: &ServiceParams,
        _req: ListTasksRequest,
    ) -> Result<ListTasksResponse, A2AError> {
        caller(params)?;
        Err(A2AError::unsupported_operation(
            "ListTasks is not supported",
        ))
    }

    #[tracing::instrument(skip_all, fields(method = "CancelTask", task_id = %req.id))]
    async fn cancel_task(
        &self,
        params: &ServiceParams,
        req: CancelTaskRequest,
    ) -> Result<Task, A2AError> {
        let caller = self.caller(params, &[])?;
        Ok(self.backend.cancel(&caller, &req.id).await?)
    }

    #[tracing::instrument(skip_all, fields(method = "SubscribeToTask", task_id = %req.id))]
    async fn subscribe_to_task(
        &self,
        params: &ServiceParams,
        req: SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let caller = self.caller(params, &[])?;
        // Resolve existence and terminality up front so the client gets a
        // proper JSON-RPC error instead of an error frame inside a 200 stream.
        let task = self
            .backend
            .get(&caller, &req.id)
            .await?
            .ok_or_else(|| A2AError::task_not_found(&req.id))?;
        if task.status.state.is_terminal() {
            return Err(A2AError::unsupported_operation(format!(
                "task {} is already in a terminal state",
                req.id
            )));
        }
        Ok(a2a_stream(self.backend.subscribe(&caller, &req.id)))
    }

    async fn create_push_config(
        &self,
        params: &ServiceParams,
        _req: TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        caller(params)?;
        Err(A2AError::push_notification_not_supported())
    }

    async fn get_push_config(
        &self,
        params: &ServiceParams,
        _req: GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        caller(params)?;
        Err(A2AError::push_notification_not_supported())
    }

    async fn list_push_configs(
        &self,
        params: &ServiceParams,
        _req: ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
        caller(params)?;
        Err(A2AError::push_notification_not_supported())
    }

    async fn delete_push_config(
        &self,
        params: &ServiceParams,
        _req: DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2AError> {
        caller(params)?;
        Err(A2AError::push_notification_not_supported())
    }

    async fn get_extended_agent_card(
        &self,
        params: &ServiceParams,
        _req: GetExtendedAgentCardRequest,
    ) -> Result<AgentCard, A2AError> {
        caller(params)?;
        Err(A2AError::extended_card_not_configured())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2a::{Message, Part, Role};

    fn task_with_history(n: usize) -> Task {
        let mut task = Task {
            id: "t".into(),
            context_id: "c".into(),
            status: a2a::TaskStatus {
                state: a2a::TaskState::Working,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        };
        task.history = Some(
            (0..n)
                .map(|i| Message::new(Role::User, vec![Part::text(i.to_string())]))
                .collect(),
        );
        task
    }

    #[test]
    fn history_is_trimmed_to_the_most_recent_messages() {
        let kept = |limit| {
            trim_history(task_with_history(4), limit)
                .history
                .unwrap()
                .iter()
                .filter_map(|m| m.text().map(str::to_owned))
                .collect::<Vec<_>>()
        };
        assert_eq!(kept(None), ["0", "1", "2", "3"]);
        assert_eq!(kept(Some(2)), ["2", "3"]);
        assert!(kept(Some(0)).is_empty());
        assert!(kept(Some(-5)).is_empty());
        assert_eq!(kept(Some(10)).len(), 4);
    }

    #[test]
    fn missing_or_ambiguous_identity_is_refused() {
        let mut params = ServiceParams::new();
        assert!(caller(&params).is_err());
        params.insert(
            CALLER_HEADER.as_str().to_owned(),
            vec!["a".into(), "b".into()],
        );
        assert!(caller(&params).is_err());
        params.insert(CALLER_HEADER.as_str().to_owned(), vec!["token-0".into()]);
        assert_eq!(caller(&params).unwrap().subject, "token-0");
    }
}
