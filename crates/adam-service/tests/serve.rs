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
        if text.contains("[slow]") {
            // Long enough that the task is still running when a webhook is registered for it.
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        Ok(Transition::Done {
            state,
            output: json!({"text": format!("{} says: {text}", self.0)}),
        })
    }
}

fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", RunId::new().0.simple())
}

/// A listener on a port the system picked, and that port, for the configuration and the card.
/// `serve_on` takes the listener itself, so no other process can take the port in between.
async fn listen() -> (TcpListener, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

/// The helpers are checked before the database is: a misconfiguration is exit 78 even when no
/// database answers, not the 69 of a store that cannot be reached.
#[tokio::test]
async fn an_unregistered_helper_is_78_before_the_database_is_asked() {
    let name = unique("serve-sessions-early");
    // Port 1 is privileged and nothing in a test listens there: the connection is refused.
    let cfg = config("postgres://nobody:nothing@127.0.0.1:1/none", "all", 0);
    let helper = format!("{name}/helper");
    let agents = agents(&name, &cfg).child_sessions(sessions_for(&helper));
    // A `serve` that did not stop would never return: the bound fails the test instead of hanging it.
    let error = tokio::time::timeout(
        Duration::from_secs(20),
        adam_service::serve(&cfg, agents, std::future::pending()),
    )
    .await
    .expect("serve stops at once for a helper nobody registered")
    .unwrap_err();
    assert!(
        matches!(&error, ServeError::ChildSessions { agent } if *agent == helper),
        "{error}"
    );
    assert_eq!(exit_code(&error), 78);
}

fn config(database: &str, role: &str, port: u16) -> ServiceConfig {
    config_with(database, role, port, &[])
}

/// [`config`] with more variables of the environment.
fn config_with(database: &str, role: &str, port: u16, more: &[(&str, String)]) -> ServiceConfig {
    let mut vars = vec![
        ("DATABASE_URL", database.to_owned()),
        ("ROLE", role.to_owned()),
        ("A2A_BEARER_TOKENS", "serve-test-token".to_owned()),
        ("PUBLIC_URL", "http://agent.test:8080/".to_owned()),
        ("LISTEN_ADDR", format!("127.0.0.1:{port}")),
    ];
    vars.extend(more.iter().map(|(k, v)| (*k, v.clone())));
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
    /// Serve on `listener` until `finish`.
    fn start(listener: TcpListener, config: ServiceConfig, agents: Agents) -> Self {
        let (stop, rx) = tokio::sync::oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            adam_service::serve_on(&config, agents, listener, async {
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
    assert!(
        matches!(
            error,
            ServeError::Connect {
                backend: "Postgres",
                ..
            }
        ),
        "{error:?}"
    );
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
    let (listener, port) = listen().await;
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let running = Running::start(
        listener,
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

/// Role `all` over a SQLite file: the scheme picks the store, the card and `/healthz` are served,
/// and a message is stepped end to end. No database server, no `ADAM_TEST_POSTGRES_URL`.
#[tokio::test]
async fn serve_runs_the_whole_agent_on_a_sqlite_file() {
    use a2a::{GetTaskRequest, SendMessageRequest, SendMessageResponse};
    use a2a_client::A2AClientFactory;
    use a2a_client::agent_card::AgentCardResolver;
    use a2a_client::auth::AuthInterceptor;
    use std::sync::Arc;

    let name = unique("serve-sqlite");
    let dir = std::env::temp_dir().join(format!("adam-serve-sqlite-{}", RunId::new().0.simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let database = dir.join("adam.sqlite");
    let url = format!("sqlite://{}", database.display());
    let (listener, port) = listen().await;
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let cfg = config(&url, "all", port);
    let agents = agents(&name, &cfg).card(AgentCardConfig::new(
        &name,
        "A test agent.",
        format!("http://{addr}/").parse().unwrap(),
        "0.0.1",
    ));
    let running = Running::start(listener, cfg.clone(), agents);
    until_ok(addr, "/healthz", &running.handle).await;
    assert_eq!(
        status_of(addr, "/.well-known/agent-card.json").await,
        Some(200)
    );

    let base = format!("http://{addr}");
    let public = AgentCardResolver::new(None).resolve(&base).await.unwrap();
    let client = A2AClientFactory::builder()
        .with_interceptor(Arc::new(AuthInterceptor::bearer("serve-test-token")))
        .build()
        .create_from_card(&public)
        .await
        .unwrap();
    let sent = client
        .send_message(&SendMessageRequest {
            message: Message::new(Role::User, vec![Part::text("over sqlite")]),
            configuration: None,
            metadata: None,
            tenant: None,
        })
        .await
        .unwrap();
    let SendMessageResponse::Task(task) = sent else {
        panic!("expected a task")
    };
    let mut answer = None;
    for _ in 0..600 {
        let got = client
            .get_task(&GetTaskRequest {
                id: task.id.clone(),
                history_length: None,
                tenant: None,
            })
            .await
            .unwrap();
        if got.status.state == TaskState::Completed {
            answer = got
                .status
                .message
                .as_ref()
                .and_then(Message::text)
                .map(str::to_owned);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        answer.as_deref(),
        Some(format!("{name} says: over sqlite").as_str()),
        "the worker over SQLite completed the run"
    );
    running.finish().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// A worker serves `/healthz` and nothing else; a control plane serves A2A and steps nothing.
#[tokio::test]
async fn a_worker_serves_only_health_and_a_control_plane_serves_a2a() {
    let Some(url) = database() else { return };

    let name = unique("serve-worker");
    let (listener, port) = listen().await;
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let cfg = config(&url, "worker", port);
    let running = Running::start(listener, config(&url, "worker", port), agents(&name, &cfg));
    until_ok(addr, "/healthz", &running.handle).await;
    assert_eq!(
        status_of(addr, "/.well-known/agent-card.json").await,
        Some(404)
    );
    running.finish().await;

    let name = unique("serve-front");
    let (listener, port) = listen().await;
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let cfg = config(&url, "control-plane", port);
    let running = Running::start(
        listener,
        config(&url, "control-plane", port),
        agents(&name, &cfg),
    );
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
        let (listener, port) = listen().await;
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
        let running = Running::start(listener, config(&url, role, port), agents);
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

/// `serve_on` serves on the listener it is given and never binds `LISTEN_ADDR`: here that address
/// is taken, which would be `ServeError::Bind` if it were read.
#[tokio::test]
async fn serve_on_serves_the_listener_it_is_given_and_never_binds_listen_addr() {
    let Some(url) = database() else { return };
    let taken = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let cfg = config(&url, "worker", taken.local_addr().unwrap().port());
    let (listener, port) = listen().await;
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let running = Running::start(listener, cfg.clone(), agents(&unique("serve-on"), &cfg));
    until_ok(addr, "/healthz", &running.handle).await;
    running.finish().await;
    drop(taken);
}

/// What a deployment turns on is on, end to end over Postgres: the card says push notifications and
/// the extended card are on and carries a signature that verifies with the key set the server
/// publishes, `ListTasks` and `GetExtendedAgentCard` answer, and a webhook named in a message is
/// told when the task completes.
#[tokio::test]
async fn push_notifications_the_extended_card_and_the_signature_work_through_serve() {
    use std::sync::{Arc, Mutex};

    use a2a::{
        AuthenticationInfo, GetExtendedAgentCardRequest, ListTasksRequest,
        SendMessageConfiguration, SendMessageRequest, SendMessageResponse,
        TaskPushNotificationConfig,
    };
    use a2a_client::A2AClientFactory;
    use a2a_client::agent_card::AgentCardResolver;
    use a2a_client::auth::AuthInterceptor;
    use adam_a2a::{ExtendedCardConfig, SkillConfig};

    let Some(url) = database() else { return };

    // A webhook that records what it is sent.
    /// What the webhook saw: the `Authorization` header and the body.
    type Seen = Vec<(Option<String>, serde_json::Value)>;
    let received: Arc<Mutex<Seen>> = Arc::default();
    let sink = received.clone();
    let hook = axum::Router::new().route(
        "/hook",
        axum::routing::post(
            move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
                let sink = sink.clone();
                async move {
                    let auth = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned);
                    sink.lock()
                        .unwrap()
                        .push((auth, serde_json::from_slice(&body).unwrap_or_default()));
                    axum::http::StatusCode::OK
                }
            },
        ),
    );
    let hook_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hook_port = hook_listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(hook_listener, hook).await.unwrap();
    });

    // The key, as a Secret would mount it.
    let dir = std::env::temp_dir().join(format!("adam-serve-key-{}", RunId::new().0.simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let key_file = dir.join("card-signing.pem");
    std::fs::write(&key_file, adam_a2a::generate_signing_key_pem(false)).unwrap();

    let name = unique("serve-features");
    let (listener, port) = listen().await;
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let cfg = config_with(
        &url,
        "all",
        port,
        &[
            ("A2A_PUSH_ALLOWED_URLS", format!("127.0.0.1:{hook_port}")),
            ("A2A_PUSH_ALLOW_PRIVATE", "true".to_owned()),
            ("A2A_CARD_SIGNING_KEY_FILE", key_file.display().to_string()),
        ],
    );
    let verifying = cfg
        .a2a
        .card_signing
        .as_ref()
        .expect("signing is configured")
        .signer
        .verifying_key();
    let extended = ExtendedCardConfig::new().with_skill(SkillConfig::new(
        "audit",
        "Audit",
        "For the signed in",
    ));
    let agents = agents(&name, &cfg)
        .card(
            AgentCardConfig::new(
                &name,
                "A test agent.",
                format!("http://{addr}/").parse().unwrap(),
                "0.0.1",
            )
            .with_extended_card(extended),
        )
        .options(RuntimeOptions {
            poll_interval: Duration::from_millis(20),
            ..RuntimeOptions::default()
        });
    let running = Running::start(listener, cfg, agents);
    until_ok(addr, "/healthz", &running.handle).await;

    let base = format!("http://{addr}");
    let public = AgentCardResolver::new(None).resolve(&base).await.unwrap();
    assert_eq!(public.capabilities.push_notifications, Some(true));
    assert_eq!(public.capabilities.extended_agent_card, Some(true));
    assert_eq!(
        verifying.verify_card(&public),
        Ok(()),
        "the card is signed by the configured key"
    );
    // The key set is served and verifies it too.
    assert_eq!(status_of(addr, "/.well-known/jwks.json").await, Some(200));

    let client = A2AClientFactory::builder()
        .with_interceptor(Arc::new(AuthInterceptor::bearer("serve-test-token")))
        .build()
        .create_from_card(&public)
        .await
        .unwrap();
    let extended = client
        .get_extended_agent_card(&GetExtendedAgentCardRequest { tenant: None })
        .await
        .unwrap();
    assert!(extended.skills.iter().any(|s| s.id == "audit"));
    assert_eq!(verifying.verify_card(&extended), Ok(()));

    let message = Message::new(Role::User, vec![Part::text("tell the webhook [slow]")]);
    let response = client
        .send_message(&SendMessageRequest {
            message,
            configuration: Some(SendMessageConfiguration {
                accepted_output_modes: None,
                task_push_notification_config: Some(TaskPushNotificationConfig {
                    url: format!("http://127.0.0.1:{hook_port}/hook"),
                    id: None,
                    task_id: String::new(),
                    token: None,
                    authentication: Some(AuthenticationInfo {
                        scheme: "Bearer".into(),
                        credentials: Some("hook-secret".into()),
                    }),
                    tenant: None,
                }),
                history_length: None,
                return_immediately: Some(true),
            }),
            metadata: None,
            tenant: None,
        })
        .await
        .unwrap();
    let SendMessageResponse::Task(task) = response else {
        panic!("expected a task")
    };
    let mut told = false;
    for _ in 0..600 {
        told = received
            .lock()
            .unwrap()
            .iter()
            .any(|(_, body)| body["statusUpdate"]["status"]["state"] == "TASK_STATE_COMPLETED");
        if told {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        told,
        "the webhook was told the task completed: {:?}",
        received.lock().unwrap()
    );
    assert!(
        received
            .lock()
            .unwrap()
            .iter()
            .all(|(auth, body)| auth.as_deref() == Some("Bearer hook-secret")
                && body["statusUpdate"]["taskId"] == task.id.as_str()),
        "every notification carried the credentials and named the task"
    );

    let listed = client
        .list_tasks(&ListTasksRequest {
            context_id: None,
            status: Some(TaskState::Completed),
            page_size: None,
            page_token: None,
            history_length: None,
            status_timestamp_after: None,
            include_artifacts: None,
            tenant: None,
        })
        .await
        .unwrap();
    assert!(listed.tasks.iter().any(|t| t.id == task.id));
    running.finish().await;
    let _ = std::fs::remove_dir_all(dir);
}

/// Without the variables, none of it is on: no push, no signature, no key set.
#[tokio::test]
async fn nothing_optional_is_on_unless_the_environment_says_so() {
    use a2a_client::agent_card::AgentCardResolver;
    let Some(url) = database() else { return };
    let name = unique("serve-plain");
    let (listener, port) = listen().await;
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let cfg = config(&url, "control-plane", port);
    let running = Running::start(
        listener,
        config(&url, "control-plane", port),
        agents(&name, &cfg),
    );
    until_ok(addr, "/healthz", &running.handle).await;
    let card = AgentCardResolver::new(None)
        .resolve(&format!("http://{addr}"))
        .await
        .unwrap();
    assert_eq!(card.capabilities.push_notifications, Some(false));
    assert_eq!(card.capabilities.extended_agent_card, Some(false));
    assert!(card.signatures.is_none());
    assert_eq!(status_of(addr, "/.well-known/jwks.json").await, Some(401));
    // The docs are the exception: on, and public, unless `A2A_DOCS=false`.
    assert_eq!(status_of(addr, "/openapi.json").await, Some(200));
    assert_eq!(status_of(addr, "/docs").await, Some(303));
    running.finish().await;
}

/// `A2A_DOCS=false`: no Swagger UI and no OpenAPI document; the routes are closed like any other.
#[tokio::test]
async fn a2a_docs_false_turns_the_docs_off() {
    let Some(url) = database() else { return };
    let name = unique("serve-nodocs");
    let (listener, port) = listen().await;
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let more = [("A2A_DOCS", "false".to_owned())];
    let cfg = config_with(&url, "control-plane", port, &more);
    let running = Running::start(
        listener,
        config_with(&url, "control-plane", port, &more),
        agents(&name, &cfg),
    );
    until_ok(addr, "/healthz", &running.handle).await;
    for path in ["/openapi.json", "/docs", "/docs/"] {
        assert_eq!(status_of(addr, path).await, Some(401), "{path}");
    }
    running.finish().await;
}

/// `Agents::commands` reaches the backend through `serve` itself: a message `/name x` is stored as
/// the expansion, and the card lists `commands/v1` exactly when commands were given.
#[tokio::test]
async fn serve_expands_the_commands_it_is_given_and_the_card_lists_them() {
    use std::sync::Arc;

    use a2a::{GetTaskRequest, SendMessageRequest, SendMessageResponse};
    use a2a_client::A2AClientFactory;
    use a2a_client::agent_card::AgentCardResolver;
    use a2a_client::auth::AuthInterceptor;
    use adam_a2a::{COMMANDS_EXTENSION, CommandInfo, ExtensionConfig};
    use adam_a2a_runtime::Commands;

    let Some(url) = database() else { return };

    for served in [true, false] {
        let name = unique("serve-commands");
        let (listener, port) = listen().await;
        let addr: SocketAddr = ([127, 0, 0, 1], port).into();
        let cfg = config(&url, "all", port);
        let mut card = AgentCardConfig::new(
            &name,
            "A test agent.",
            format!("http://{addr}/").parse().unwrap(),
            "0.0.1",
        );
        let mut agents = agents(&name, &cfg);
        if served {
            let info = CommandInfo {
                name: "echo".into(),
                description: "Echo.".into(),
                argument_hint: None,
            };
            card = card.with_extension(ExtensionConfig::commands(&[info]).unwrap());
            agents = agents.commands(Commands::new([("echo", "[$ARGUMENTS]")]));
        }
        let running = Running::start(listener, cfg, agents.card(card));
        until_ok(addr, "/healthz", &running.handle).await;

        let public = AgentCardResolver::new(None)
            .resolve(&format!("http://{addr}"))
            .await
            .unwrap();
        let listed = public
            .capabilities
            .extensions
            .as_ref()
            .is_some_and(|all| all.iter().any(|e| e.uri == COMMANDS_EXTENSION));
        assert_eq!(
            listed, served,
            "the card lists the extension iff commands are served"
        );

        let client = A2AClientFactory::builder()
            .with_interceptor(Arc::new(AuthInterceptor::bearer("serve-test-token")))
            .build()
            .create_from_card(&public)
            .await
            .unwrap();
        let sent = client
            .send_message(&SendMessageRequest {
                message: Message::new(Role::User, vec![Part::text("/echo x")]),
                configuration: None,
                metadata: None,
                tenant: None,
            })
            .await
            .unwrap();
        let SendMessageResponse::Task(task) = sent else {
            panic!("expected a task")
        };
        let mut answer = None;
        for _ in 0..600 {
            let got = client
                .get_task(&GetTaskRequest {
                    id: task.id.clone(),
                    history_length: None,
                    tenant: None,
                })
                .await
                .unwrap();
            if got.status.state == TaskState::Completed {
                answer = got
                    .status
                    .message
                    .as_ref()
                    .and_then(Message::text)
                    .map(str::to_owned);
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let wanted = if served {
            format!("{name} says: [x]")
        } else {
            format!("{name} says: /echo x")
        };
        assert_eq!(answer.as_deref(), Some(wanted.as_str()), "served: {served}");
        running.finish().await;
    }
}

/// A runtime on the memory store with the agent `name` and the helpers registered as starters.
fn service_with(name: &str, helpers: &[&str]) -> Service {
    let mut builder = Runtime::builder(std::sync::Arc::new(adam_core::MemoryStore::new()))
        .starter(EchoStarter(name.to_owned()));
    for helper in helpers {
        builder = builder.starter(EchoStarter((*helper).to_owned()));
    }
    Service::new(builder, name, &RuntimeOptions::default())
}

fn sessions_for(registered: &str) -> adam_a2a_runtime::ChildSessions {
    use adam_a2a_runtime::{ChildSessionAgent, ChildSessions};
    ChildSessions::new([ChildSessionAgent::new("helper", registered, "Helps.")], 8).unwrap()
}

/// `Service::with_child_sessions` hands the sessions to the backend only when every helper is
/// registered on the runtime; `None` changes nothing.
#[tokio::test]
async fn a_service_serves_child_sessions_only_when_every_helper_is_registered() {
    let name = unique("svc-sessions");
    let helper = format!("{name}/helper");

    let none = service_with(&name, &[&helper]).with_child_sessions(None);
    assert!(none.child_sessions().is_none());

    let served = service_with(&name, &[&helper]).with_child_sessions(Some(sessions_for(&helper)));
    assert_eq!(served.child_sessions(), Some(&sessions_for(&helper)));

    let missing = service_with(&name, &[]).with_child_sessions(Some(sessions_for(&helper)));
    assert!(
        missing.child_sessions().is_none(),
        "a helper nobody registered"
    );
}

/// The card lists `child-sessions/v1` exactly when `Agents::child_sessions` is given and served,
/// with the limits' `maxOpen`; `serve` adds it, not the binary's card.
#[tokio::test]
async fn serve_lists_child_sessions_on_the_card_exactly_when_it_serves_them() {
    use a2a_client::agent_card::AgentCardResolver;
    use adam_a2a::CHILD_SESSIONS_EXTENSION;

    let Some(url) = database() else { return };

    for role in ["all", "control-plane"] {
        for served in [true, false] {
            let name = unique("serve-sessions");
            let helper = format!("{name}/helper");
            let (listener, port) = listen().await;
            let addr: SocketAddr = ([127, 0, 0, 1], port).into();
            let cfg = config(&url, role, port);
            let (n, h) = (name.clone(), helper.clone());
            let register = cfg.role.runs_workers();
            let mut agents = Agents::new(name.as_str(), move |b| {
                if register {
                    b.agent(Echo(n)).agent(Echo(h))
                } else {
                    b.starter(EchoStarter(n)).starter(EchoStarter(h))
                }
            })
            .card(AgentCardConfig::new(
                &name,
                "A test agent.",
                format!("http://{addr}/").parse().unwrap(),
                "0.0.1",
            ))
            .options(RuntimeOptions {
                poll_interval: Duration::from_millis(20),
                ..RuntimeOptions::default()
            });
            if served {
                agents = agents.child_sessions(sessions_for(&helper));
            }
            let running = Running::start(listener, cfg, agents);
            until_ok(addr, "/healthz", &running.handle).await;

            let public = AgentCardResolver::new(None)
                .resolve(&format!("http://{addr}"))
                .await
                .unwrap();
            let entry = public
                .capabilities
                .extensions
                .iter()
                .flatten()
                .find(|e| e.uri == CHILD_SESSIONS_EXTENSION);
            assert_eq!(entry.is_some(), served, "{role}, served {served}");
            if let Some(entry) = entry {
                let params = entry.params.as_ref().unwrap();
                assert_eq!(params["maxOpen"], 8, "{role}");
                assert_eq!(
                    params["agents"],
                    json!([{"name": "helper", "description": "Helps."}]),
                    "{role}"
                );
            }
            running.finish().await;
        }
    }
}

/// The card lists `model-pick/v1` with the choices the backend serves, and lists nothing when the
/// backend serves none (a deployment that offers one model has nothing to pick).
#[tokio::test]
async fn the_card_lists_model_pick_exactly_when_the_backend_serves_it() {
    use a2a_client::agent_card::AgentCardResolver;
    use adam_a2a::{MODEL_PICK_EXTENSION, ModelChoices};

    let Some(url) = database() else { return };

    for served in [true, false] {
        let name = unique("serve-model-pick");
        let (listener, port) = listen().await;
        let addr: SocketAddr = ([127, 0, 0, 1], port).into();
        let cfg = config(&url, "all", port);
        let mut agents = agents(&name, &cfg).card(AgentCardConfig::new(
            &name,
            "A test agent.",
            format!("http://{addr}/").parse().unwrap(),
            "0.0.1",
        ));
        if served {
            // One choice names its endpoint label, the other does not (ANR-623): the card carries
            // each as it is, and invents nothing.
            let profiles = adam_model::ModelProfiles::parse_json(
                r#"{"profiles": {"alias-a": {"endpoint": "gateway-a"}, "alias-b": {}}}"#,
            )
            .expect("valid profiles");
            agents = agents.model_choices(ModelChoices::from_profiles(
                ["alias-a", "alias-b"],
                &profiles,
            ));
        }
        let running = Running::start(listener, cfg, agents);
        until_ok(addr, "/healthz", &running.handle).await;

        let public = AgentCardResolver::new(None)
            .resolve(&format!("http://{addr}"))
            .await
            .unwrap();
        let entry = public
            .capabilities
            .extensions
            .iter()
            .flatten()
            .find(|e| e.uri == MODEL_PICK_EXTENSION);
        assert_eq!(entry.is_some(), served);
        if let Some(entry) = entry {
            let params = entry.params.as_ref().unwrap();
            assert_eq!(
                params["choices"],
                json!([{"id": "alias-a", "endpoint": "gateway-a"}, {"id": "alias-b"}])
            );
        }
        running.finish().await;
    }
}

/// A helper the process did not register is a fault of the binary, found at start: exit 78 and
/// nothing served.
#[tokio::test]
async fn a_helper_that_is_not_registered_stops_serve_with_78() {
    let Some(url) = database() else { return };
    let name = unique("serve-sessions-missing");
    let cfg = config(&url, "all", 0);
    let helper = format!("{name}/helper");
    let agents = agents(&name, &cfg).child_sessions(sessions_for(&helper));
    let error = adam_service::serve(&cfg, agents, std::future::pending())
        .await
        .unwrap_err();
    assert!(
        matches!(&error, ServeError::ChildSessions { agent } if *agent == helper),
        "{error}"
    );
    assert_eq!(exit_code(&error), 78);
    assert!(!error.to_string().contains("postgres://"));
}

/// `DEFAULT_LOCALE` reaches the card through `serve`: the card lists `locale/v1` with the tag
/// when the deployment sets one, and lists nothing when it does not.
#[tokio::test]
async fn the_card_lists_locale_exactly_when_the_deployment_names_a_default_language() {
    use a2a_client::agent_card::AgentCardResolver;
    use adam_a2a::LOCALE_EXTENSION;

    let Some(url) = database() else { return };

    for tag in [Some("fr-FR"), None] {
        let name = unique("serve-locale");
        let (listener, port) = listen().await;
        let addr: SocketAddr = ([127, 0, 0, 1], port).into();
        let more: Vec<(&str, String)> = tag
            .map(|t| ("DEFAULT_LOCALE", t.to_owned()))
            .into_iter()
            .collect();
        let cfg = config_with(&url, "all", port, &more);
        let card = AgentCardConfig::new(
            &name,
            "A test agent.",
            format!("http://{addr}/").parse().unwrap(),
            "0.0.1",
        );
        let agents = agents(&name, &cfg);
        let running = Running::start(listener, cfg, agents.card(card));
        until_ok(addr, "/healthz", &running.handle).await;

        let public = AgentCardResolver::new(None)
            .resolve(&format!("http://{addr}"))
            .await
            .unwrap();
        let entry = public
            .capabilities
            .extensions
            .as_ref()
            .and_then(|all| all.iter().find(|e| e.uri == LOCALE_EXTENSION));
        match tag {
            Some(t) => {
                let params = entry.and_then(|e| e.params.as_ref()).expect("params");
                assert_eq!(params["locale"], t);
            }
            None => assert!(entry.is_none(), "no tag, no entry"),
        }
        running.finish().await;
    }
}

/// The card lists `uploads/v1` with the largest upload: the folder's value wins over
/// `UPLOADS_MAX_BYTES`, which wins over the default.
#[tokio::test]
async fn the_card_lists_uploads_with_the_folder_then_the_environment_then_the_default() {
    use a2a_client::agent_card::AgentCardResolver;
    use adam_a2a::{DEFAULT_UPLOADS_MAX_BYTES, UPLOADS_EXTENSION};

    let Some(url) = database() else { return };

    let cases: [(Option<&str>, Option<u64>, u64); 4] = [
        (None, None, DEFAULT_UPLOADS_MAX_BYTES),
        (Some("2048"), None, 2048),
        (None, Some(4096), 4096),
        (Some("2048"), Some(4096), 4096),
    ];
    for (env, folder, expected) in cases {
        let name = unique("serve-uploads");
        let (listener, port) = listen().await;
        let addr: SocketAddr = ([127, 0, 0, 1], port).into();
        let more: Vec<(&str, String)> = env
            .map(|t| ("UPLOADS_MAX_BYTES", t.to_owned()))
            .into_iter()
            .collect();
        let cfg = config_with(&url, "all", port, &more);
        let card = AgentCardConfig::new(
            &name,
            "A test agent.",
            format!("http://{addr}/").parse().unwrap(),
            "0.0.1",
        );
        let agents = agents(&name, &cfg).card(card);
        let agents = match folder {
            Some(bytes) => agents.uploads_max_bytes(bytes),
            None => agents,
        };
        let running = Running::start(listener, cfg, agents);
        until_ok(addr, "/healthz", &running.handle).await;

        let public = AgentCardResolver::new(None)
            .resolve(&format!("http://{addr}"))
            .await
            .unwrap();
        let entry = public
            .capabilities
            .extensions
            .as_ref()
            .and_then(|all| all.iter().find(|e| e.uri == UPLOADS_EXTENSION))
            .expect("listed");
        let params = entry.params.as_ref().expect("params");
        assert_eq!(params["maxBytes"], expected, "{env:?} {folder:?}");
        running.finish().await;
    }
}
