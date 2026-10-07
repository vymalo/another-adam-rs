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
    SubscribeToTaskRequest, Task, TaskPushNotificationConfig, TaskState,
};
use a2a_server::{RequestHandler, ServiceParams};
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::{StreamExt, future};

use crate::activation::{self, HEADER};
use crate::auth::CALLER_HEADER;
use crate::backend::{BackendError, Caller, DynTaskBackend, TaskEvent};
use crate::page::TaskQuery;
use crate::push::{
    MAX_CONFIG_ID_LEN, MAX_CONFIGS_PER_TASK, MAX_CREDENTIALS_LEN, MAX_TOKEN_LEN, NewPushConfig,
    PushCursor, PushStoreError, PushSupport,
};

/// Serves the A2A 1.0 JSON-RPC methods on top of a [`TaskBackend`](crate::TaskBackend).
pub(crate) struct BackendHandler {
    backend: DynTaskBackend,
    /// The URIs the card declares: what a request may activate.
    declared: Vec<String>,
    /// Push notifications, when the deployment turned them on (the policy allows a webhook).
    push: Option<PushSupport>,
    /// The extended agent card, when one is configured and the server authenticates.
    extended: Option<Arc<AgentCard>>,
}

impl BackendHandler {
    pub(crate) fn new(
        backend: DynTaskBackend,
        declared: Vec<String>,
        push: Option<PushSupport>,
        extended: Option<Arc<AgentCard>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            backend,
            declared,
            push,
            extended,
        })
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
    /// Submit the message, and register the push configuration it carries, if any.
    ///
    /// The configuration is checked **before** the task is created, so a refused webhook leaves
    /// nothing behind; it is registered **after**, against the task that came back, with what the
    /// task says now as the baseline (a message that was just sent is news to the webhook).
    async fn submit(&self, caller: Caller, request: SendMessageRequest) -> Result<Task, A2AError> {
        validate(&request)?;
        let inline = request
            .configuration
            .as_ref()
            .and_then(|c| c.task_push_notification_config.clone());
        if let Some(config) = &inline {
            self.validate_push_config(self.push_support()?, config)?;
        }
        let message = request.message;
        let task_id = message.task_id.clone();
        let context_id = message.context_id.clone();
        let task = self
            .backend
            .submit(caller.clone(), message, task_id, context_id)
            .await?;
        if let Some(config) = inline {
            self.register_inline(&caller, &task, config).await?;
        }
        Ok(task)
    }

    fn push_support(&self) -> Result<&PushSupport, A2AError> {
        self.push
            .as_ref()
            .ok_or_else(A2AError::push_notification_not_supported)
    }

    /// The task, if it is the caller's: another caller's task is not found, as for `GetTask`.
    async fn owned_task(&self, caller: &Caller, task_id: &str) -> Result<Task, A2AError> {
        if task_id.is_empty() {
            return Err(A2AError::invalid_params("taskId is required"));
        }
        self.backend
            .get(caller, task_id)
            .await?
            .ok_or_else(|| A2AError::task_not_found(task_id))
    }

    /// What a webhook configuration must be, before anything is stored.
    fn validate_push_config(
        &self,
        support: &PushSupport,
        config: &TaskPushNotificationConfig,
    ) -> Result<(), A2AError> {
        support
            .policy()
            .check(&config.url)
            .map_err(|refused| A2AError::invalid_params(refused.to_string()))?;
        if let Some(id) = config.id.as_deref().filter(|id| !id.is_empty()) {
            let ok = id.len() <= MAX_CONFIG_ID_LEN
                && id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'));
            if !ok {
                return Err(A2AError::invalid_params(format!(
                    "the push config id must be at most {MAX_CONFIG_ID_LEN} characters of letters, digits and - _ . :"
                )));
            }
        }
        // Everything the deliverer will put in a header must be a header value, now, so the
        // client hears about it instead of a delivery that fails for ever.
        let header_ok = |text: &str| reqwest::header::HeaderValue::from_str(text).is_ok();
        if let Some(token) = &config.token
            && (token.len() > MAX_TOKEN_LEN || !header_ok(token))
        {
            return Err(A2AError::invalid_params(
                "the push notification token must be a header value of at most 4096 characters",
            ));
        }
        if let Some(auth) = &config.authentication {
            let scheme_ok = !auth.scheme.is_empty()
                && auth.scheme.len() <= 64
                && auth
                    .scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c));
            if !scheme_ok {
                return Err(A2AError::invalid_params(
                    "the push authentication scheme must be an HTTP authentication scheme name",
                ));
            }
            if let Some(credentials) = &auth.credentials
                && (credentials.len() > MAX_CREDENTIALS_LEN || !header_ok(credentials))
            {
                return Err(A2AError::invalid_params(
                    "the push credentials must be a header value of at most 4096 characters",
                ));
            }
        }
        Ok(())
    }

    /// Store `config` for `task`, starting from `cursor`; the config as it is read back.
    async fn store_push_config(
        &self,
        caller: &Caller,
        task: &Task,
        mut config: TaskPushNotificationConfig,
        cursor: PushCursor,
        replace: bool,
    ) -> Result<Option<TaskPushNotificationConfig>, A2AError> {
        let support = self.push_support()?;
        let id = config
            .id
            .clone()
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let existing = support
            .store()
            .list(&task.id)
            .await
            .map_err(|e| push_error(e, &task.id))?;
        let present = existing.iter().any(|r| r.id == id);
        if present && !replace {
            return Ok(None);
        }
        if !present && existing.len() >= MAX_CONFIGS_PER_TASK {
            return Err(A2AError::invalid_params(format!(
                "a task may have at most {MAX_CONFIGS_PER_TASK} push notification configs"
            )));
        }
        config.id = Some(id.clone());
        config.task_id.clone_from(&task.id);
        config.tenant = None;
        let record = support
            .store()
            .put(NewPushConfig {
                task_id: task.id.clone(),
                id,
                owner: caller.subject.clone(),
                config,
                cursor,
            })
            .await
            .map_err(|e| push_error(e, &task.id))?;
        // A hint: the deliverer looks at the config now instead of at its next poll.
        support.nudge.notify_one();
        Ok(Some(redacted(record.config)))
    }

    /// The configuration of a `SendMessage` request, registered against the task it created.
    ///
    /// An id the task already has is left alone (a repeated request must not reset the cursor
    /// of the first), and an absent id is derived from the URL, so a repeated request does not
    /// add a second config for the same webhook.
    async fn register_inline(
        &self,
        caller: &Caller,
        task: &Task,
        mut config: TaskPushNotificationConfig,
    ) -> Result<(), A2AError> {
        if config.id.as_deref().is_none_or(str::is_empty) {
            config.id = Some(inline_id(&config.url));
        }
        self.store_push_config(caller, task, config, PushCursor::baseline(task), false)
            .await
            .map(|_| ())
    }
}

/// What a client may read back of a config: everything but the secrets it wrote (the token and
/// the credentials are write-only).
fn redacted(mut config: TaskPushNotificationConfig) -> TaskPushNotificationConfig {
    config.token = None;
    if let Some(auth) = &mut config.authentication {
        auth.credentials = None;
    }
    config
}

/// The id of a config a `SendMessage` request gave no id: stable for one URL.
fn inline_id(url: &str) -> String {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(url.as_bytes());
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("inline-{hex}")
}

/// A push-store failure, as the A2A error the client is told (the cause goes to the log).
fn push_error(err: PushStoreError, task_id: &str) -> A2AError {
    match err {
        PushStoreError::NotFound => A2AError::task_not_found(task_id),
        PushStoreError::Conflict => A2AError::from(BackendError::unavailable("push store busy")),
        PushStoreError::Unavailable(source) => A2AError::from(
            BackendError::unavailable("the push notification store is unavailable")
                .with_source(source),
        ),
        PushStoreError::Internal(source) => A2AError::from(
            BackendError::internal("the push notification store failed").with_source(source),
        ),
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

    #[tracing::instrument(skip_all, fields(method = "ListTasks"))]
    async fn list_tasks(
        &self,
        params: &ServiceParams,
        req: ListTasksRequest,
    ) -> Result<ListTasksResponse, A2AError> {
        let caller = self.caller(params, &[])?;
        let page_size = TaskQuery::resolve_page_size(req.page_size);
        let include_artifacts = req.include_artifacts.unwrap_or(false);
        let mut query = TaskQuery::new();
        query.context_id = req.context_id.filter(|c| !c.is_empty());
        query.status = req.status.filter(|s| *s != TaskState::Unspecified);
        query.status_timestamp_after = req.status_timestamp_after;
        query.page_size = page_size;
        query.page_token = req.page_token.filter(|t| !t.is_empty());
        query.include_artifacts = include_artifacts;
        let page = self.backend.list(&caller, &query).await?;
        let tasks = page
            .tasks
            .into_iter()
            .map(|mut task| {
                // The field is omitted entirely, not empty, when artifacts were not asked for
                // (specification §3.1.4).
                if !include_artifacts {
                    task.artifacts = None;
                }
                trim_history(task, req.history_length)
            })
            .collect();
        Ok(ListTasksResponse {
            tasks,
            // Always present; the empty string on the last page (specification §3.1.4).
            next_page_token: page.next_page_token.unwrap_or_default(),
            page_size: i32::try_from(page_size).unwrap_or(i32::MAX),
            total_size: i32::try_from(page.total_size).unwrap_or(i32::MAX),
        })
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

    #[tracing::instrument(skip_all, fields(method = "CreateTaskPushNotificationConfig", task_id = %req.task_id))]
    async fn create_push_config(
        &self,
        params: &ServiceParams,
        req: TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        let caller = self.caller(params, &[])?;
        let support = self.push_support()?;
        let task = self.owned_task(&caller, &req.task_id).await?;
        self.validate_push_config(support, &req)?;
        // What the webhook already knows is what the task is now: only later changes are news.
        let cursor = PushCursor::baseline(&task);
        self.store_push_config(&caller, &task, req, cursor, true)
            .await?
            .ok_or_else(|| A2AError::internal("internal error"))
    }

    #[tracing::instrument(skip_all, fields(method = "GetTaskPushNotificationConfig", task_id = %req.task_id))]
    async fn get_push_config(
        &self,
        params: &ServiceParams,
        req: GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        let caller = self.caller(params, &[])?;
        let support = self.push_support()?;
        let task = self.owned_task(&caller, &req.task_id).await?;
        support
            .store()
            .list(&task.id)
            .await
            .map_err(|e| push_error(e, &task.id))?
            .into_iter()
            .find(|r| r.id == req.id)
            .map(|r| redacted(r.config))
            // "The push notification configuration does not exist" is TaskNotFound (§3.1.8).
            .ok_or_else(|| A2AError::task_not_found(&req.id))
    }

    #[tracing::instrument(skip_all, fields(method = "ListTaskPushNotificationConfigs", task_id = %req.task_id))]
    async fn list_push_configs(
        &self,
        params: &ServiceParams,
        req: ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
        let caller = self.caller(params, &[])?;
        let support = self.push_support()?;
        let task = self.owned_task(&caller, &req.task_id).await?;
        let page_size = TaskQuery::resolve_page_size(req.page_size);
        // The configs are ordered by id and a task has few: the token is the last id of the
        // page, and the page is what follows it.
        let after = match req.page_token.as_deref().filter(|t| !t.is_empty()) {
            Some(token) => Some(decode_config_token(token)?),
            None => None,
        };
        let mut configs: Vec<_> = support
            .store()
            .list(&task.id)
            .await
            .map_err(|e| push_error(e, &task.id))?
            .into_iter()
            .filter(|r| after.as_deref().is_none_or(|a| r.id.as_str() > a))
            .collect();
        let more = configs.len() > page_size;
        configs.truncate(page_size);
        let next_page_token = more
            .then(|| configs.last().map(|r| encode_config_token(&r.id)))
            .flatten();
        Ok(ListTaskPushNotificationConfigsResponse {
            configs: configs.into_iter().map(|r| redacted(r.config)).collect(),
            next_page_token,
        })
    }

    #[tracing::instrument(skip_all, fields(method = "DeleteTaskPushNotificationConfig", task_id = %req.task_id))]
    async fn delete_push_config(
        &self,
        params: &ServiceParams,
        req: DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2AError> {
        let caller = self.caller(params, &[])?;
        let support = self.push_support()?;
        let task = self.owned_task(&caller, &req.task_id).await?;
        // Idempotent: deleting what is not there is not an error (§3.1.10).
        support
            .store()
            .delete(&task.id, &req.id)
            .await
            .map(|_| ())
            .map_err(|e| push_error(e, &task.id))
    }

    #[tracing::instrument(skip_all, fields(method = "GetExtendedAgentCard"))]
    async fn get_extended_agent_card(
        &self,
        params: &ServiceParams,
        _req: GetExtendedAgentCardRequest,
    ) -> Result<AgentCard, A2AError> {
        let caller = self.caller(params, &[])?;
        // Authenticated callers only. The server turns the extended card on only when it
        // authenticates, so the anonymous caller never reaches a card here; this refuses it all
        // the same.
        match &self.extended {
            Some(card) if caller.subject != Caller::ANONYMOUS => Ok(AgentCard::clone(card)),
            _ => Err(A2AError::unsupported_operation(
                "this agent has no extended agent card",
            )),
        }
    }
}

/// A page token of `ListTaskPushNotificationConfigs`: the last id of the page, opaque to clients.
fn encode_config_token(id: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(id)
}

fn decode_config_token(token: &str) -> Result<String, A2AError> {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(token)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .ok_or_else(|| A2AError::invalid_params("invalid page token"))
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
