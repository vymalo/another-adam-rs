//! The service over the in-memory store with a tiny scripted agent: what `Service` composes
//! (the runtime, the A2A backend and the router) without Postgres.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use a2a::{Message, Part, Role, Task, TaskState};
use adam_a2a::{AgentCardConfig, AuthConfig, Caller, TaskBackend as _};
use adam_core::{DynStore, MemoryStore};
use adam_runtime::{
    Agent, AgentError, AgentStarter, Ctx, Inbound, RunEvent, Runtime, RuntimeBuilder, Transition,
};
use adam_service::{LiveSignals, RuntimeOptions, Service, router};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const TOKEN: &str = "service-test-token";

/// Answers `<name> says: <what it was asked>`, in one step.
struct Echo(String);

/// What starting a run of [`Echo`] needs, without the agent: a control plane registers this.
struct EchoStarter(String);

fn start(input: &Inbound) -> Value {
    json!({"text": input.payload["text"].as_str().unwrap_or_default()})
}

impl AgentStarter for EchoStarter {
    type State = Value;

    fn name(&self) -> &str {
        &self.0
    }

    fn init(&self, input: Inbound) -> Result<Value, AgentError> {
        Ok(start(&input))
    }
}

#[async_trait]
impl Agent for Echo {
    type State = Value;

    fn name(&self) -> &str {
        &self.0
    }

    fn init(&self, input: Inbound) -> Result<Value, AgentError> {
        Ok(start(&input))
    }

    async fn step(&self, ctx: &mut Ctx, state: Value) -> Result<Transition<Value>, AgentError> {
        ctx.emit(RunEvent::Progress {
            message: "echoing".into(),
        })
        .await;
        let text = state["text"].as_str().unwrap_or_default().to_owned();
        Ok(Transition::Done {
            state,
            output: json!({"text": format!("{} says: {text}", self.0)}),
        })
    }
}

fn options() -> RuntimeOptions {
    RuntimeOptions {
        poll_interval: Duration::from_millis(10),
        ..RuntimeOptions::default()
    }
}

fn card(name: &str) -> AgentCardConfig {
    AgentCardConfig::new(
        name,
        "A test agent.",
        "http://agent.test:8080/".parse().unwrap(),
        "0.0.1",
    )
}

fn user(text: &str) -> Message {
    Message::new(Role::User, vec![Part::text(text)])
}

fn alice() -> Caller {
    Caller::new("token-0")
}

/// The service for the agent `name` over `store`: the whole agent, or its starter only.
fn service(store: &DynStore, name: &str, whole: bool) -> Service {
    let builder: RuntimeBuilder = Runtime::builder(store.clone());
    let builder = if whole {
        builder.agent(Echo(name.to_owned()))
    } else {
        builder.starter(EchoStarter(name.to_owned()))
    };
    Service::new(builder, name, &options())
}

/// Run the workers of `service` until the returned sender is used.
fn spawn_worker(
    service: &Service,
) -> (
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let (stop, rx) = tokio::sync::oneshot::channel::<()>();
    let runtime = service.runtime.clone();
    let handle = tokio::spawn(async move {
        let _ = runtime
            .run_worker(async {
                let _ = rx.await;
            })
            .await;
    });
    (stop, handle)
}

async fn wait_for(service: &Service, caller: &Caller, id: &str, state: TaskState) -> Task {
    for _ in 0..1000 {
        let task = service
            .backend
            .get(caller, id)
            .await
            .expect("get")
            .expect("the task exists");
        if task.status.state == state {
            return task;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("task {id} never reached {state:?}");
}

/// The router served on a port of its own (port 0: the system picks a free one).
async fn serve_router(router: axum::Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    addr
}

/// One raw HTTP/1.1 request: `(status, body)`.
async fn raw(addr: SocketAddr, method: &str, path: &str, bearer: Option<&str>) -> (u16, String) {
    let auth = bearer.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(
            format!(
                "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\
                 Content-Length: 0\r\n{auth}\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut response = String::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_string(&mut response))
        .await
        .expect("a response")
        .unwrap();
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("a status line");
    (status, response)
}

/// The router serves the card and `/healthz` to anyone, and refuses a call without the token.
#[tokio::test]
async fn the_router_serves_the_card_and_health_and_closes_the_rest() {
    let store: DynStore = Arc::new(MemoryStore::new());
    let service = service(&store, "echo", true);
    let addr = serve_router(service.router(
        card("Echo"),
        AuthConfig::BearerTokens(vec![TOKEN.to_owned().into()]),
    ))
    .await;

    let (status, body) = raw(addr, "GET", "/.well-known/agent-card.json", None).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"name\":\"Echo\""), "{body}");
    assert!(body.contains("http://agent.test:8080/"), "{body}");
    assert_eq!(raw(addr, "GET", "/healthz", None).await.0, 200);
    assert_eq!(raw(addr, "POST", "/", None).await.0, 401, "no token");
    assert_eq!(
        raw(addr, "POST", "/", Some("wrong")).await.0,
        401,
        "wrong token"
    );
    assert_eq!(
        raw(addr, "POST", "/", Some(&TOKEN[..1])).await.0,
        401,
        "a prefix"
    );
}

/// A task is a run: submitted over the backend, stepped by the service's worker, and read back.
#[tokio::test]
async fn a_task_completes_through_the_service() {
    let store: DynStore = Arc::new(MemoryStore::new());
    let service = service(&store, "echo", true);
    let (stop, worker) = spawn_worker(&service);

    let task = service
        .backend
        .submit(alice(), user("hello"), None, Some("ctx-1".into()))
        .await
        .expect("submit");
    assert_eq!(task.context_id, "ctx-1");
    let done = wait_for(&service, &alice(), &task.id, TaskState::Completed).await;
    let said = done
        .status
        .message
        .as_ref()
        .and_then(Message::text)
        .map(str::to_owned);
    assert_eq!(said.as_deref(), Some("echo says: hello"));

    // Another caller does not see it.
    assert!(
        service
            .backend
            .get(&Caller::new("token-1"), &task.id)
            .await
            .unwrap()
            .is_none()
    );
    let _ = stop.send(());
    tokio::time::timeout(Duration::from_secs(10), worker)
        .await
        .expect("the worker stops")
        .unwrap();
}

/// The two halves of one agent in two services over one store: a control plane that registers the
/// starter only starts the run, and a worker that registers the whole agent steps it. They meet in
/// the store, with no signal between them (`LiveSignals::local` shares nothing).
#[tokio::test]
async fn a_control_plane_and_a_worker_meet_in_the_store() {
    let store: DynStore = Arc::new(MemoryStore::new());
    let front = service(&store, "echo", false);
    let back = service(&store, "echo", true);

    let task = front
        .backend
        .submit(alice(), user("split"), None, None)
        .await
        .expect("the front starts the run");
    // Nothing steps it here: the front's worker claims nothing.
    let (stop_front, front_worker) = spawn_worker(&front);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let waiting = front
        .backend
        .get(&alice(), &task.id)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(waiting.status.state, TaskState::Completed);
    let _ = stop_front.send(());
    front_worker.await.unwrap();

    let (stop, worker) = spawn_worker(&back);
    let done = wait_for(&front, &alice(), &task.id, TaskState::Completed).await;
    assert_eq!(
        done.status.message.as_ref().and_then(Message::text),
        Some("echo says: split")
    );
    let _ = stop.send(());
    worker.await.unwrap();
}

/// Two agents in one database, each in a service under its own name: a service serves its agent
/// only, and its worker steps only the runs of the agents it registered.
#[tokio::test]
async fn two_services_of_different_names_share_a_store_without_stealing_runs() {
    let store: DynStore = Arc::new(MemoryStore::new());
    let one = service(&store, "one", true);
    let two = service(&store, "two", true);
    let (stop_one, worker_one) = spawn_worker(&one);
    let (stop_two, worker_two) = spawn_worker(&two);

    let task_one = one
        .backend
        .submit(alice(), user("a"), None, None)
        .await
        .unwrap();
    let task_two = two
        .backend
        .submit(alice(), user("b"), None, None)
        .await
        .unwrap();
    let done_one = wait_for(&one, &alice(), &task_one.id, TaskState::Completed).await;
    let done_two = wait_for(&two, &alice(), &task_two.id, TaskState::Completed).await;
    assert_eq!(
        done_one.status.message.as_ref().and_then(Message::text),
        Some("one says: a")
    );
    assert_eq!(
        done_two.status.message.as_ref().and_then(Message::text),
        Some("two says: b")
    );
    // A task of the other agent is not this service's to show.
    assert!(
        one.backend
            .get(&alice(), &task_two.id)
            .await
            .map(|t| t.is_none())
            .unwrap_or(true),
        "service `one` shows a run of `two`"
    );
    for (stop, worker) in [(stop_one, worker_one), (stop_two, worker_two)] {
        let _ = stop.send(());
        worker.await.unwrap();
    }
}

/// The free `router` function is the one `Service::router` calls, for a composition that keeps the
/// runtime and the backend itself.
#[tokio::test]
async fn the_router_function_serves_the_backend_it_is_given() {
    let store: DynStore = Arc::new(MemoryStore::new());
    let service = service(&store, "echo", true);
    let addr = serve_router(router(
        &service.backend,
        card("Echo"),
        AuthConfig::BearerTokens(vec![TOKEN.to_owned().into()]),
    ))
    .await;
    assert_eq!(raw(addr, "GET", "/healthz", None).await.0, 200);
}

/// `Service::new_with` takes the live signals it is given: the events of a run reach the
/// broadcast the caller made, which is what a process that learns of other processes through
/// Postgres puts its own sink in front of.
#[tokio::test]
async fn new_with_uses_the_live_signals_it_is_given() {
    let store: DynStore = Arc::new(MemoryStore::new());
    let live = LiveSignals::local();
    let broadcast = live.broadcast.clone();
    let builder = Runtime::builder(store).agent(Echo("echo".into()));
    let service = Service::new_with(builder, "echo", &options(), live);

    let task = service
        .backend
        .submit(alice(), user("signals"), None, None)
        .await
        .unwrap();
    let mut events = broadcast.subscribe_run(adam_core::RunId(task.id.parse().unwrap()));
    let (stop, worker) = spawn_worker(&service);
    let first = tokio::time::timeout(Duration::from_secs(10), events.recv())
        .await
        .expect("an event");
    assert_eq!(
        first,
        Some(RunEvent::Progress {
            message: "echoing".into()
        })
    );
    wait_for(&service, &alice(), &task.id, TaskState::Completed).await;
    let _ = stop.send(());
    worker.await.unwrap();
}
