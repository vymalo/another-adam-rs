//! Shared by the push, listing, extended-card and signature tests: a server over
//! `InMemoryBackend`, the official client, and a webhook that records what it is sent.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use a2a::{
    AgentInterface, GetTaskRequest, Message, Part, Role, SendMessageConfiguration,
    SendMessageRequest, SendMessageResponse, StreamResponse, Task, TaskState,
};
use a2a_client::agent_card::AgentCardResolver;
use a2a_client::auth::AuthInterceptor;
use a2a_client::{A2AClient, A2AClientFactory, Transport};
use adam_a2a::push::{
    InMemoryPushStore, PushDeliverer, PushDeliveryOptions, PushPolicy, PushSupport,
};
use adam_a2a::{
    A2aServer, AgentCardConfig, AuthConfig, CardSigner, ExtendedCardConfig, ExtensionConfig,
    InMemoryBackend, InMemoryConfig, ServerOptions, SkillConfig,
};
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use secrecy::SecretString;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

pub const TOKEN: &str = "test-token-alpha";
pub const OTHER_TOKEN: &str = "test-token-beta";

pub type Client = A2AClient<Box<dyn Transport>>;

pub fn bearer() -> AuthConfig {
    AuthConfig::BearerTokens(vec![
        SecretString::from(TOKEN),
        SecretString::from(OTHER_TOKEN),
    ])
}

/// What a test server is built with.
pub struct Setup {
    pub auth: AuthConfig,
    pub push: Option<PushSupport>,
    pub signer: Option<CardSigner>,
    pub extended: Option<ExtendedCardConfig>,
    pub backend: InMemoryBackend,
    /// Swagger UI and the OpenAPI document (on by default, as in `ServerOptions`).
    pub docs: bool,
    /// Extensions the public card declares.
    pub extensions: Vec<ExtensionConfig>,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            auth: bearer(),
            push: None,
            signer: None,
            extended: None,
            backend: InMemoryBackend::with_config(InMemoryConfig {
                step_delay: Duration::from_millis(15),
            }),
            docs: true,
            extensions: Vec::new(),
        }
    }
}

pub struct TestServer {
    pub addr: SocketAddr,
    pub backend: InMemoryBackend,
    handle: JoinHandle<()>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl TestServer {
    pub async fn start(setup: Setup) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut card = AgentCardConfig::new(
            "echo-agent",
            "Echoes messages",
            format!("http://{addr}/").parse().unwrap(),
            "0.1.0",
        )
        .with_skill(SkillConfig::new("echo", "Echo", "Repeats what it is told"));
        if let Some(extended) = setup.extended {
            card = card.with_extended_card(extended);
        }
        for extension in setup.extensions {
            card = card.with_extension(extension);
        }
        let mut options = ServerOptions::default().with_docs(setup.docs);
        if let Some(push) = setup.push {
            options = options.with_push(push);
        }
        if let Some(signer) = setup.signer {
            options = options.with_card_signer(signer);
        }
        let app = A2aServer::router_with_options(
            card,
            Arc::new(setup.backend.clone()),
            setup.auth,
            options,
        );
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            addr,
            backend: setup.backend,
            handle,
        }
    }

    pub fn base(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub async fn client(&self, token: Option<&str>) -> Client {
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

    /// The official client, preferring the card's HTTP+JSON interface, and the interface it chose.
    pub async fn rest_client(&self, token: Option<&str>) -> (Client, AgentInterface) {
        let card = AgentCardResolver::new(None)
            .resolve(&self.base())
            .await
            .unwrap();
        let mut factory =
            A2AClientFactory::builder().preferred_bindings(vec!["HTTP+JSON".to_owned()]);
        if let Some(token) = token {
            factory = factory.with_interceptor(Arc::new(AuthInterceptor::bearer(token)));
        }
        factory
            .build()
            .create_from_card_with_interface(&card)
            .await
            .unwrap()
    }

    /// A plain HTTP call: `(status, headers, body)`. `token` goes in `Authorization: Bearer`.
    pub async fn http(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<&serde_json::Value>,
    ) -> (u16, reqwest::header::HeaderMap, String) {
        self.http_with(
            method,
            path,
            token,
            &[],
            body.map(|b| ("application/json", b.to_string())),
        )
        .await
    }

    /// [`http`](Self::http) with extra headers and the body's content type under the test's control.
    pub async fn http_with(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        headers: &[(&str, &str)],
        body: Option<(&str, String)>,
    ) -> (u16, reqwest::header::HeaderMap, String) {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
        let mut request = client.request(method, format!("{}{path}", self.base()));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        if let Some((content_type, body)) = body {
            request = request.header("content-type", content_type).body(body);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        (status, headers, response.text().await.unwrap())
    }
}

pub fn text_message(text: &str) -> Message {
    Message::new(Role::User, vec![Part::text(text)])
}

pub fn send(text: &str, context: Option<&str>) -> SendMessageRequest {
    let mut message = text_message(text);
    message.context_id = context.map(str::to_owned);
    SendMessageRequest {
        message,
        configuration: Some(SendMessageConfiguration {
            accepted_output_modes: None,
            task_push_notification_config: None,
            history_length: None,
            return_immediately: Some(true),
        }),
        metadata: None,
        tenant: None,
    }
}

pub fn task_of(response: SendMessageResponse) -> Task {
    match response {
        SendMessageResponse::Task(task) => task,
        other => panic!("expected a task, got {other:?}"),
    }
}

pub fn get(id: &str) -> GetTaskRequest {
    GetTaskRequest {
        id: id.to_owned(),
        history_length: None,
        tenant: None,
    }
}

pub async fn wait_for_state(client: &Client, id: &str, state: TaskState) -> Task {
    for _ in 0..400 {
        let task = client.get_task(&get(id)).await.unwrap();
        if task.status.state == state {
            return task;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("task {id} never reached {state:?}");
}

// ------------------------------------------------------------------ webhook

/// One request the webhook received.
#[derive(Clone, Debug)]
pub struct Received {
    pub headers: Vec<(String, String)>,
    pub body: serde_json::Value,
    /// What the webhook answered.
    pub answered: u16,
}

impl Received {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// `status:TASK_STATE_WORKING`, `artifact:<text>` or `other`.
    pub fn label(&self) -> String {
        if let Some(s) = self.body.get("statusUpdate") {
            return format!("status:{}", s["status"]["state"].as_str().unwrap_or("?"));
        }
        if let Some(a) = self.body.get("artifactUpdate") {
            return format!(
                "artifact:{}",
                a["artifact"]["parts"][0]["text"].as_str().unwrap_or("?")
            );
        }
        "other".to_owned()
    }

    pub fn task_id(&self) -> Option<&str> {
        ["statusUpdate", "artifactUpdate"]
            .iter()
            .find_map(|k| self.body.get(*k))
            .and_then(|v| v["taskId"].as_str())
    }
}

#[derive(Default)]
struct WebhookState {
    received: Mutex<Vec<Received>>,
    script: Mutex<VecDeque<u16>>,
    default_status: Mutex<Option<u16>>,
    /// How long to hold each request before answering it.
    delay: Mutex<Duration>,
}

/// A local webhook: records every request, answers from a script and then a default status.
#[derive(Clone)]
pub struct Webhook {
    pub addr: SocketAddr,
    state: Arc<WebhookState>,
}

impl Webhook {
    pub async fn start() -> Self {
        let state = Arc::new(WebhookState::default());
        *state.default_status.lock().unwrap() = Some(200);
        let app = Router::new()
            .route("/hook", post(record))
            .route("/other/{id}", post(record))
            .with_state(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { addr, state }
    }

    pub fn url(&self) -> String {
        format!("http://{}/hook", self.addr)
    }

    /// Answer every request that is not scripted with `status`.
    pub fn answer_with(&self, status: u16) {
        *self.state.default_status.lock().unwrap() = Some(status);
    }

    /// Hold every request for `delay` before answering it (a slow webhook).
    pub fn answer_after(&self, delay: Duration) {
        *self.state.delay.lock().unwrap() = delay;
    }

    /// Answer the next requests with these statuses, then the default.
    pub fn script(&self, statuses: impl IntoIterator<Item = u16>) {
        self.state.script.lock().unwrap().extend(statuses);
    }

    pub fn all(&self) -> Vec<Received> {
        self.state.received.lock().unwrap().clone()
    }

    /// The requests the webhook accepted (answered 2xx), in order.
    pub fn accepted(&self) -> Vec<Received> {
        self.all()
            .into_iter()
            .filter(|r| (200..300).contains(&r.answered))
            .collect()
    }

    pub fn labels(&self) -> Vec<String> {
        self.accepted().iter().map(Received::label).collect()
    }

    /// Wait until `n` requests were accepted.
    pub async fn wait_accepted(&self, n: usize) -> Vec<Received> {
        for _ in 0..1000 {
            let accepted = self.accepted();
            if accepted.len() >= n {
                return accepted;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "the webhook never accepted {n} requests; it got {:?}",
            self.all()
                .iter()
                .map(|r| (r.label(), r.answered))
                .collect::<Vec<_>>()
        );
    }
}

async fn record(
    State(state): State<Arc<WebhookState>>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let answered = state
        .script
        .lock()
        .unwrap()
        .pop_front()
        .or(*state.default_status.lock().unwrap())
        .unwrap_or(200);
    let delay = *state.delay.lock().unwrap();
    // Recorded on arrival, answered after the delay: a request in flight is already counted.
    state.received.lock().unwrap().push(Received {
        headers: headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("<binary>").to_owned()))
            .collect(),
        body: serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
        answered,
    });
    tokio::time::sleep(delay).await;
    StatusCode::from_u16(answered).unwrap()
}

// --------------------------------------------------------------- deliverers

/// Policy allowing the local webhooks (plain http on loopback, which needs the dev switch).
pub fn local_policy(webhooks: &[&Webhook]) -> PushPolicy {
    PushPolicy::new(
        webhooks
            .iter()
            .map(|w| format!("127.0.0.1:{}", w.addr.port())),
    )
    .unwrap()
    .allow_private_addresses(true)
}

/// Delivery tuned for tests: short polls and backoff.
pub fn fast_options() -> PushDeliveryOptions {
    PushDeliveryOptions::new()
        .with_poll_interval(Duration::from_millis(10))
        .with_backoff(Duration::from_millis(10), Duration::from_millis(40))
        .with_give_up_after(Duration::from_secs(30))
        .with_request_timeout(Duration::from_secs(5))
}

/// A running delivery loop; dropping it (or `stop`) ends it, as a restart would.
pub struct Delivery {
    stop: Option<oneshot::Sender<()>>,
    handle: JoinHandle<()>,
}

impl Delivery {
    pub fn start(deliverer: PushDeliverer) -> Self {
        let (stop, rx) = oneshot::channel();
        let handle = tokio::spawn(async move {
            deliverer
                .run(async {
                    let _ = rx.await;
                })
                .await;
        });
        Self {
            stop: Some(stop),
            handle,
        }
    }

    pub async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let _ = (&mut self.handle).await;
    }
}

/// Push support over a fresh in-memory store that allows the given local webhooks.
pub fn local_push(webhooks: &[&Webhook]) -> (PushSupport, InMemoryPushStore) {
    let store = InMemoryPushStore::new();
    (
        PushSupport::new(Arc::new(store.clone()), local_policy(webhooks)),
        store,
    )
}

pub fn stream_label(item: &StreamResponse) -> String {
    match item {
        StreamResponse::Task(t) => format!("task:{:?}", t.status.state),
        StreamResponse::Message(_) => "message".to_owned(),
        StreamResponse::StatusUpdate(u) => format!("status:{:?}", u.status.state),
        StreamResponse::ArtifactUpdate(_) => "artifact".to_owned(),
    }
}
