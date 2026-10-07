//! Round trips through a real listener with the official A2A client
//! (`a2a-client-lf`), plus raw HTTP where the client hides what we need to see
//! (status codes, SSE comment frames).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use a2a::{
    CancelTaskRequest, GetTaskRequest, Message, Part, Role, SendMessageConfiguration,
    SendMessageRequest, SendMessageResponse, StreamResponse, SubscribeToTaskRequest, Task,
    TaskState, error_code,
};
use a2a_client::agent_card::AgentCardResolver;
use a2a_client::auth::AuthInterceptor;
use a2a_client::{A2AClient, A2AClientFactory, Transport};
use adam_a2a::{
    A2aServer, AgentCardConfig, AuthConfig, BackendError, Caller, ExtensionConfig, InMemoryBackend,
    InMemoryConfig, ServerOptions, SkillConfig, TaskBackend, TaskEvent,
};
use futures::StreamExt;
use futures::stream::BoxStream;
use secrecy::SecretString;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const TOKEN: &str = "test-token-alpha";
const OTHER_TOKEN: &str = "test-token-beta";

type Client = A2AClient<Box<dyn Transport>>;
type Events = BoxStream<'static, Result<StreamResponse, a2a::A2AError>>;

struct TestServer {
    addr: SocketAddr,
    backend: InMemoryBackend,
}

impl TestServer {
    async fn start(auth: AuthConfig) -> Self {
        Self::start_with(auth, InMemoryConfig::default(), ServerOptions::default()).await
    }

    async fn start_with(auth: AuthConfig, memory: InMemoryConfig, options: ServerOptions) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let card = AgentCardConfig::new(
            "echo-agent",
            "Echoes messages",
            format!("http://{addr}/").parse().unwrap(),
            "0.1.0",
        )
        .with_skill(SkillConfig::new("echo", "Echo", "Repeats what it is told"));
        let backend = InMemoryBackend::with_config(memory);
        let app = A2aServer::router_with_options(card, Arc::new(backend.clone()), auth, options);
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { addr, backend }
    }

    fn bearer() -> AuthConfig {
        AuthConfig::BearerTokens(vec![
            SecretString::from(TOKEN),
            SecretString::from(OTHER_TOKEN),
        ])
    }

    fn base(&self) -> String {
        format!("http://{}", self.addr)
    }

    async fn client(&self, token: Option<&str>) -> Client {
        let card = AgentCardResolver::new(None)
            .resolve(&self.base())
            .await
            .unwrap();
        let mut factory = A2AClientFactory::builder();
        if let Some(token) = token {
            factory = factory.with_interceptor(Arc::new(AuthInterceptor::bearer(token)));
        }
        factory.build().create_from_card(&card).await.unwrap()
    }
}

fn user_message(text: &str, task_id: Option<&str>) -> SendMessageRequest {
    let mut message = Message::new(Role::User, vec![Part::text(text)]);
    message.task_id = task_id.map(str::to_owned);
    SendMessageRequest {
        message,
        configuration: None,
        metadata: None,
        tenant: None,
    }
}

fn returning_immediately(mut request: SendMessageRequest) -> SendMessageRequest {
    request.configuration = Some(SendMessageConfiguration {
        accepted_output_modes: None,
        task_push_notification_config: None,
        history_length: None,
        return_immediately: Some(true),
    });
    request
}

fn task_of(response: SendMessageResponse) -> Task {
    match response {
        SendMessageResponse::Task(task) => task,
        other => panic!("expected a task, got {other:?}"),
    }
}

fn get_request(id: &str) -> GetTaskRequest {
    GetTaskRequest {
        id: id.to_owned(),
        history_length: None,
        tenant: None,
    }
}

fn cancel_request(id: &str) -> CancelTaskRequest {
    CancelTaskRequest {
        id: id.to_owned(),
        metadata: None,
        tenant: None,
    }
}

fn subscribe_request(id: &str) -> SubscribeToTaskRequest {
    SubscribeToTaskRequest {
        id: id.to_owned(),
        tenant: None,
    }
}

/// Short label per stream item, for order assertions.
fn label(item: &StreamResponse) -> String {
    match item {
        StreamResponse::Task(t) => format!("task:{:?}", t.status.state),
        StreamResponse::Message(_) => "message".to_owned(),
        StreamResponse::StatusUpdate(u) => format!("status:{:?}", u.status.state),
        StreamResponse::ArtifactUpdate(_) => "artifact".to_owned(),
    }
}

async fn next(events: &mut Events) -> StreamResponse {
    tokio::time::timeout(Duration::from_secs(5), events.next())
        .await
        .expect("timed out waiting for a stream item")
        .expect("stream ended early")
        .expect("stream item was an error")
}

async fn drain(events: &mut Events) -> Vec<String> {
    let mut labels = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(5), events.next()).await {
            Ok(Some(item)) => labels.push(label(&item.unwrap())),
            Ok(None) => return labels,
            Err(_) => panic!("timed out draining stream; got {labels:?}"),
        }
    }
}

async fn wait_for_state(client: &Client, id: &str, state: TaskState) -> Task {
    for _ in 0..200 {
        let task = client.get_task(&get_request(id)).await.unwrap();
        if task.status.state == state {
            return task;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("task {id} never reached {state:?}");
}

// ---------------------------------------------------------------- raw HTTP

struct Raw {
    status: u16,
    headers: String,
    body: String,
}

async fn raw(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Raw {
    raw_typed(addr, method, path, Some("application/json"), headers, body).await
}

/// Like [`raw`], with the `Content-Type` under the test's control (`None`
/// sends none).
async fn raw_typed(
    addr: SocketAddr,
    method: &str,
    path: &str,
    content_type: Option<&str>,
    headers: &[(&str, &str)],
    body: &str,
) -> Raw {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(content_type) = content_type {
        request.push_str(&format!("Content-Type: {content_type}\r\n"));
    }
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);
    stream.write_all(request.as_bytes()).await.unwrap();

    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut bytes))
        .await
        .expect("timed out reading response")
        .unwrap();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap();
    Raw {
        status,
        headers: head.to_lowercase(),
        body: body.to_owned(),
    }
}

fn rpc(method: &str, params: serde_json::Value) -> String {
    serde_json::json!({"jsonrpc": "2.0", "id": "1", "method": method, "params": params}).to_string()
}

// ------------------------------------------------------------------- tests

#[tokio::test]
async fn card_is_public_and_describes_the_agent() {
    let server = TestServer::start(TestServer::bearer()).await;
    let card = AgentCardResolver::new(None)
        .resolve(&server.base())
        .await
        .unwrap();
    assert_eq!(card.name, "echo-agent");
    assert_eq!(card.version, "0.1.0");
    assert_eq!(card.capabilities.streaming, Some(true));
    assert_eq!(card.capabilities.push_notifications, Some(false));
    assert_eq!(card.skills[0].id, "echo");
    assert_eq!(
        card.supported_interfaces[0].url,
        format!("{}/", server.base())
    );
    let schemes = card.security_schemes.expect("bearer scheme advertised");
    assert!(schemes.contains_key("bearer"));
    assert!(card.security_requirements.is_some());
}

#[tokio::test]
async fn card_omits_security_when_anonymous() {
    let server = TestServer::start(AuthConfig::AllowAnonymous).await;
    let card = AgentCardResolver::new(None)
        .resolve(&server.base())
        .await
        .unwrap();
    assert!(card.security_schemes.is_none());
    assert!(card.security_requirements.is_none());
}

#[tokio::test]
async fn send_returns_a_completed_task_with_the_echo_artifact() {
    let server = TestServer::start(TestServer::bearer()).await;
    let client = server.client(Some(TOKEN)).await;

    let task = task_of(
        client
            .send_message(&user_message("hello", None))
            .await
            .unwrap(),
    );
    assert_eq!(task.status.state, TaskState::Completed);
    let artifacts = task.artifacts.expect("artifact");
    assert_eq!(artifacts[0].parts[0].as_text(), Some("echo: hello"));
    assert_eq!(task.history.unwrap()[0].text(), Some("hello"));
}

#[tokio::test]
async fn stream_yields_working_then_artifact_then_completed_in_order() {
    let server = TestServer::start_with(
        TestServer::bearer(),
        InMemoryConfig {
            step_delay: Duration::from_millis(100),
        },
        ServerOptions::default(),
    )
    .await;
    let client = server.client(Some(TOKEN)).await;

    let mut events = client
        .send_streaming_message(&user_message("stream me", None))
        .await
        .unwrap();
    assert_eq!(
        drain(&mut events).await,
        [
            "task:Submitted",
            "status:Working",
            "artifact",
            "status:Completed"
        ]
    );
}

#[tokio::test]
async fn stream_carries_the_echo_text() {
    let server = TestServer::start(TestServer::bearer()).await;
    let client = server.client(Some(TOKEN)).await;

    let mut events = client
        .send_streaming_message(&user_message("payload", None))
        .await
        .unwrap();
    let mut echoed = None;
    while let Some(item) = events.next().await {
        if let StreamResponse::ArtifactUpdate(update) = item.unwrap() {
            echoed = update.artifact.parts[0].as_text().map(str::to_owned);
        }
    }
    assert_eq!(echoed.as_deref(), Some("echo: payload"));
}

#[tokio::test]
async fn get_matches_the_task_that_send_returned() {
    let server = TestServer::start(TestServer::bearer()).await;
    let client = server.client(Some(TOKEN)).await;

    let sent = task_of(
        client
            .send_message(&user_message("hello", None))
            .await
            .unwrap(),
    );
    let got = client.get_task(&get_request(&sent.id)).await.unwrap();
    assert_eq!(got.id, sent.id);
    assert_eq!(got.context_id, sent.context_id);
    assert_eq!(got.status.state, sent.status.state);
    assert_eq!(
        got.artifacts.map(|a| a.len()),
        sent.artifacts.map(|a| a.len())
    );
}

#[tokio::test]
async fn get_trims_history_and_reports_unknown_tasks() {
    let server = TestServer::start(TestServer::bearer()).await;
    let client = server.client(Some(TOKEN)).await;

    let sent = task_of(
        client
            .send_message(&user_message("hello", None))
            .await
            .unwrap(),
    );
    let mut request = get_request(&sent.id);
    request.history_length = Some(0);
    let trimmed = client.get_task(&request).await.unwrap();
    assert!(trimmed.history.unwrap_or_default().is_empty());

    let err = client
        .get_task(&get_request("no-such-task"))
        .await
        .unwrap_err();
    assert_eq!(err.code, error_code::TASK_NOT_FOUND);
}

#[tokio::test]
async fn cancel_gives_canceled_and_stops_the_work() {
    let server = TestServer::start(TestServer::bearer()).await;
    let client = server.client(Some(TOKEN)).await;

    let started = task_of(
        client
            .send_message(&returning_immediately(user_message("[hold] work", None)))
            .await
            .unwrap(),
    );
    wait_for_state(&client, &started.id, TaskState::Working).await;

    let canceled = client
        .cancel_task(&cancel_request(&started.id))
        .await
        .unwrap();
    assert_eq!(canceled.status.state, TaskState::Canceled);

    // Releasing the (now canceled) task must not resurrect it.
    server.backend.release(&started.id);
    tokio::time::sleep(Duration::from_millis(150)).await;
    let after = client.get_task(&get_request(&started.id)).await.unwrap();
    assert_eq!(after.status.state, TaskState::Canceled);
    assert!(after.artifacts.unwrap_or_default().is_empty());

    // Idempotent, and a completed task is not cancelable.
    let again = client
        .cancel_task(&cancel_request(&started.id))
        .await
        .unwrap();
    assert_eq!(again.status.state, TaskState::Canceled);
    let done = task_of(
        client
            .send_message(&user_message("quick", None))
            .await
            .unwrap(),
    );
    let err = client
        .cancel_task(&cancel_request(&done.id))
        .await
        .unwrap_err();
    assert_eq!(err.code, error_code::TASK_NOT_CANCELABLE);
}

#[tokio::test]
async fn cancel_ends_open_streams_with_canceled() {
    let server = TestServer::start(TestServer::bearer()).await;
    let client = server.client(Some(TOKEN)).await;

    let mut events = client
        .send_streaming_message(&user_message("[hold] work", None))
        .await
        .unwrap();
    let StreamResponse::Task(task) = next(&mut events).await else {
        panic!("first frame must be the task snapshot");
    };
    client.cancel_task(&cancel_request(&task.id)).await.unwrap();
    assert!(
        drain(&mut events)
            .await
            .contains(&"status:Canceled".to_owned())
    );
}

#[tokio::test]
async fn resubscribe_on_a_running_task_continues_the_stream() {
    let server = TestServer::start(TestServer::bearer()).await;
    let client = server.client(Some(TOKEN)).await;

    let mut first = client
        .send_streaming_message(&user_message("[hold] resume me", None))
        .await
        .unwrap();
    let StreamResponse::Task(task) = next(&mut first).await else {
        panic!("first frame must be the task snapshot");
    };
    assert_eq!(label(&next(&mut first).await), "status:Working");

    // A second connection joins mid-flight: snapshot first, then the rest.
    let mut resub = client
        .subscribe_to_task(&subscribe_request(&task.id))
        .await
        .unwrap();
    assert_eq!(label(&next(&mut resub).await), "task:Working");
    assert!(server.backend.release(&task.id));
    assert_eq!(drain(&mut resub).await, ["artifact", "status:Completed"]);
    assert_eq!(drain(&mut first).await, ["artifact", "status:Completed"]);
}

#[tokio::test]
async fn resubscribing_to_a_finished_task_is_an_unsupported_operation() {
    let server = TestServer::start(TestServer::bearer()).await;
    let client = server.client(Some(TOKEN)).await;

    let done = task_of(
        client
            .send_message(&user_message("quick", None))
            .await
            .unwrap(),
    );
    let err = client
        .subscribe_to_task(&subscribe_request(&done.id))
        .await
        .err()
        .expect("terminal task cannot be resubscribed");
    assert_eq!(err.code, error_code::UNSUPPORTED_OPERATION);
    let err = client
        .subscribe_to_task(&subscribe_request("no-such-task"))
        .await
        .err()
        .expect("unknown task");
    assert_eq!(err.code, error_code::TASK_NOT_FOUND);
}

#[tokio::test]
async fn input_required_is_resumed_by_a_follow_up_with_the_same_task_id() {
    let server = TestServer::start(TestServer::bearer()).await;
    let client = server.client(Some(TOKEN)).await;

    let asked = task_of(
        client
            .send_message(&user_message("[input-required] need details", None))
            .await
            .unwrap(),
    );
    assert_eq!(asked.status.state, TaskState::InputRequired);

    let answered = task_of(
        client
            .send_message(&user_message("here they are", Some(&asked.id)))
            .await
            .unwrap(),
    );
    assert_eq!(answered.id, asked.id);
    assert_eq!(answered.context_id, asked.context_id);
    assert_eq!(answered.status.state, TaskState::Completed);
    assert_eq!(answered.history.unwrap().len(), 2);
    assert_eq!(
        answered.artifacts.unwrap()[0].parts[0].as_text(),
        Some("echo: here they are")
    );
}

#[tokio::test]
async fn input_required_streams_end_at_the_interruption_and_a_follow_up_can_stream_on() {
    let server = TestServer::start(TestServer::bearer()).await;
    let client = server.client(Some(TOKEN)).await;

    let mut events = client
        .send_streaming_message(&user_message("[input-required] q", None))
        .await
        .unwrap();
    let StreamResponse::Task(task) = next(&mut events).await else {
        panic!("first frame must be the task snapshot");
    };
    assert_eq!(
        drain(&mut events).await,
        ["status:Working", "status:InputRequired"]
    );

    let mut resumed = client
        .send_streaming_message(&user_message("answer", Some(&task.id)))
        .await
        .unwrap();
    assert_eq!(
        drain(&mut resumed).await,
        ["task:Working", "artifact", "status:Completed"]
    );
}

#[tokio::test]
async fn follow_ups_to_unknown_or_finished_tasks_are_rejected() {
    let server = TestServer::start(TestServer::bearer()).await;
    let client = server.client(Some(TOKEN)).await;

    let err = client
        .send_message(&user_message("hi", Some("nope")))
        .await
        .unwrap_err();
    assert_eq!(err.code, error_code::TASK_NOT_FOUND);

    let done = task_of(
        client
            .send_message(&user_message("quick", None))
            .await
            .unwrap(),
    );
    let err = client
        .send_message(&user_message("again", Some(&done.id)))
        .await
        .unwrap_err();
    assert_eq!(err.code, error_code::UNSUPPORTED_OPERATION);
}

#[tokio::test]
async fn invalid_messages_are_invalid_params() {
    let server = TestServer::start(TestServer::bearer()).await;
    let client = server.client(Some(TOKEN)).await;

    let mut empty = user_message("x", None);
    empty.message.parts.clear();
    let err = client.send_message(&empty).await.unwrap_err();
    assert_eq!(err.code, error_code::INVALID_PARAMS);

    let mut agent_role = user_message("x", None);
    agent_role.message.role = Role::Agent;
    let err = client.send_message(&agent_role).await.unwrap_err();
    assert_eq!(err.code, error_code::INVALID_PARAMS);
}

#[tokio::test]
async fn unsupported_methods_answer_with_a2a_errors() {
    let server = TestServer::start(TestServer::bearer()).await;
    let auth = [("Authorization", format!("Bearer {TOKEN}"))];
    let headers: Vec<(&str, &str)> = auth.iter().map(|(k, v)| (*k, v.as_str())).collect();

    // What this server was not given: push notifications and an extended card are off by default
    // (`ListTasks` is served: see `tests/list_tasks.rs`).
    for (method, params, code) in [
        (
            "CreateTaskPushNotificationConfig",
            serde_json::json!({"taskId": "t", "url": "https://example.com/hook"}),
            error_code::PUSH_NOTIFICATION_NOT_SUPPORTED,
        ),
        (
            "GetExtendedAgentCard",
            serde_json::json!({}),
            // A2A §3.3.4: without the capability the answer is UnsupportedOperationError.
            error_code::UNSUPPORTED_OPERATION,
        ),
        (
            "message/send",
            serde_json::json!({}),
            error_code::METHOD_NOT_FOUND,
        ),
    ] {
        let response = raw(server.addr, "POST", "/", &headers, &rpc(method, params)).await;
        assert_eq!(response.status, 200, "{method}");
        let body: serde_json::Value = serde_json::from_str(&response.body).unwrap();
        assert_eq!(body["error"]["code"], code, "{method}: {body}");
    }
}

// ------------------------------------------------------------------- auth

#[tokio::test]
async fn every_rpc_route_rejects_missing_and_wrong_tokens_with_401() {
    let server = TestServer::start(TestServer::bearer()).await;
    let methods = [
        "SendMessage",
        "SendStreamingMessage",
        "GetTask",
        "CancelTask",
        "SubscribeToTask",
        "ListTasks",
        "GetExtendedAgentCard",
        "CreateTaskPushNotificationConfig",
        "no-such-method",
    ];
    let credentials: [Option<&str>; 6] = [
        None,
        Some("Bearer wrong-token"),
        Some("Bearer "),
        Some("Basic dGVzdDp0ZXN0"),
        Some(TOKEN),                    // right token, missing scheme
        Some("Bearer test-token-alph"), // prefix of a valid token
    ];

    for method in methods {
        for credential in credentials {
            let headers: Vec<(&str, &str)> = credential
                .map(|c| ("Authorization", c))
                .into_iter()
                .collect();
            let response = raw(
                server.addr,
                "POST",
                "/",
                &headers,
                &rpc(method, serde_json::json!({})),
            )
            .await;
            assert_eq!(response.status, 401, "{method} with {credential:?}");
            assert!(
                response.headers.contains("www-authenticate: bearer"),
                "{method}: {}",
                response.headers
            );
            let body: serde_json::Value = serde_json::from_str(&response.body).unwrap();
            assert_eq!(body["jsonrpc"], "2.0");
            assert_eq!(body["error"]["code"], -32000);
        }
    }
}

#[tokio::test]
async fn other_routes_and_methods_are_closed_too() {
    let server = TestServer::start(TestServer::bearer()).await;
    for (method, path) in [
        ("GET", "/"),
        ("POST", "/anything"),
        ("GET", "/tasks/abc"),
        ("POST", "/healthz"),
        ("POST", "/.well-known/agent-card.json"),
    ] {
        let response = raw(server.addr, method, path, &[], "{}").await;
        assert_eq!(response.status, 401, "{method} {path}");
    }
}

#[tokio::test]
async fn card_and_healthz_stay_open() {
    let server = TestServer::start(TestServer::bearer()).await;
    let card = raw(server.addr, "GET", "/.well-known/agent-card.json", &[], "").await;
    assert_eq!(card.status, 200);
    assert!(card.body.contains("echo-agent"));
    let health = raw(server.addr, "GET", "/healthz", &[], "").await;
    assert_eq!(health.status, 200);
    assert_eq!(
        health.body.trim_end_matches('\n').lines().last(),
        Some("ok")
    );
}

/// `A2aServer::health_router` is the probe route alone: `/healthz` answers, and nothing of A2A is
/// there (a worker with no front still answers its probes).
#[tokio::test]
async fn health_router_serves_healthz_and_nothing_else() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, A2aServer::health_router())
            .await
            .unwrap();
    });
    let health = raw(addr, "GET", "/healthz", &[], "").await;
    assert_eq!(health.status, 200);
    assert_eq!(
        health.body.trim_end_matches('\n').lines().last(),
        Some("ok")
    );
    for (method, path) in [
        ("GET", "/.well-known/agent-card.json"),
        ("POST", "/"),
        ("POST", "/healthz"),
    ] {
        let response = raw(addr, method, path, &[], "{}").await;
        assert!(
            response.status == 404 || response.status == 405,
            "{method} {path}: {}",
            response.status
        );
    }
}

#[tokio::test]
async fn the_client_gets_a_typed_error_on_a_bad_token() {
    let server = TestServer::start(TestServer::bearer()).await;
    let client = server.client(Some("wrong")).await;
    let err = client.get_task(&get_request("t")).await.unwrap_err();
    assert!(
        err.code == -32000 || err.message.to_lowercase().contains("unauthorized"),
        "expected an auth error, got {err:?}"
    );
    let client = server.client(None).await;
    let err = client
        .send_message(&user_message("hi", None))
        .await
        .unwrap_err();
    assert!(
        err.code == -32000 || err.message.to_lowercase().contains("unauthorized"),
        "expected an auth error, got {err:?}"
    );
}

#[tokio::test]
async fn the_correct_token_works_and_either_configured_token_is_accepted() {
    let server = TestServer::start(TestServer::bearer()).await;
    for token in [TOKEN, OTHER_TOKEN] {
        let client = server.client(Some(token)).await;
        let task = task_of(
            client
                .send_message(&user_message("hi", None))
                .await
                .unwrap(),
        );
        assert_eq!(task.status.state, TaskState::Completed);
    }
}

#[tokio::test]
async fn anonymous_mode_needs_no_token() {
    let server = TestServer::start(AuthConfig::AllowAnonymous).await;
    let client = server.client(None).await;
    let task = task_of(
        client
            .send_message(&user_message("hi", None))
            .await
            .unwrap(),
    );
    assert_eq!(task.status.state, TaskState::Completed);
}

#[tokio::test]
async fn an_empty_token_list_rejects_everything() {
    let server = TestServer::start(AuthConfig::BearerTokens(Vec::new())).await;
    let response = raw(
        server.addr,
        "POST",
        "/",
        &[("Authorization", "Bearer anything")],
        &rpc("GetTask", serde_json::json!({"id": "t"})),
    )
    .await;
    assert_eq!(response.status, 401);
}

#[tokio::test]
async fn tasks_are_private_to_their_caller_and_identity_cannot_be_forged() {
    let server = TestServer::start(TestServer::bearer()).await;
    let owner = server.client(Some(TOKEN)).await;
    let other = server.client(Some(OTHER_TOKEN)).await;

    let task = task_of(
        owner
            .send_message(&user_message("secret", None))
            .await
            .unwrap(),
    );

    // The other token cannot see, cancel or follow up on it.
    let err = other.get_task(&get_request(&task.id)).await.unwrap_err();
    assert_eq!(err.code, error_code::TASK_NOT_FOUND);
    let err = other
        .cancel_task(&cancel_request(&task.id))
        .await
        .unwrap_err();
    assert_eq!(err.code, error_code::TASK_NOT_FOUND);

    // Claiming to be the owner with a client-sent identity header changes nothing.
    let response = raw(
        server.addr,
        "POST",
        "/",
        &[
            ("Authorization", &format!("Bearer {OTHER_TOKEN}")),
            ("X-Adam-Caller-Subject", "token-0"),
        ],
        &rpc("GetTask", serde_json::json!({"id": task.id})),
    )
    .await;
    let body: serde_json::Value = serde_json::from_str(&response.body).unwrap();
    assert_eq!(body["error"]["code"], error_code::TASK_NOT_FOUND, "{body}");

    // ...and a forged header with no credential is still just a 401.
    let response = raw(
        server.addr,
        "POST",
        "/",
        &[("X-Adam-Caller-Subject", "token-0")],
        &rpc("GetTask", serde_json::json!({"id": task.id})),
    )
    .await;
    assert_eq!(response.status, 401);

    // The owner still can.
    assert_eq!(
        owner.get_task(&get_request(&task.id)).await.unwrap().id,
        task.id
    );
}

// ------------------------------------------------------------ extensions

const EXT_A: &str = "https://example.org/extensions/a/v1";
const EXT_B: &str = "https://example.org/extensions/b/v1";
const EXT_UNDECLARED: &str = "https://example.org/extensions/undeclared/v1";

/// The reference backend, remembering the caller of every call it gets.
#[derive(Clone)]
struct Recording {
    inner: InMemoryBackend,
    seen: Arc<Mutex<Vec<(&'static str, Caller)>>>,
}

impl Recording {
    fn note(&self, call: &'static str, caller: &Caller) {
        self.seen.lock().unwrap().push((call, caller.clone()));
    }

    /// The extensions the caller of the last `call` carried.
    fn last(&self, call: &str) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(c, _)| *c == call)
            .unwrap_or_else(|| panic!("no {call} call was made"))
            .1
            .extensions
            .clone()
    }

    fn subjects(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|(_, c)| c.subject.clone())
            .collect()
    }
}

#[async_trait::async_trait]
impl TaskBackend for Recording {
    async fn submit(
        &self,
        caller: Caller,
        message: Message,
        task_id: Option<String>,
        context_id: Option<String>,
    ) -> Result<Task, BackendError> {
        self.note("submit", &caller);
        self.inner
            .submit(caller, message, task_id, context_id)
            .await
    }

    async fn get(&self, caller: &Caller, task_id: &str) -> Result<Option<Task>, BackendError> {
        self.note("get", caller);
        self.inner.get(caller, task_id).await
    }

    async fn cancel(&self, caller: &Caller, task_id: &str) -> Result<Task, BackendError> {
        self.note("cancel", caller);
        self.inner.cancel(caller, task_id).await
    }

    fn subscribe(
        &self,
        caller: &Caller,
        task_id: &str,
    ) -> BoxStream<'static, Result<TaskEvent, BackendError>> {
        self.note("subscribe", caller);
        self.inner.subscribe(caller, task_id)
    }
}

/// A server whose card declares `declared` extensions, over a [`Recording`] backend.
async fn start_declaring(declared: &[&str]) -> (SocketAddr, Recording) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut card = AgentCardConfig::new(
        "echo-agent",
        "Echoes messages",
        format!("http://{addr}/").parse().unwrap(),
        "0.1.0",
    );
    for uri in declared {
        card = card.with_extension(ExtensionConfig::new(*uri));
    }
    let backend = Recording {
        inner: InMemoryBackend::default(),
        seen: Arc::default(),
    };
    let app = A2aServer::router(card, Arc::new(backend.clone()), TestServer::bearer());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, backend)
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

/// `SendMessage` as a client that names `message_extensions` in the message sends it.
fn send_naming(message_extensions: &[&str]) -> String {
    rpc(
        "SendMessage",
        serde_json::json!({
            "message": {
                "messageId": "m-1", "role": "ROLE_USER", "parts": [{"text": "hello"}],
                "extensions": message_extensions,
            },
            "configuration": {"returnImmediately": true}
        }),
    )
}

#[tokio::test]
async fn a_request_activates_the_extensions_its_header_and_its_message_name_that_the_card_declares()
{
    let (addr, backend) = start_declaring(&[EXT_A, EXT_B]).await;
    let auth = bearer(TOKEN);
    let named = format!("{EXT_A}, {EXT_UNDECLARED}");
    let response = raw(
        addr,
        "POST",
        "/",
        &[("Authorization", &auth), ("A2A-Extensions", &named)],
        &send_naming(&[EXT_B, EXT_A]),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);

    // The backend sees the declared ones, once each, in the order they were named (the header
    // first); the one the card never declared is not switched on.
    assert_eq!(backend.last("submit"), [EXT_A, EXT_B]);
    // The response says which were activated.
    assert!(
        response
            .headers
            .contains(&format!("a2a-extensions: {EXT_A}, {EXT_B}").to_lowercase()),
        "{}",
        response.headers
    );
    // Who the caller is does not change.
    assert_eq!(backend.subjects(), ["token-0"]);
}

/// The HTTP+JSON binding goes through the same handler: the backend sees the same caller and the
/// same extensions as for the JSON-RPC request above, and the response echoes them.
#[tokio::test]
async fn http_json_gives_the_backend_the_same_caller_and_extensions() {
    let (addr, backend) = start_declaring(&[EXT_A, EXT_B]).await;
    let auth = bearer(TOKEN);
    let named = format!("{EXT_A}, {EXT_UNDECLARED}");
    let body = serde_json::json!({
        "message": {
            "messageId": "m-rest", "role": "ROLE_USER", "parts": [{"text": "hello"}],
            "extensions": [EXT_B, EXT_A],
        },
        "configuration": {"returnImmediately": true}
    });
    let response = raw(
        addr,
        "POST",
        "/message:send",
        &[("Authorization", &auth), ("A2A-Extensions", &named)],
        &body.to_string(),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(backend.last("submit"), [EXT_A, EXT_B]);
    assert!(
        response
            .headers
            .contains(&format!("a2a-extensions: {EXT_A}, {EXT_B}").to_lowercase()),
        "{}",
        response.headers
    );
    let id = serde_json::from_str::<serde_json::Value>(&response.body).unwrap()["task"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let poll = raw(
        addr,
        "GET",
        &format!("/tasks/{id}"),
        &[("Authorization", &auth), ("A2A-Extensions", EXT_B)],
        "",
    )
    .await;
    assert_eq!(poll.status, 200, "{}", poll.body);
    assert_eq!(backend.last("get"), [EXT_B]);
    assert_eq!(backend.subjects(), ["token-0", "token-0"]);
}

#[tokio::test]
async fn nothing_is_activated_unless_the_request_asks_and_the_card_declares() {
    let (addr, backend) = start_declaring(&[EXT_A]).await;
    let auth = bearer(TOKEN);

    // Declared, but not asked for: no header, a message that names none.
    let response = raw(
        addr,
        "POST",
        "/",
        &[("Authorization", &auth)],
        &send_naming(&[]),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert!(backend.last("submit").is_empty());
    assert!(
        !response.headers.contains("a2a-extensions"),
        "no echo for nothing: {}",
        response.headers
    );

    // Asked for, but not declared (a near miss included): not activated, not echoed.
    for named in [
        EXT_UNDECLARED,
        "https://example.org/extensions/a/v2",
        "HTTPS://EXAMPLE.ORG/EXTENSIONS/A/V1",
    ] {
        let response = raw(
            addr,
            "POST",
            "/",
            &[("Authorization", &auth), ("A2A-Extensions", named)],
            &send_naming(&[named]),
        )
        .await;
        assert_eq!(response.status, 200, "{}", response.body);
        assert!(backend.last("submit").is_empty(), "{named}");
        assert!(!response.headers.contains("a2a-extensions"), "{named}");
    }

    // A card that declares nothing activates nothing, however much is asked.
    let (addr, backend) = start_declaring(&[]).await;
    let response = raw(
        addr,
        "POST",
        "/",
        &[("Authorization", &auth), ("A2A-Extensions", EXT_A)],
        &send_naming(&[EXT_A]),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert!(backend.last("submit").is_empty());
    assert!(!response.headers.contains("a2a-extensions"));
}

#[tokio::test]
async fn a_resubscribe_a_poll_and_a_cancel_activate_for_themselves() {
    let (addr, backend) = start_declaring(&[EXT_A]).await;
    let auth = bearer(TOKEN);
    // A task that stays working, so that it can be subscribed to.
    let held = rpc(
        "SendMessage",
        serde_json::json!({
            "message": {"messageId": "m-1", "role": "ROLE_USER", "parts": [{"text": "[hold]"}]},
            "configuration": {"returnImmediately": true}
        }),
    );
    let sent = raw(addr, "POST", "/", &[("Authorization", &auth)], &held).await;
    let task: serde_json::Value = serde_json::from_str(&sent.body).unwrap();
    let id = task["result"]["task"]["id"].as_str().unwrap().to_owned();
    assert!(backend.last("submit").is_empty());

    let headers = [("Authorization", auth.as_str()), ("A2A-Extensions", EXT_A)];
    let polled = raw(
        addr,
        "POST",
        "/",
        &headers,
        &rpc("GetTask", serde_json::json!({"id": id})),
    )
    .await;
    assert_eq!(polled.status, 200);
    assert_eq!(backend.last("get"), [EXT_A]);
    assert!(
        polled
            .headers
            .contains(&format!("a2a-extensions: {EXT_A}").to_lowercase())
    );

    // The same task, without the header: the backend gets a caller with none, and the task is the
    // same task (ownership is the subject's).
    let polled = raw(
        addr,
        "POST",
        "/",
        &[("Authorization", &auth)],
        &rpc("GetTask", serde_json::json!({"id": id})),
    )
    .await;
    let polled: serde_json::Value = serde_json::from_str(&polled.body).unwrap();
    assert_eq!(polled["result"]["id"], id, "{polled}");
    assert!(backend.last("get").is_empty());

    // A resubscribe (the stream ends when the task is released) names it again.
    let subscribe = {
        let headers = headers.map(|(k, v)| (k.to_owned(), v.to_owned()));
        let body = rpc("SubscribeToTask", serde_json::json!({"id": id}));
        tokio::spawn(async move {
            let headers: Vec<(&str, &str)> = headers
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            raw(addr, "POST", "/", &headers, &body).await
        })
    };
    for _ in 0..200 {
        if backend
            .seen
            .lock()
            .unwrap()
            .iter()
            .any(|(c, _)| *c == "subscribe")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(backend.last("subscribe"), [EXT_A]);

    let canceled = raw(
        addr,
        "POST",
        "/",
        &headers,
        &rpc("CancelTask", serde_json::json!({"id": id})),
    )
    .await;
    assert_eq!(canceled.status, 200);
    assert_eq!(backend.last("cancel"), [EXT_A]);
    let ended = subscribe.await.unwrap();
    assert_eq!(ended.status, 200);
    assert!(
        ended
            .headers
            .contains(&format!("a2a-extensions: {EXT_A}").to_lowercase())
    );
    // Every call was the same person's.
    assert!(backend.subjects().iter().all(|s| s == "token-0"));
}

// ------------------------------------------------- keepalive & disconnects

#[tokio::test]
async fn idle_streams_receive_keepalive_comment_frames() {
    let server = TestServer::start_with(
        TestServer::bearer(),
        InMemoryConfig::default(),
        ServerOptions::default().with_keepalive_interval(Duration::from_millis(50)),
    )
    .await;

    let body = rpc(
        "SendStreamingMessage",
        serde_json::json!({"message": {
            "messageId": "m1", "role": "ROLE_USER", "parts": [{"text": "[hold] idle"}]
        }}),
    );
    let mut stream = TcpStream::connect(server.addr).await.unwrap();
    let request = format!(
        "POST / HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nAccept: text/event-stream\r\nContent-Length: {}\r\n\r\n{body}",
        server.addr,
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();

    // The task is held, so nothing but keepalives can arrive after the first frames.
    let mut received = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut buf = [0u8; 4096];
    while received.matches(":\n\n").count() < 2 {
        let read = tokio::time::timeout_at(deadline, stream.read(&mut buf))
            .await
            .unwrap_or_else(|_| panic!("no keepalives within 5s; got {received:?}"))
            .unwrap();
        assert!(read > 0, "server closed the stream; got {received:?}");
        received.push_str(&String::from_utf8_lossy(&buf[..read]));
    }
    assert!(received.contains("text/event-stream"));
    assert!(
        received.contains("TASK_STATE_"),
        "task events precede the keepalives"
    );
}

#[tokio::test]
async fn client_disconnect_does_not_cancel_the_task() {
    let server = TestServer::start(TestServer::bearer()).await;
    let client = server.client(Some(TOKEN)).await;

    let mut events = client
        .send_streaming_message(&user_message("[hold] survive", None))
        .await
        .unwrap();
    let StreamResponse::Task(task) = next(&mut events).await else {
        panic!("first frame must be the task snapshot");
    };
    assert_eq!(label(&next(&mut events).await), "status:Working");
    drop(events); // the client goes away mid-stream

    tokio::time::sleep(Duration::from_millis(100)).await;
    let still = client.get_task(&get_request(&task.id)).await.unwrap();
    assert_eq!(still.status.state, TaskState::Working);

    assert!(server.backend.release(&task.id));
    let done = wait_for_state(&client, &task.id, TaskState::Completed).await;
    assert_eq!(
        done.artifacts.unwrap()[0].parts[0].as_text(),
        Some("echo: [hold] survive")
    );
}

#[tokio::test]
async fn a_blocking_send_that_is_abandoned_leaves_the_task_running() {
    let server = TestServer::start(TestServer::bearer()).await;
    let client = server.client(Some(TOKEN)).await;

    // The blocking SendMessage waits for completion; give up on it.
    let abandoned = tokio::time::timeout(
        Duration::from_millis(300),
        client.send_message(&user_message("[hold] abandoned", None)),
    )
    .await;
    assert!(abandoned.is_err(), "the held task cannot complete");

    let ids = server.backend.task_ids();
    assert_eq!(ids.len(), 1);
    let task = client.get_task(&get_request(&ids[0])).await.unwrap();
    assert_eq!(task.status.state, TaskState::Working);
    server.backend.release(&ids[0]);
    wait_for_state(&client, &ids[0], TaskState::Completed).await;
}

#[tokio::test]
async fn the_backend_seam_reports_unknown_tasks_as_not_found() {
    let backend = InMemoryBackend::new();
    let mut events = backend.subscribe(&Caller::new("token-0"), "unknown");
    assert!(events.next().await.unwrap().is_err());
    assert!(
        backend
            .get(&Caller::new("token-0"), "unknown")
            .await
            .unwrap()
            .is_none()
    );
}

fn bearer_header() -> [(&'static str, String); 1] {
    [("Authorization", format!("Bearer {TOKEN}"))]
}

/// A response to a request that is not a JSON-RPC request is always one shape: HTTP 200 with a
/// JSON-RPC error object whose code is -32700 (the body is not JSON) or -32600 (it is JSON, but not
/// a request, or was not declared as JSON), and whose id is null when none could be read (a request
/// that parses but is wrong, such as `"jsonrpc": "1.0"`, gets its own id back). It is never a
/// plain-text 400/415/422 from the SDK's extractor, never a 5xx, never empty or truncated.
/// Returns the error code and the response id.
fn assert_clean_rejection(what: &str, response: &Raw) -> (i64, serde_json::Value) {
    assert_eq!(
        response.status, 200,
        "{what}: unexpected status: {}",
        response.body
    );
    let body: serde_json::Value = serde_json::from_str(&response.body)
        .unwrap_or_else(|e| panic!("{what}: 200 with a non-JSON body ({e}): {}", response.body));
    assert_eq!(body["jsonrpc"], "2.0", "{what}: {body}");
    assert!(body.get("result").is_none(), "{what}: {body}");
    let code = body["error"]["code"]
        .as_i64()
        .unwrap_or_else(|| panic!("{what}: no error code: {body}"));
    assert!(
        [-32700, -32600].contains(&code),
        "{what}: unexpected code {code}: {body}"
    );
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty()),
        "{what}: {body}"
    );
    (code, body["id"].clone())
}

/// Bodies that are not JSON-RPC requests, sent with a valid token: rejected
/// cleanly, and the server keeps serving afterwards.
#[tokio::test]
async fn malformed_json_with_a_valid_token_is_rejected_cleanly() {
    for auth in [TestServer::bearer(), AuthConfig::AllowAnonymous] {
        let anonymous = matches!(auth, AuthConfig::AllowAnonymous);
        let server = TestServer::start(auth).await;
        let credentials = bearer_header();
        let headers: Vec<(&str, &str)> = if anonymous {
            Vec::new()
        } else {
            credentials.iter().map(|(k, v)| (*k, v.as_str())).collect()
        };
        let bodies = [
            "{not json",
            "",
            "[1,2",
            "\u{0}",
            "null",
            "{}",
            "[]",
            "\"a string\"",
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"params\":{}}",
            "{\"jsonrpc\":\"1.0\",\"id\":1,\"method\":\"GetTask\"}",
            // Deeply nested input must not blow the stack.
            &"[".repeat(10_000),
        ];
        for body in bodies {
            let response = raw(server.addr, "POST", "/", &headers, body).await;
            let what = format!("body {:?}", &body[..body.len().min(24)]);
            let (code, id) = assert_clean_rejection(&what, &response);
            // Not JSON at all is a parse error; JSON that is not a request is an invalid request.
            let expected = match body {
                "{not json" | "" | "[1,2" | "\u{0}" => Some(-32700),
                "null"
                | "{}"
                | "[]"
                | "\"a string\""
                | "{\"jsonrpc\":\"2.0\",\"id\":1,\"params\":{}}" => Some(-32600),
                _ => None,
            };
            if let Some(expected) = expected {
                assert_eq!(code, expected, "{what}: {}", response.body);
                assert!(
                    id.is_null(),
                    "{what}: no id could be read: {}",
                    response.body
                );
            }
        }

        // Valid JSON-RPC with params of the wrong shape is a JSON-RPC error
        // (-32700/-32602 family), and an unknown method is -32601.
        let wrong_params = raw(
            server.addr,
            "POST",
            "/",
            &headers,
            &rpc("SendMessage", serde_json::json!("oops")),
        )
        .await;
        assert_eq!(wrong_params.status, 200, "{}", wrong_params.body);
        let body: serde_json::Value = serde_json::from_str(&wrong_params.body).unwrap();
        assert!(
            [-32700, -32602].contains(&body["error"]["code"].as_i64().unwrap()),
            "{body}"
        );
        assert_eq!(body["id"], "1", "the request id is echoed: {body}");
        let unknown = raw(
            server.addr,
            "POST",
            "/",
            &headers,
            &rpc("no-such-method", serde_json::json!({})),
        )
        .await;
        assert_eq!(unknown.status, 200, "{}", unknown.body);
        let body: serde_json::Value = serde_json::from_str(&unknown.body).unwrap();
        assert_eq!(body["error"]["code"], -32601, "{body}");

        // The abuse did not wedge or crash anything: a real client still works.
        let client = server.client((!anonymous).then_some(TOKEN)).await;
        let task = task_of(
            client
                .send_message(&user_message("still there?", None))
                .await
                .expect("a normal send after the bad requests"),
        );
        assert_eq!(task.status.state, TaskState::Completed);
    }
}

/// Authentication comes before parsing: a garbage body without a valid token
/// is a 401, not a parse error that would tell an anonymous caller how the
/// server reads requests.
#[tokio::test]
async fn malformed_json_without_a_valid_token_is_still_401() {
    let server = TestServer::start(TestServer::bearer()).await;
    for body in ["{not json", "", "[1,2", "{}"] {
        for headers in [Vec::new(), vec![("Authorization", "Bearer wrong-token")]] {
            let response = raw(server.addr, "POST", "/", &headers, body).await;
            assert_eq!(response.status, 401, "body {body:?} with {headers:?}");
        }
    }
}

/// A body that is not declared as JSON is refused before it is read as
/// JSON-RPC, with a -32600 error object, for every request that is not
/// `application/json`; parameters on the media type are fine.
#[tokio::test]
async fn wrong_content_type_is_rejected_cleanly() {
    let server = TestServer::start(TestServer::bearer()).await;
    let credentials = bearer_header();
    let headers: Vec<(&str, &str)> = credentials.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let valid_body = rpc("GetTask", serde_json::json!({"id": "nope"}));
    for content_type in [
        None,
        Some("text/plain"),
        Some("application/xml"),
        Some("application/x-www-form-urlencoded"),
        Some("multipart/form-data; boundary=x"),
        Some("json"),
        Some(""),
    ] {
        let response = raw_typed(
            server.addr,
            "POST",
            "/",
            content_type,
            &headers,
            &valid_body,
        )
        .await;
        // Refused before the body is read as JSON-RPC: had the valid `GetTask` been processed, the
        // answer would be -32001 (task not found), not an invalid request.
        let (code, id) =
            assert_clean_rejection(&format!("content-type {content_type:?}"), &response);
        assert!(id.is_null(), "{}", response.body);
        assert_eq!(
            code, -32600,
            "content-type {content_type:?} was accepted as JSON-RPC: {}",
            response.body
        );
    }
    // Media type parameters are not a reason to refuse.
    let response = raw_typed(
        server.addr,
        "POST",
        "/",
        Some("application/json; charset=utf-8"),
        &headers,
        &valid_body,
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);
    let body: serde_json::Value = serde_json::from_str(&response.body).unwrap();
    assert_eq!(body["error"]["code"], -32001, "task not found: {body}");
}

/// A body over the SDK's size limit is an invalid request (-32600) like any other rejection, not a
/// bare plain-text 413.
#[tokio::test]
async fn an_oversized_body_is_an_invalid_request() {
    let server = TestServer::start(TestServer::bearer()).await;
    let credentials = bearer_header();
    let headers: Vec<(&str, &str)> = credentials.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let huge = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"GetTask\",\"params\":{{\"id\":\"{}\"}}}}",
        "x".repeat(a2a_server::jsonrpc::MAX_REQUEST_BODY_BYTES)
    );
    let response = raw(server.addr, "POST", "/", &headers, &huge).await;
    let (code, id) = assert_clean_rejection("an oversized body", &response);
    assert_eq!(code, -32600, "{}", response.body);
    assert!(id.is_null(), "{}", response.body);
}
