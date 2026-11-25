//! `prompt-statement/v1` from the A2A message to the wire (ADR 0077): a real `LlmAgent` on a real
//! `Runtime`, its model calls made by `adam-model-openai` with `InjectedRun` (what
//! `MODEL_AUTH=injector` gives) to a WireMock server, which keeps the head of every call.
//!
//! * A message that activated the extension: every model call of its run carries
//!   `Prompt-Statement` and `Prompt-User-Signature`, the root's, a local subagent's and a session
//!   round's, beside `X-Run-Id`.
//! * A steer that brings new values replaces them for every later call (the session opened after
//!   it included); one that brings none keeps them; a malformed entry is refused and writes nothing.
//! * A message whose request did not activate it, or that forges the payload key, sends neither.
//! * Neither value is in any log line of the run, at any level, nor in an error's text.
//! * With `SignedRequests` (ADR 0068, amended 2026-11-25) every call of the run, the root's, a local
//!   subagent's and a session round's, carries a `Turn-Signature` that verifies over the request
//!   the mock received and binds the statement's `jti` (or a device envelope's `nonce`); no
//!   signature is in any log line.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use a2a::{Message, Part, Role, Task, TaskState};
use adam_a2a::{
    BackendError, Caller, PROMPT_STATEMENT_EXTENSION, STEER_EXTENSION, TaskBackend as _,
};
use adam_a2a_runtime::{RuntimeTaskBackend, task_id_for};
use adam_core::turn::{DynTurnKeys, KeySet, ModelCall, TurnKeys, verify_request};
use adam_core::{DynStore, MemoryStore, RunId};
use adam_llm_agent::{
    Conversation, LlmAgent, SessionAgent, SessionTools, Tool, ToolCtx, ToolError, ToolOutput,
};
use adam_model::{DynModel, InjectedRun, SignedRequests, ToolSpec};
use adam_model_openai::{OpenAiCompatible, OpenAiConfig};
use adam_runtime::{BroadcastSink, Runtime, child_run_id};
use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use secrecy::SecretString;
use serde_json::{Value, json};
use tokio::sync::{Notify, oneshot};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// The values of the first message: a statement and the person's envelope.
const STATEMENT_A: &str = "eyJhbGciOiJFZERTQSJ9.eyJwcm9tcHQiOiJhIn0.c3RhdGVtZW50LWE";
const USER_A: &str = "eyJhbGciOiJFUzI1NiJ9.eyJ1c2VyIjoiYSJ9.ZW52ZWxvcGUtYQ";
/// The values a steer brings: a statement and no envelope.
const STATEMENT_B: &str = "eyJhbGciOiJFZERTQSJ9.eyJwcm9tcHQiOiJiIn0.c3RhdGVtZW50LWI";

// ---------------------------------------------------------------------------
// Logs: one global subscriber for this binary, so no callsite caches "nobody listens".
// ---------------------------------------------------------------------------

fn logs() -> &'static Arc<Mutex<Vec<u8>>> {
    static LOGS: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    LOGS.get_or_init(|| {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let writer = Logs(buffer.clone());
        tracing::subscriber::set_global_default(
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::TRACE)
                .with_ansi(false)
                .with_writer(writer)
                .finish(),
        )
        .expect("one subscriber for the binary");
        buffer
    })
}

#[derive(Clone)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logs {
    type Writer = Logs;
    fn make_writer(&'a self) -> Logs {
        self.clone()
    }
}

fn assert_no_value_logged() {
    let text = String::from_utf8_lossy(&logs().lock().unwrap()).into_owned();
    assert!(!text.is_empty(), "the capture saw the run");
    for value in [STATEMENT_A, USER_A, STATEMENT_B] {
        // The signature segment alone is enough to recognise a value.
        let tail = value.rsplit('.').next().unwrap();
        assert!(!text.contains(tail), "a value is in the logs");
    }
}

// ---------------------------------------------------------------------------
// The model: one scripted answer queue per agent, each agent at its own path.
// ---------------------------------------------------------------------------

struct Script(Mutex<VecDeque<Value>>);

impl Respond for Script {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        match self.0.lock().unwrap().pop_front() {
            Some(message) => ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"index": 0, "message": message, "finish_reason":
                    if message.get("tool_calls").is_some() { "tool_calls" } else { "stop" }}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}
            })),
            None => ResponseTemplate::new(500).set_body_string("the script is over"),
        }
    }
}

fn text(words: &str) -> Value {
    json!({"role": "assistant", "content": words})
}

fn calls(id: &str, name: &str, args: Value) -> Value {
    json!({"role": "assistant", "content": null, "tool_calls": [{
        "id": id, "type": "function",
        "function": {"name": name, "arguments": args.to_string()}
    }]})
}

async fn script(server: &MockServer, prefix: &str, answers: Vec<Value>) {
    Mock::given(method("POST"))
        .and(path(format!("/{prefix}/v1/chat/completions")))
        .respond_with(Script(Mutex::new(answers.into())))
        .mount(server)
        .await;
}

fn model(server: &MockServer, prefix: &str, keys: Option<&DynTurnKeys>) -> DynModel {
    let mut config = OpenAiConfig::new(
        format!("{}/{prefix}/v1", server.uri()),
        SecretString::from(""),
    );
    config.timeout = Duration::from_secs(20);
    let credentials: adam_model::DynCredentials = match keys {
        None => Arc::new(InjectedRun::new()),
        Some(keys) => Arc::new(SignedRequests::new(
            Arc::new(InjectedRun::new()),
            keys.clone(),
        )),
    };
    Arc::new(
        OpenAiCompatible::new(config)
            .expect("client")
            .with_credentials(credentials),
    )
}

/// `(Prompt-Statement, Prompt-User-Signature, X-Run-Id)` of each call to `prefix`, in order.
async fn heads(
    server: &MockServer,
    prefix: &str,
) -> Vec<(Option<String>, Option<String>, Option<String>)> {
    let header =
        |r: &Request, name: &str| r.headers.get(name).map(|v| v.to_str().unwrap().to_owned());
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().starts_with(&format!("/{prefix}/")))
        .map(|r| {
            (
                header(r, "prompt-statement"),
                header(r, "prompt-user-signature"),
                header(r, "x-run-id"),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The agents
// ---------------------------------------------------------------------------

fn spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: format!("the {name} tool"),
        parameters: json!({"type": "object", "properties": {}}),
    }
}

/// Starts the local subagent and waits for it.
struct Spawn {
    target: String,
}

#[async_trait]
impl Tool for Spawn {
    fn spec(&self) -> ToolSpec {
        spec("spawn")
    }
    async fn call(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        let run = ctx.start_child(&self.target, "do the child's part").await?;
        Err(ToolError::AwaitRun { run })
    }
}

#[derive(Default)]
struct Gate {
    reached: Notify,
    release: Notify,
}

/// Waits at its gate while the test steers the task.
struct GateTool(Arc<Gate>);

#[async_trait]
impl Tool for GateTool {
    fn spec(&self) -> ToolSpec {
        spec("gate")
    }
    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        self.0.reached.notify_one();
        self.0.release.notified().await;
        Ok(ToolOutput::text("released"))
    }
}

struct Rig {
    server: MockServer,
    root: String,
    runtime: Runtime,
    backend: RuntimeTaskBackend,
    gate: Arc<Gate>,
}

impl Rig {
    async fn new() -> Self {
        Self::with_keys(None).await
    }

    async fn with_keys(keys: Option<DynTurnKeys>) -> Self {
        let keys = keys.as_ref();
        let _ = logs();
        let server = MockServer::start().await;
        let root = format!("root-{}", RunId::new());
        let child = format!("child-{}", RunId::new());
        let helper = format!("helper-{}", RunId::new());
        let gate = Arc::new(Gate::default());
        let sessions = SessionTools::new([SessionAgent {
            name: "helper".into(),
            agent: helper.clone(),
            description: "Does a piece of work.".into(),
            within: None,
            remote: false,
            bounds: adam_runtime::SessionBounds::default(),
        }]);
        let mut builder = LlmAgent::builder(&root, model(&server, "root", keys), "m")
            .stream_text(false)
            .session_agents([helper.clone()])
            .tool(Spawn {
                target: child.clone(),
            })
            .tool(GateTool(gate.clone()));
        for tool in sessions.tools() {
            builder = builder.dyn_tool(tool);
        }
        let agent = |name: &str, prefix: &str| {
            LlmAgent::builder(name, model(&server, prefix, keys), "m")
                .stream_text(false)
                .build()
        };
        let store: DynStore = Arc::new(MemoryStore::new());
        let events = BroadcastSink::default();
        let runtime = Runtime::builder(store)
            .agent(builder.build())
            .agent(agent(&child, "child"))
            .agent(agent(&helper, "helper"))
            .event_sink(events.clone())
            .poll_interval(Duration::from_millis(10))
            .build();
        let backend = RuntimeTaskBackend::new(runtime.clone(), events, root.clone())
            .with_poll_interval(Duration::from_millis(10));
        Self {
            server,
            root,
            runtime,
            backend,
            gate,
        }
    }

    fn worker(&self) -> (oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
        let (stop, rx) = oneshot::channel::<()>();
        let rt = self.runtime.clone();
        let handle = tokio::spawn(async move {
            let _ = rt
                .run_worker(async {
                    let _ = rx.await;
                })
                .await;
        });
        (stop, handle)
    }

    async fn wait(&self, who: &Caller, id: &str, state: TaskState) -> Task {
        for _ in 0..3000 {
            let task = self.backend.get(who, id).await.unwrap().unwrap();
            if task.status.state == state {
                return task;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("task {id} never reached {state:?}");
    }

    async fn state(&self, run: RunId) -> Conversation {
        serde_json::from_value(self.runtime.state(run).await.unwrap().unwrap()).unwrap()
    }
}

fn signing(token: &str) -> Caller {
    Caller::new(token).with_extensions([PROMPT_STATEMENT_EXTENSION, STEER_EXTENSION])
}

fn message(words: &str, id: &str, entry: Option<Value>) -> Message {
    let mut m = Message::new(Role::User, vec![Part::text(words)]);
    m.message_id = id.into();
    if let Some(entry) = entry {
        m.metadata = Some(HashMap::from([(
            PROMPT_STATEMENT_EXTENSION.to_owned(),
            entry,
        )]));
    }
    m
}

fn a() -> Option<Value> {
    Some(json!({"statement": STATEMENT_A, "user": USER_A}))
}

fn b() -> Option<Value> {
    Some(json!({"statement": STATEMENT_B}))
}

// ---------------------------------------------------------------------------
// Cases
// ---------------------------------------------------------------------------

/// The root's calls, the local subagent's and the session round's all carry the values; the steer
/// replaces them for the calls after it, the session opened after it included.
#[tokio::test]
async fn every_model_call_of_the_run_carries_the_values_and_a_steer_replaces_them() {
    let rig = Rig::new().await;
    let token = format!("token-{}", RunId::new());
    let root_run = task_id_for(&rig.root, &token, None, "m-1");
    let session = child_run_id(root_run, "c1");
    script(
        &rig.server,
        "root",
        vec![
            calls("p1", "spawn", json!({})),
            calls("g1", "gate", json!({})),
            calls(
                "c1",
                "subagent_start",
                json!({"agent": "helper", "description": "look", "prompt": "do the work",
                       "keep_open": true}),
            ),
            calls(
                "r1",
                "subagent_read",
                json!({"session": session.to_string(), "wait": true}),
            ),
            text("root done"),
        ],
    )
    .await;
    script(&rig.server, "child", vec![text("child done")]).await;
    script(&rig.server, "helper", vec![text("helper done")]).await;

    let who = signing(&token);
    let task = rig
        .backend
        .submit(who.clone(), message("go", "m-1", a()), None, None)
        .await
        .unwrap();
    assert_eq!(task.id, root_run.to_string());
    let (stop, worker) = rig.worker();

    // The root waits at the gate: steer it with new values.
    tokio::time::timeout(Duration::from_secs(20), rig.gate.reached.notified())
        .await
        .expect("the gate is reached");
    rig.backend
        .submit(
            who.clone(),
            message("and sign this", "m-2", b()),
            Some(task.id.clone()),
            None,
        )
        .await
        .expect("the steer is accepted");
    rig.gate.release.notify_one();
    rig.wait(&who, &task.id, TaskState::Completed).await;
    stop.send(()).ok();
    worker.await.ok();

    let run = Some(root_run.to_string());
    let with_a = (
        Some(STATEMENT_A.to_owned()),
        Some(USER_A.to_owned()),
        run.clone(),
    );
    let with_b = (Some(STATEMENT_B.to_owned()), None, run.clone());
    assert_eq!(
        heads(&rig.server, "root").await,
        vec![
            with_a.clone(),
            with_a.clone(),
            with_b.clone(),
            with_b.clone(),
            with_b.clone()
        ],
        "the root: two calls before the steer, three after"
    );
    assert_eq!(
        heads(&rig.server, "child").await,
        vec![with_a],
        "the local subagent, started before the steer"
    );
    assert_eq!(
        heads(&rig.server, "helper").await,
        vec![with_b],
        "the session's round, opened after the steer"
    );
    // The run keeps the values in its state, never in its context.
    let state = rig.state(root_run).await;
    let kept = state.prompt_statement.expect("kept");
    assert_eq!((kept.statement(), kept.user()), (Some(STATEMENT_B), None));
    let context = serde_json::to_string(&state.context).unwrap();
    assert!(!context.contains(STATEMENT_B), "{context}");
    // No body carries them.
    for r in rig.server.received_requests().await.unwrap() {
        let body = String::from_utf8_lossy(&r.body);
        assert!(!body.contains(STATEMENT_A) && !body.contains(STATEMENT_B));
    }
    assert_no_value_logged();
}

/// A steer that brings no values keeps the run's; a malformed entry is refused with a sentence
/// that quotes no value, and writes nothing.
#[tokio::test]
async fn a_steer_without_values_keeps_them_and_a_malformed_one_is_refused() {
    let rig = Rig::new().await;
    let token = format!("token-{}", RunId::new());
    script(
        &rig.server,
        "root",
        vec![calls("g1", "gate", json!({})), text("root done")],
    )
    .await;
    let who = signing(&token);
    let task = rig
        .backend
        .submit(who.clone(), message("go", "m-1", a()), None, None)
        .await
        .unwrap();
    let (stop, worker) = rig.worker();
    tokio::time::timeout(Duration::from_secs(20), rig.gate.reached.notified())
        .await
        .expect("the gate is reached");

    let forged = format!("{STATEMENT_B}.extra");
    for entry in [
        json!({"statement": forged}),
        json!({"statement": STATEMENT_B, "user": "not a jws"}),
        json!({"statement": null}),
    ] {
        let err = rig
            .backend
            .submit(
                who.clone(),
                message("bad", "m-bad", Some(entry.clone())),
                Some(task.id.clone()),
                None,
            )
            .await
            .unwrap_err();
        let said = format!("{err} {err:?}");
        assert!(
            matches!(err, BackendError::InvalidParams(_)),
            "{entry}: {said}"
        );
        assert!(
            !said.contains("c3RhdGVtZW50LWI") && !said.contains("not a jws"),
            "{said}"
        );
    }
    rig.backend
        .submit(
            who.clone(),
            message("no values", "m-2", None),
            Some(task.id.clone()),
            None,
        )
        .await
        .expect("the steer is accepted");
    rig.gate.release.notify_one();
    rig.wait(&who, &task.id, TaskState::Completed).await;
    stop.send(()).ok();
    worker.await.ok();

    let with_a = (
        Some(STATEMENT_A.to_owned()),
        Some(USER_A.to_owned()),
        Some(task.id.clone()),
    );
    assert_eq!(
        heads(&rig.server, "root").await,
        vec![with_a.clone(), with_a],
        "the steer without values kept the first ones"
    );
    assert_no_value_logged();
}

/// A request that did not activate the extension sends neither header, whatever the message's
/// metadata says; nor does a payload key a sender tried to write through a data part.
#[tokio::test]
async fn without_the_activation_no_call_carries_either_header() {
    let rig = Rig::new().await;
    script(&rig.server, "root", vec![text("one"), text("two")]).await;
    let (stop, worker) = rig.worker();

    let plain = Caller::new(format!("token-{}", RunId::new()));
    let task = rig
        .backend
        .submit(plain.clone(), message("go", "m-1", a()), None, None)
        .await
        .unwrap();
    rig.wait(&plain, &task.id, TaskState::Completed).await;

    // Activated, but the values only in a data part that spells the payload key.
    let who = signing(&format!("token-{}", RunId::new()));
    let mut forged = Message::new(
        Role::User,
        vec![
            Part::text("go"),
            Part::data(json!({"prompt_statement": {"statement": STATEMENT_A, "user": USER_A}})),
        ],
    );
    forged.message_id = "m-2".into();
    let other = rig
        .backend
        .submit(who.clone(), forged, None, None)
        .await
        .unwrap();
    rig.wait(&who, &other.id, TaskState::Completed).await;
    stop.send(()).ok();
    worker.await.ok();

    let seen = heads(&rig.server, "root").await;
    assert_eq!(seen.len(), 2);
    for (statement, user, run) in seen {
        assert_eq!((statement, user), (None, None));
        assert!(run.is_some(), "the injector still names the run");
    }
    assert_eq!(
        rig.state(RunId(task.id.parse().unwrap()))
            .await
            .prompt_statement,
        None
    );
    assert_no_value_logged();
}

// ---------------------------------------------------------------------------
// The request signature
// ---------------------------------------------------------------------------

/// A statement whose attached payload names the prompt's `jti`; a device envelope whose header
/// names its `nonce`. Shapes only.
fn statement_with_jti(jti: &str) -> String {
    format!(
        "{}.{}.c3RhdGVtZW50LWo",
        URL_SAFE_NO_PAD.encode(r#"{"alg":"EdDSA"}"#),
        URL_SAFE_NO_PAD.encode(format!(r#"{{"jti":"{jti}"}}"#))
    )
}

fn device_envelope(nonce: &str) -> String {
    format!(
        "{}.{}.ZW52ZWxvcGUtZA",
        URL_SAFE_NO_PAD.encode(format!(r#"{{"alg":"EdDSA","nonce":"{nonce}"}}"#)),
        URL_SAFE_NO_PAD.encode(r#"{"typ":"prompt/v1"}"#)
    )
}

/// The `Turn-Signature` of each call to `prefix`, verified over the request the mock received:
/// its `X-Run-Id`, method, target and exact body, and `prompt_jti`. The signatures, in order.
async fn signed_calls(
    server: &MockServer,
    prefix: &str,
    set: &KeySet,
    run: &str,
    prompt_jti: &str,
) -> Vec<String> {
    let now = jiff::Timestamp::now().as_second();
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().starts_with(&format!("/{prefix}/")))
        .map(|r| {
            let text = |n: &str| r.headers.get(n).map(|v| v.to_str().unwrap().to_owned());
            assert_eq!(text("x-run-id").as_deref(), Some(run), "{prefix}");
            let jws = text("turn-signature").expect("every call is signed");
            verify_request(
                set,
                &jws,
                &ModelCall {
                    run,
                    method: r.method.as_str(),
                    path: r.url.path(),
                    body: &r.body,
                    prompt_jti: Some(prompt_jti),
                },
                now,
            )
            .unwrap_or_else(|e| panic!("{prefix}: {e}"));
            jws
        })
        .collect()
}

fn assert_no_signature_logged(signatures: &[String]) {
    let text = String::from_utf8_lossy(&logs().lock().unwrap()).into_owned();
    for jws in signatures {
        let tail = jws.rsplit('.').next().unwrap();
        assert!(!text.contains(tail), "a turn signature is in the logs");
    }
    assert!(!text.contains("PRIVATE KEY"));
}

/// The root's calls, the local subagent's and the session round's each carry a signature of that
/// request, bound to the run and to the statement's `jti`.
#[tokio::test]
async fn every_model_call_of_the_run_is_signed() {
    let keys = <dyn TurnKeys>::ephemeral().unwrap();
    let set = keys.key_set().await.unwrap();
    let rig = Rig::with_keys(Some(keys)).await;
    let token = format!("token-{}", RunId::new());
    let root_run = task_id_for(&rig.root, &token, None, "m-1");
    let session = child_run_id(root_run, "c1");
    script(
        &rig.server,
        "root",
        vec![
            calls("p1", "spawn", json!({})),
            calls(
                "c1",
                "subagent_start",
                json!({"agent": "helper", "description": "look", "prompt": "do the work",
                       "keep_open": true}),
            ),
            calls(
                "r1",
                "subagent_read",
                json!({"session": session.to_string(), "wait": true}),
            ),
            text("root done"),
        ],
    )
    .await;
    script(&rig.server, "child", vec![text("child done")]).await;
    script(&rig.server, "helper", vec![text("helper done")]).await;

    let who = signing(&token);
    let entry = json!({"statement": statement_with_jti("jti-root-1"), "user": USER_A});
    let task = rig
        .backend
        .submit(who.clone(), message("go", "m-1", Some(entry)), None, None)
        .await
        .unwrap();
    let (stop, worker) = rig.worker();
    rig.wait(&who, &task.id, TaskState::Completed).await;
    stop.send(()).ok();
    worker.await.ok();

    let run = root_run.to_string();
    let root = signed_calls(&rig.server, "root", &set, &run, "jti-root-1").await;
    let child = signed_calls(&rig.server, "child", &set, &run, "jti-root-1").await;
    let helper = signed_calls(&rig.server, "helper", &set, &run, "jti-root-1").await;
    assert_eq!((root.len(), child.len(), helper.len()), (4, 1, 1));
    let mut all: Vec<String> = root.into_iter().chain(child).chain(helper).collect();
    assert_no_signature_logged(&all);
    all.sort();
    all.dedup();
    assert_eq!(all.len(), 6, "each call has its own signature");
}

/// A device's envelope alone: every call carries it, no statement, and the signature binds the
/// envelope's `nonce`.
#[tokio::test]
async fn a_device_envelope_alone_is_sent_and_its_nonce_is_signed() {
    let keys = <dyn TurnKeys>::ephemeral().unwrap();
    let set = keys.key_set().await.unwrap();
    let rig = Rig::with_keys(Some(keys)).await;
    script(&rig.server, "root", vec![text("done")]).await;
    let token = format!("token-{}", RunId::new());
    let who = signing(&token);
    let envelope = device_envelope("bm9uY2UtZGV2aWNl");
    let task = rig
        .backend
        .submit(
            who.clone(),
            message("go", "m-1", Some(json!({"user": envelope}))),
            None,
            None,
        )
        .await
        .unwrap();
    let (stop, worker) = rig.worker();
    rig.wait(&who, &task.id, TaskState::Completed).await;
    stop.send(()).ok();
    worker.await.ok();

    assert_eq!(
        heads(&rig.server, "root").await,
        vec![(None, Some(envelope), Some(task.id.clone()))]
    );
    let signatures = signed_calls(&rig.server, "root", &set, &task.id, "bm9uY2UtZGV2aWNl").await;
    assert_eq!(signatures.len(), 1);
    assert_no_signature_logged(&signatures);
}
