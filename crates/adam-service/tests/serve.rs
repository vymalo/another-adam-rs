//! `serve`: the whole process, in this process. The cases that need a database use
//! `ADAM_TEST_POSTGRES_URL` (and are skipped without it, or fail with `ADAM_TEST_REQUIRE_DB=1`);
//! the ones that fail before a database is needed run anywhere. Every case names its agent
//! apart, so cases that share a database never claim each other's runs.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::net::SocketAddr;
use std::time::Duration;

use a2a::{Message, Part, Role, TaskState};
use adam_a2a::{AgentCardConfig, Caller, TaskBackend as _};
use adam_core::{DynStore, RunId};
use adam_runtime::{Agent, AgentError, AgentStarter, Ctx, Inbound, Runtime, Transition};
use adam_service::{
    Agents, ConfigError, RuntimeOptions, ServeError, Service, ServiceConfig, exit_code,
};
use adam_store_postgres::PgStore;
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Answers `<name> says: <what it was asked>`, in one step.
struct Echo(String);

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

    async fn step(&self, _ctx: &mut Ctx, state: Value) -> Result<Transition<Value>, AgentError> {
        let text = state["text"].as_str().unwrap_or_default().to_owned();
        Ok(Transition::Done {
            state,
            output: json!({"text": format!("{} says: {text}", self.0)}),
        })
    }
}

fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", RunId::new().0.simple())
}

/// A port nobody listens on right now. (Another process may take it before it is used: the cases
/// that use it retry nothing and fail loudly if it happens.)
async fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

fn config(database: &str, role: &str, port: u16) -> ServiceConfig {
    let vars = [
        ("DATABASE_URL", database.to_owned()),
        ("ROLE", role.to_owned()),
        ("A2A_BEARER_TOKENS", "serve-test-token".to_owned()),
        ("PUBLIC_URL", "http://agent.test:8080/".to_owned()),
        ("LISTEN_ADDR", format!("127.0.0.1:{port}")),
    ];
    let mut problems = Vec::new();
    let config = ServiceConfig::parse(
        &|name: &str| {
            vars.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.clone())
        },
        &mut problems,
    );
    ConfigError::check(problems).expect("a valid configuration");
    config
}

fn card(name: &str) -> AgentCardConfig {
    AgentCardConfig::new(
        name,
        "A test agent.",
        "http://agent.test:8080/".parse().unwrap(),
        "0.0.1",
    )
}

/// The agents of a process for `role`: the whole agent where workers run, its starter otherwise.
fn agents(name: &str, role: &ServiceConfig) -> Agents {
    let n = name.to_owned();
    let agents = if role.role.runs_workers() {
        Agents::new(name, move |b| b.agent(Echo(n)))
    } else {
        Agents::new(name, move |b| b.starter(EchoStarter(n)))
    };
    agents
        .card_if(role.role.runs_control_plane().then(|| card(name)))
        .options(RuntimeOptions {
            poll_interval: Duration::from_millis(20),
            ..RuntimeOptions::default()
        })
}

/// One raw HTTP/1.1 request: the status.
async fn status_of(addr: SocketAddr, path: &str) -> Option<u16> {
    let mut stream = TcpStream::connect(addr).await.ok()?;
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .ok()?;
    let mut response = String::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_string(&mut response))
        .await
        .ok()?
        .ok()?;
    response.split_whitespace().nth(1)?.parse().ok()
}

/// Wait until `path` answers 200 at `addr`.
async fn until_ok(
    addr: SocketAddr,
    path: &str,
    running: &tokio::task::JoinHandle<Result<(), ServeError>>,
) {
    for _ in 0..600 {
        assert!(
            !running.is_finished(),
            "serve ended before it answered {path}"
        );
        if status_of(addr, path).await == Some(200) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{path} never answered 200");
}

struct Running {
    stop: tokio::sync::oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<Result<(), ServeError>>,
}

impl Running {
    /// Serve until `finish`.
    fn start(config: ServiceConfig, agents: Agents) -> Self {
        let (stop, rx) = tokio::sync::oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            adam_service::serve(&config, agents, async {
                let _ = rx.await;
            })
            .await
        });
        Self { stop, handle }
    }

    /// Ask it to stop and wait for it: a clean stop is `Ok`.
    async fn finish(self) {
        let _ = self.stop.send(());
        let result = tokio::time::timeout(Duration::from_secs(30), self.handle)
            .await
            .expect("serve stops within the drain")
            .expect("serve does not panic");
        result.expect("serve stops cleanly");
    }
}

fn database() -> Option<String> {
    adam_core::testing::test_env("ADAM_TEST_POSTGRES_URL")
}

/// A role that serves A2A is refused without a card, before anything connects (no database is
/// needed to see it), and the exit code says the deployment cannot fix it by waiting.
#[tokio::test]
async fn a_role_that_serves_a2a_needs_a_card_and_nothing_connects_first() {
    for role in ["all", "control-plane"] {
        let config = config("postgres://u:p@127.0.0.1:1/x", role, 0);
        let agents = Agents::new("echo", |b| b);
        let error = adam_service::serve(&config, agents, std::future::pending())
            .await
            .unwrap_err();
        assert!(matches!(error, ServeError::NoCard), "{role}: {error}");
        assert_eq!(exit_code(&error), 78, "{role}");
    }
}

/// Postgres that cannot be reached is `ServeError::Connect`, exit 69 (a supervisor retries), and
/// the message names the step and not the password.
#[tokio::test]
async fn a_database_that_is_down_is_a_connect_error_and_exit_69() {
    let config = config(
        "postgres://adam:s3cr3tpassw0rd@127.0.0.1:1/adam",
        "worker",
        0,
    );
    let error = adam_service::serve(&config, agents("echo", &config), std::future::pending())
        .await
        .unwrap_err();
    assert!(matches!(error, ServeError::Connect(_)), "{error:?}");
    assert_eq!(error.to_string(), "connecting to Postgres");
    assert_eq!(exit_code(&error), 69);
    assert!(
        !format!("{error:?}").contains("s3cr3tpassw0rd"),
        "{error:?}"
    );
}

/// Role `all`: the card and `/healthz` are served, a run started through another service over the
/// same database is stepped by this one's worker, and a shutdown ends it cleanly.
#[tokio::test]
async fn serve_runs_the_whole_agent_and_stops_on_shutdown() {
    let Some(url) = database() else { return };
    let name = unique("serve-all");
    let port = free_port().await;
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let running = Running::start(
        config(&url, "all", port),
        agents(&name, &config(&url, "all", port)),
    );
    until_ok(addr, "/healthz", &running.handle).await;
    assert_eq!(
        status_of(addr, "/.well-known/agent-card.json").await,
        Some(200)
    );

    // A front of another process: it starts the run, this process's worker steps it.
    let store: DynStore = std::sync::Arc::new(PgStore::connect(&url).await.unwrap());
    let front = Service::new(
        Runtime::builder(store).starter(EchoStarter(name.clone())),
        &name,
        &RuntimeOptions {
            poll_interval: Duration::from_millis(20),
            ..RuntimeOptions::default()
        },
    );
    let caller = Caller::new("token-0");
    let task = front
        .backend
        .submit(
            caller.clone(),
            Message::new(Role::User, vec![Part::text("over postgres")]),
            None,
            None,
        )
        .await
        .unwrap();
    let mut done = None;
    for _ in 0..600 {
        let task = front.backend.get(&caller, &task.id).await.unwrap().unwrap();
        if task.status.state == TaskState::Completed {
            done = Some(task);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let done = done.expect("the worker of `serve` completed the run");
    assert_eq!(
        done.status.message.as_ref().and_then(Message::text),
        Some(format!("{name} says: over postgres").as_str())
    );
    running.finish().await;
}

/// A worker serves `/healthz` and nothing else; a control plane serves A2A and steps nothing.
#[tokio::test]
async fn a_worker_serves_only_health_and_a_control_plane_serves_a2a() {
    let Some(url) = database() else { return };

    let name = unique("serve-worker");
    let port = free_port().await;
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let cfg = config(&url, "worker", port);
    let running = Running::start(config(&url, "worker", port), agents(&name, &cfg));
    until_ok(addr, "/healthz", &running.handle).await;
    assert_eq!(
        status_of(addr, "/.well-known/agent-card.json").await,
        Some(404)
    );
    running.finish().await;

    let name = unique("serve-front");
    let port = free_port().await;
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let cfg = config(&url, "control-plane", port);
    let running = Running::start(config(&url, "control-plane", port), agents(&name, &cfg));
    until_ok(addr, "/healthz", &running.handle).await;
    assert_eq!(
        status_of(addr, "/.well-known/agent-card.json").await,
        Some(200)
    );
    running.finish().await;
}

/// A component the binary adds (the coder's sweep of finished workspaces) runs in the roles that
/// run workers, with the store `serve` connected, and stops with the workers; a control plane does
/// not start it.
#[tokio::test]
async fn a_component_of_the_binary_runs_with_the_workers_and_stops_with_them() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    let Some(url) = database() else { return };
    for (role, runs) in [("all", true), ("worker", true), ("control-plane", false)] {
        let (started, stopped) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (on_start, on_stop) = (started.clone(), stopped.clone());
        let name = unique("serve-component");
        let port = free_port().await;
        let addr: SocketAddr = ([127, 0, 0, 1], port).into();
        let cfg = config(&url, role, port);
        let agents = agents(&name, &cfg).worker_component("probe", move |store, stop| async move {
            // The store is the one `serve` connected: this run is not there.
            assert!(store.load_run(RunId::new()).await?.is_none());
            on_start.fetch_add(1, SeqCst);
            stop.cancelled().await;
            on_stop.fetch_add(1, SeqCst);
            Ok(())
        });
        let running = Running::start(config(&url, role, port), agents);
        until_ok(addr, "/healthz", &running.handle).await;
        for _ in 0..100 {
            if started.load(SeqCst) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(started.load(SeqCst), usize::from(runs), "{role}");
        assert_eq!(
            stopped.load(SeqCst),
            0,
            "{role}: it runs until the host stops it"
        );
        running.finish().await;
        assert_eq!(stopped.load(SeqCst), usize::from(runs), "{role}");
    }
}

/// An address that is taken is `ServeError::Bind`, exit 71, and nothing is left running.
#[tokio::test]
async fn an_address_that_is_taken_is_a_bind_error_and_exit_71() {
    let Some(url) = database() else { return };
    let taken = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = taken.local_addr().unwrap().port();
    let cfg = config(&url, "worker", port);
    let error = adam_service::serve(
        &cfg,
        agents(&unique("serve-bind"), &cfg),
        std::future::pending(),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, ServeError::Bind { .. }), "{error:?}");
    assert_eq!(error.to_string(), format!("binding 127.0.0.1:{port}"));
    assert_eq!(exit_code(&error), 71);
}
