//! `child-sessions/v1` on the A2A backend, by a real `LlmAgent` helper on a real `Runtime`: a
//! person starts a session, follows its rounds as tasks, talks to it, stops a round and closes it;
//! the parent task says its sessions; and every request that is not the caller's, not activated or
//! malformed is refused before anything is written.
//!
//! Run against `MemoryStore` always and against PostgreSQL when `ADAM_TEST_POSTGRES_URL` is set.
//! Nothing sleeps for a fixed time: the helper's model call is held at a gate, and the few polls
//! wait for a state with a deadline.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::time::{Duration, Instant};

use a2a::{Message, Part, Role, Task, TaskState};
use adam_a2a::{
    ACCESS_TIER_EXTENSION, AccessTier, AccessTiers, BackendError, CHILD_SESSIONS_EXTENSION, Caller,
    STEER_EXTENSION, TaskBackend, TaskQuery,
};
use adam_a2a_runtime::{ChildSessionAgent, ChildSessions, RuntimeTaskBackend};
use adam_core::{DynStore, MemoryStore, RunId};
use adam_llm_agent::{LlmAgent, SessionAgent, SessionTools};
use adam_model::{
    DynModel, MockModel, ModelClient, ModelDelta, ModelError, ModelRequest, ModelResponse, ToolCall,
};
use adam_runtime::{BroadcastSink, Runtime, parse_session_conversation_id};
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::sync::{Notify, oneshot};

async fn stores() -> Vec<DynStore> {
    let mut all: Vec<DynStore> = vec![Arc::new(MemoryStore::new())];
    if let Some(url) = adam_core::testing::test_env("ADAM_TEST_POSTGRES_URL") {
        let store = adam_store_postgres::PgStore::connect(&url)
            .await
            .expect("connect to postgres");
        adam_core::Store::migrate(&store).await.expect("migrate");
        all.push(Arc::new(store));
    }
    all
}

fn uniq(prefix: &str) -> String {
    format!("{prefix}-{}", RunId::new())
}

#[derive(Default)]
struct Gate {
    reached: Notify,
    release: Notify,
}

/// Wraps a model: the first call waits at the gate when `armed`.
struct GateModel {
    inner: Arc<MockModel>,
    calls: AtomicUsize,
    hold_on: Option<usize>,
    gate: Arc<Gate>,
}

impl GateModel {
    async fn hold(&self) {
        let n = self.calls.fetch_add(1, SeqCst);
        if self.hold_on == Some(n) {
            self.gate.reached.notify_one();
            self.gate.release.notified().await;
        }
    }
}

#[async_trait]
impl ModelClient for GateModel {
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse, ModelError> {
        self.hold().await;
        self.inner.complete(req).await
    }

    async fn stream(
        &self,
        req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError> {
        self.hold().await;
        self.inner.stream(req).await
    }
}

struct Rig {
    store: DynStore,
    runtime: Runtime,
    backend: RuntimeTaskBackend,
    parent_mock: Arc<MockModel>,
    helper_mock: Arc<MockModel>,
    hidden_mock: Arc<MockModel>,
    hidden: String,
    gate: Arc<Gate>,
    /// Holds the parent's model, when `hold_parent_on` says.
    parent_gate: Arc<Gate>,
    helper: String,
    worker: Option<(oneshot::Sender<()>, tokio::task::JoinHandle<()>)>,
}

struct Options {
    hold_helper_on: Option<usize>,
    hold_parent_on: Option<usize>,
    max_open: u32,
    tiers: bool,
    listing_page: usize,
    max_rounds: u32,
}

impl Options {
    fn new() -> Self {
        Self {
            hold_helper_on: None,
            hold_parent_on: None,
            max_open: 16,
            tiers: false,
            listing_page: 100,
            max_rounds: 20,
        }
    }
}

impl Rig {
    fn new(store: &DynStore) -> Self {
        Self::with(store, Options::new())
    }

    fn with(store: &DynStore, options: Options) -> Self {
        let parent = uniq("parent");
        let helper = uniq("helper");
        let parent_mock = Arc::new(MockModel::new());
        let helper_mock = Arc::new(MockModel::new());
        let hidden = uniq("hidden");
        let hidden_mock = Arc::new(MockModel::new());
        let gate = Arc::new(Gate::default());
        let parent_gate = Arc::new(Gate::default());
        let parent_model: DynModel = Arc::new(GateModel {
            inner: parent_mock.clone(),
            calls: AtomicUsize::new(0),
            hold_on: options.hold_parent_on,
            gate: parent_gate.clone(),
        });
        let helper_model: DynModel = Arc::new(GateModel {
            inner: helper_mock.clone(),
            calls: AtomicUsize::new(0),
            hold_on: options.hold_helper_on,
            gate: gate.clone(),
        });
        let tools = SessionTools::new([
            SessionAgent {
                name: "helper".into(),
                agent: helper.clone(),
                description: "Does a piece of work.".into(),
                within: None,
                remote: false,
            },
            // A helper the model may start but the card does not list.
            SessionAgent {
                name: "hidden".into(),
                agent: hidden.clone(),
                description: "Not on the card.".into(),
                within: None,
                remote: false,
            },
        ]);
        let mut parent_agent = LlmAgent::builder(&parent, parent_model, "m")
            .session_agents([helper.clone(), hidden.clone()]);
        for tool in tools.tools() {
            parent_agent = parent_agent.dyn_tool(tool);
        }
        let events = BroadcastSink::default();
        let runtime = Runtime::builder(store.clone())
            .agent(parent_agent.build())
            .agent(LlmAgent::builder(&helper, helper_model, "m").build())
            .agent(LlmAgent::builder(&hidden, hidden_mock.clone() as DynModel, "m").build())
            .event_sink(events.clone())
            .poll_interval(Duration::from_millis(10))
            .build();
        let sessions = ChildSessions::new(
            [ChildSessionAgent::new("planner", helper.clone(), "Plans.")],
            options.max_open,
        )
        .unwrap()
        .with_listing_page(options.listing_page)
        .with_max_rounds(options.max_rounds);
        let mut backend = RuntimeTaskBackend::new(runtime.clone(), events, parent)
            .with_poll_interval(Duration::from_millis(10))
            .with_child_sessions(sessions);
        if options.tiers {
            backend = backend.with_access_tiers(
                AccessTiers::new([AccessTier::Plan, AccessTier::Auto], AccessTier::Auto).unwrap(),
            );
        }
        let (stop, rx) = oneshot::channel::<()>();
        let rt = runtime.clone();
        let handle = tokio::spawn(async move {
            let _ = rt
                .run_worker(async {
                    let _ = rx.await;
                })
                .await;
        });
        Self {
            store: store.clone(),
            runtime,
            backend,
            parent_mock,
            helper_mock,
            hidden_mock,
            hidden,
            gate,
            parent_gate,
            helper,
            worker: Some((stop, handle)),
        }
    }

    async fn stop(mut self) {
        if let Some((stop, handle)) = self.worker.take() {
            let _ = stop.send(());
            let _ = tokio::time::timeout(Duration::from_secs(10), handle).await;
        }
    }

    async fn submit(
        &self,
        caller: &Caller,
        message: Message,
        task_id: Option<&str>,
    ) -> Result<Task, BackendError> {
        let context = message.context_id.clone();
        self.backend
            .submit(caller.clone(), message, task_id.map(str::to_owned), context)
            .await
    }

    /// Start a session as `caller` in `context` (a new one when `None`), keep-open as asked.
    async fn start(
        &self,
        caller: &Caller,
        context: Option<&str>,
        text: &str,
        keep_open: bool,
    ) -> Task {
        let mut message = said(text, context);
        message.metadata = Some(HashMap::from([(
            CHILD_SESSIONS_EXTENSION.to_owned(),
            json!({"start": {"agent": "planner", "description": "Plan it", "keepOpen": keep_open}}),
        )]));
        self.submit(caller, message, None).await.expect("start")
    }

    async fn task(&self, caller: &Caller, id: &str) -> Task {
        self.backend
            .get(caller, id)
            .await
            .expect("get")
            .expect("task")
    }

    async fn until(&self, caller: &Caller, id: &str, state: TaskState) -> Task {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let task = self.task(caller, id).await;
            if task.status.state == state {
                return task;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {state:?}; last {:?}",
                task.status.state
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

fn said(text: &str, context: Option<&str>) -> Message {
    let mut message = Message::new(Role::User, vec![Part::text(text)]);
    message.context_id = context.map(str::to_owned);
    message
}

fn alice() -> Caller {
    activated("alice")
}

fn activated(subject: &str) -> Caller {
    let mut caller = Caller::new(subject);
    caller.extensions = vec![CHILD_SESSIONS_EXTENSION.to_owned()];
    caller
}

fn plain(subject: &str) -> Caller {
    Caller::new(subject)
}

fn object(task: &Task) -> Value {
    task.metadata
        .as_ref()
        .and_then(|m| m.get(CHILD_SESSIONS_EXTENSION))
        .cloned()
        .unwrap_or(Value::Null)
}

fn invalid(result: Result<Task, BackendError>) -> bool {
    matches!(result, Err(BackendError::InvalidParams(_)))
}

fn unsupported(result: Result<Task, BackendError>) -> bool {
    matches!(result, Err(BackendError::UnsupportedOperation(_)))
}

fn close_message(context: &str, cancel: bool) -> Message {
    let mut message = said("close", Some(context));
    message.metadata = Some(HashMap::from([(
        CHILD_SESSIONS_EXTENSION.to_owned(),
        json!({"close": {"cancel": cancel}}),
    )]));
    message
}

/// A person starts a session: round 1 comes back in a new child context, no parent task runs, the
/// round says it is the person's and is its own parent in the store, and a repeat of the request
/// finds the same round.
#[tokio::test]
async fn a_person_starts_a_session_and_it_is_the_round_of_a_new_child_context() {
    for store in stores().await {
        let rig = Rig::new(&store);
        rig.helper_mock.push_text("a plan");
        let start = rig.start(&alice(), Some("ctx-1"), "plan it", true).await;
        let (parent, session) = parse_session_conversation_id(&start.context_id).unwrap();
        assert_eq!(parent, "ctx-1");
        assert_eq!(start.id, session.to_string());
        let o = object(&start);
        assert_eq!(o["session"], session.to_string());
        assert_eq!(o["parentContextId"], "ctx-1");
        assert_eq!(o["agent"], "planner");
        assert_eq!(o["description"], "Plan it");
        assert_eq!(o["keepOpen"], true);
        assert_eq!(o["round"], 1);
        assert_eq!(o["by"], "person");
        let rec = store.load_run(session).await.unwrap().unwrap();
        assert_eq!(rec.parent_id, Some(session), "the round is its own parent");

        let done = rig.until(&alice(), &start.id, TaskState::Completed).await;
        assert_eq!(
            object(&done)["state"],
            "idle",
            "kept open: idle after a round"
        );

        // No task of the parent context was made.
        let roots = rig.backend.list(&alice(), &TaskQuery::new()).await.unwrap();
        assert!(roots.tasks.is_empty(), "{:?}", roots.tasks);

        // The same request again is the same round.
        let mut again = said("plan it", Some("ctx-1"));
        again.message_id = "m-repeat".into();
        again.metadata = Some(HashMap::from([(
            CHILD_SESSIONS_EXTENSION.to_owned(),
            json!({"start": {"agent": "planner", "description": "Plan it", "keepOpen": true}}),
        )]));
        let first = rig.submit(&alice(), again.clone(), None).await.unwrap();
        let second = rig.submit(&alice(), again, None).await.unwrap();
        assert_eq!(first.id, second.id);
        rig.stop().await;
    }
}

/// A message to an idle session starts round 2, which continues round 1's conversation; one to a
/// session that closes when its round ends is refused after it.
#[tokio::test]
async fn a_message_to_an_idle_session_starts_the_next_round_and_a_closed_one_refuses() {
    for store in stores().await {
        let rig = Rig::new(&store);
        rig.helper_mock
            .push_text("first answer")
            .push_text("second answer");
        let one = rig.start(&alice(), None, "first question", true).await;
        rig.until(&alice(), &one.id, TaskState::Completed).await;

        let two = rig
            .submit(
                &alice(),
                said("second question", Some(&one.context_id)),
                None,
            )
            .await
            .unwrap();
        assert_ne!(two.id, one.id);
        assert_eq!(two.context_id, one.context_id);
        assert_eq!(object(&two)["round"], 2);
        assert_eq!(object(&two)["by"], "person");
        rig.until(&alice(), &two.id, TaskState::Completed).await;
        let seen = format!("{:?}", rig.helper_mock.requests().last().unwrap());
        assert!(
            seen.contains("first question") && seen.contains("first answer"),
            "{seen}"
        );

        // Not kept open: the session closes when its round ends.
        let rig2 = Rig::new(&store);
        rig2.helper_mock.push_text("only answer");
        let once = rig2.start(&alice(), None, "once", false).await;
        let done = rig2.until(&alice(), &once.id, TaskState::Completed).await;
        assert_eq!(object(&done)["state"], "closed");
        assert!(unsupported(
            rig2.submit(&alice(), said("more", Some(&once.context_id)), None)
                .await
        ));
        rig.stop().await;
        rig2.stop().await;
    }
}

/// A message to a working session is delivered into its round (the answer is that round's task); a
/// message naming the round is a steer only with `steer/v1`; one naming an ended round is refused.
#[tokio::test]
async fn a_message_to_a_working_session_goes_into_its_round() {
    for store in stores().await {
        let rig = Rig::with(
            &store,
            Options {
                hold_helper_on: Some(0),
                ..Options::new()
            },
        );
        rig.helper_mock.push_text("one").push_text("two");
        let one = rig.start(&alice(), None, "work", true).await;
        rig.gate.reached.notified().await;

        let into = rig
            .submit(&alice(), said("also this", Some(&one.context_id)), None)
            .await
            .unwrap();
        assert_eq!(into.id, one.id, "delivered into the open round");

        // Naming the round without steer/v1 is refused, as for any task.
        let named = rig
            .submit(
                &alice(),
                said("steer", Some(&one.context_id)),
                Some(&one.id),
            )
            .await;
        assert!(invalid(named));
        let mut steering = alice();
        steering.extensions.push(STEER_EXTENSION.to_owned());
        let steered = rig
            .submit(
                &steering,
                said("steer", Some(&one.context_id)),
                Some(&one.id),
            )
            .await
            .unwrap();
        assert_eq!(steered.id, one.id);

        rig.gate.release.notify_one();
        rig.until(&alice(), &one.id, TaskState::Completed).await;
        // The round is over: naming it is refused as for a terminal task.
        assert!(unsupported(
            rig.submit(&alice(), said("late", Some(&one.context_id)), Some(&one.id))
                .await
        ));
        rig.stop().await;
    }
}

/// `CancelTask` of a working round stops it and a kept-open session goes idle.
#[tokio::test]
async fn cancel_of_a_working_round_makes_a_kept_open_session_idle() {
    for store in stores().await {
        let rig = Rig::with(
            &store,
            Options {
                hold_helper_on: Some(0),
                ..Options::new()
            },
        );
        rig.helper_mock.push_text("never read");
        let one = rig.start(&alice(), None, "work", true).await;
        rig.gate.reached.notified().await;
        let canceled = rig.backend.cancel(&alice(), &one.id).await.unwrap();
        assert_eq!(canceled.status.state, TaskState::Canceled);
        assert_eq!(object(&canceled)["state"], "idle");
        rig.gate.release.notify_one();
        rig.stop().await;
    }
}

/// A close: an idle session closes at once by a round of its own (and a later message is refused,
/// and a repeated close changes nothing); a working one closes when its round ends, saying
/// `closing`; with `cancel` the round is stopped first.
#[tokio::test]
async fn a_close_ends_the_session_when_idle_or_after_its_round() {
    for store in stores().await {
        let rig = Rig::new(&store);
        rig.helper_mock.push_text("answer");
        let one = rig.start(&alice(), None, "work", true).await;
        rig.until(&alice(), &one.id, TaskState::Completed).await;
        let closed = rig
            .submit(&alice(), close_message(&one.context_id, false), None)
            .await
            .unwrap();
        let o = object(&closed);
        assert_eq!(o["state"], "closed");
        assert_eq!(o["by"], "person");
        assert_ne!(closed.id, one.id, "the close is a round of its own");
        assert!(unsupported(
            rig.submit(&alice(), said("more", Some(&one.context_id)), None)
                .await
        ));
        let again = rig
            .submit(&alice(), close_message(&one.context_id, false), None)
            .await
            .unwrap();
        assert_eq!(again.id, closed.id);
        rig.stop().await;

        // A working round: closing, and closed when it ends.
        let rig = Rig::with(
            &store,
            Options {
                hold_helper_on: Some(0),
                ..Options::new()
            },
        );
        rig.helper_mock.push_text("late answer");
        let one = rig.start(&alice(), None, "work", true).await;
        rig.gate.reached.notified().await;
        let asked = rig
            .submit(&alice(), close_message(&one.context_id, false), None)
            .await
            .unwrap();
        assert_eq!(asked.id, one.id);
        assert_eq!(object(&asked)["closing"], true);
        rig.gate.release.notify_one();
        let ended = rig.until(&alice(), &one.id, TaskState::Completed).await;
        assert_eq!(object(&ended)["state"], "closed");
        rig.stop().await;

        // With cancel the round is stopped first.
        let rig = Rig::with(
            &store,
            Options {
                hold_helper_on: Some(0),
                ..Options::new()
            },
        );
        rig.helper_mock.push_text("never");
        let one = rig.start(&alice(), None, "work", true).await;
        rig.gate.reached.notified().await;
        let closed = rig
            .submit(&alice(), close_message(&one.context_id, true), None)
            .await
            .unwrap();
        assert_eq!(object(&closed)["state"], "closed");
        let old = rig.task(&alice(), &one.id).await;
        assert_eq!(old.status.state, TaskState::Canceled);
        rig.gate.release.notify_one();
        rig.stop().await;
    }
}

/// Without activation, a child context and a round are `InvalidParams` before anything is written,
/// a `start` is just a message, and a plain `ListTasks` lists root tasks only.
#[tokio::test]
async fn without_activation_a_child_context_is_refused_and_nothing_is_said() {
    for store in stores().await {
        let rig = Rig::new(&store);
        rig.helper_mock.push_text("answer");
        let one = rig.start(&alice(), Some("ctx-a"), "work", true).await;
        rig.until(&alice(), &one.id, TaskState::Completed).await;
        let before = rig
            .store
            .count_runs(&adam_core::RunQuery::new(
                &rig.helper,
                adam_core::ConversationScope::Prefix(String::new()),
                0,
            ))
            .await
            .unwrap();

        let plain_alice = plain("alice");
        assert!(invalid(
            rig.submit(&plain_alice, said("hi", Some(&one.context_id)), None)
                .await
        ));
        assert!(invalid(
            rig.submit(&plain_alice, said("hi", None), Some(&one.id))
                .await
        ));
        assert!(matches!(
            rig.backend.get(&plain_alice, &one.id).await,
            Err(BackendError::InvalidParams(_))
        ));
        assert!(matches!(
            rig.backend.cancel(&plain_alice, &one.id).await,
            Err(BackendError::InvalidParams(_))
        ));
        let mut query = TaskQuery::new();
        query.context_id = Some(one.context_id.clone());
        assert!(matches!(
            rig.backend.list(&plain_alice, &query).await,
            Err(BackendError::InvalidParams(_))
        ));
        let after = rig
            .store
            .count_runs(&adam_core::RunQuery::new(
                &rig.helper,
                adam_core::ConversationScope::Prefix(String::new()),
                0,
            ))
            .await
            .unwrap();
        assert_eq!(before, after, "nothing was written");

        // A root message naming a child-shaped context is refused whether or not activated.
        assert!(invalid(
            rig.submit(&plain_alice, said("hi", Some(&one.context_id)), None)
                .await
        ));

        // A `start` that was not activated is a plain message: a turn of the parent.
        rig.parent_mock.push_text("a parent turn");
        let mut message = said("hello", Some("ctx-b"));
        message.metadata = Some(HashMap::from([(
            CHILD_SESSIONS_EXTENSION.to_owned(),
            json!({"start": {"agent": "planner", "description": "d"}}),
        )]));
        let task = rig.submit(&plain_alice, message, None).await.unwrap();
        assert_eq!(task.context_id, "ctx-b");
        assert!(object(&task).is_null());
        rig.stop().await;
    }
}

/// A session of another caller is never visible: a message is `InvalidParams`, a task is not
/// found, `ListTasks` is empty, and a start in another caller's context never touches it.
#[tokio::test]
async fn another_caller_sees_nothing_of_a_session() {
    for store in stores().await {
        let rig = Rig::new(&store);
        rig.helper_mock.push_text("answer");
        let one = rig.start(&alice(), None, "work", true).await;
        rig.until(&alice(), &one.id, TaskState::Completed).await;
        let bob = activated("bob");
        assert!(invalid(
            rig.submit(&bob, said("hi", Some(&one.context_id)), None)
                .await
        ));
        assert!(invalid(
            rig.submit(&bob, close_message(&one.context_id, false), None)
                .await
        ));
        assert!(matches!(
            rig.submit(&bob, said("hi", Some(&one.context_id)), Some(&one.id))
                .await,
            Err(BackendError::TaskNotFound(_))
        ));
        assert!(rig.backend.get(&bob, &one.id).await.unwrap().is_none());
        assert!(matches!(
            rig.backend.cancel(&bob, &one.id).await,
            Err(BackendError::TaskNotFound(_))
        ));
        let mut query = TaskQuery::new();
        query.context_id = Some(one.context_id.clone());
        assert!(
            rig.backend
                .list(&bob, &query)
                .await
                .unwrap()
                .tasks
                .is_empty()
        );
        let mine = rig.backend.list(&alice(), &query).await.unwrap();
        assert_eq!(mine.tasks.len(), 1);
        assert_eq!(object(&mine.tasks[0])["round"], 1);
        rig.stop().await;
    }
}

/// A malformed or over-limit `start` is refused with nothing started, and a session cannot hold a
/// session.
#[tokio::test]
async fn a_start_is_refused_before_anything_is_written() {
    for store in stores().await {
        let rig = Rig::with(
            &store,
            Options {
                max_open: 1,
                ..Options::new()
            },
        );
        let with = |start: Value| {
            let mut message = said("go", Some("ctx-r"));
            message.metadata = Some(HashMap::from([(
                CHILD_SESSIONS_EXTENSION.to_owned(),
                json!({ "start": start }),
            )]));
            message
        };
        let long = "x".repeat(81);
        for start in [
            json!({"agent": "nobody", "description": "d"}),
            json!({"agent": "planner", "description": long}),
            json!({"agent": "planner"}),
            json!({"agent": "planner", "description": "d", "keepOpen": 1}),
            json!("start"),
        ] {
            assert!(
                invalid(rig.submit(&alice(), with(start.clone()), None).await),
                "{start}"
            );
        }
        // taskId with a start is refused.
        assert!(invalid(
            rig.submit(
                &alice(),
                with(json!({"agent": "planner", "description": "d"})),
                Some("00000000-0000-0000-0000-000000000000")
            )
            .await
        ));
        // maxOpen: the second open session is refused.
        rig.helper_mock.push_text("one");
        let one = rig.start(&alice(), Some("ctx-r"), "first", true).await;
        rig.until(&alice(), &one.id, TaskState::Completed).await;
        assert!(unsupported(
            rig.submit(
                &alice(),
                with(json!({"agent": "planner", "description": "d", "keepOpen": true})),
                None
            )
            .await
        ));
        // A session cannot hold a session.
        let deeper = adam_runtime::session_conversation_id(&one.context_id, RunId::new());
        assert!(invalid(
            rig.submit(&alice(), said("hi", Some(&deeper)), None).await
        ));
        let none = rig
            .runtime
            .list_sessions("alice:ctx-r", &[rig.helper.as_str()], 0, 10)
            .await
            .unwrap();
        assert_eq!(none.sessions.len(), 1, "only the first session exists");
        rig.stop().await;
    }
}

/// `ListTasks {contextId: K}` pages the rounds, newest first, each saying its session; and a task
/// of the parent context says its sessions, open first.
#[tokio::test]
async fn rounds_are_listed_and_the_parent_task_says_its_sessions() {
    for store in stores().await {
        let rig = Rig::new(&store);
        rig.helper_mock.push_text("a").push_text("b");
        let one = rig.start(&alice(), Some("ctx-l"), "one", true).await;
        rig.until(&alice(), &one.id, TaskState::Completed).await;
        let two = rig
            .submit(&alice(), said("two", Some(&one.context_id)), None)
            .await
            .unwrap();
        rig.until(&alice(), &two.id, TaskState::Completed).await;

        let mut query = TaskQuery::new();
        query.context_id = Some(one.context_id.clone());
        let page = rig.backend.list(&alice(), &query).await.unwrap();
        let ids: Vec<&str> = page.tasks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, [two.id.as_str(), one.id.as_str()]);
        assert!(page.tasks.iter().all(|t| object(t)["session"] == one.id));

        // A parent task: the sessions object.
        rig.parent_mock.push_text("parent says");
        let parent = rig
            .submit(&alice(), said("hello", Some("ctx-l")), None)
            .await
            .unwrap();
        let sessions = object(&parent)["sessions"].clone();
        assert_eq!(sessions.as_array().unwrap().len(), 1);
        assert_eq!(sessions[0]["session"], one.id);
        assert_eq!(sessions[0]["contextId"], one.context_id);
        assert_eq!(sessions[0]["agent"], "planner");
        assert_eq!(sessions[0]["state"], "idle");
        assert_eq!(sessions[0]["round"]["number"], 2);
        assert_eq!(sessions[0]["round"]["taskId"], two.id);
        let got = rig.task(&alice(), &parent.id).await;
        assert_eq!(object(&got)["sessions"], sessions);
        let mut roots = TaskQuery::new();
        roots.context_id = Some("ctx-l".into());
        let listed = rig.backend.list(&alice(), &roots).await.unwrap();
        assert_eq!(
            listed.tasks.len(),
            1,
            "the rounds are not tasks of the parent context"
        );
        assert_eq!(object(&listed.tasks[0])["sessions"], sessions);
        // Not activated: nothing.
        let quiet = rig
            .backend
            .get(&plain("alice"), &parent.id)
            .await
            .unwrap()
            .unwrap();
        assert!(object(&quiet).is_null());
        rig.stop().await;
    }
}

/// A session the parent's model starts is the same kind of session: it is in the parent task's
/// object, its round says `by: agent` and is a child of the parent run, and a person can talk to it.
#[tokio::test]
async fn a_session_the_model_starts_is_listed_and_a_person_can_talk_to_it() {
    for store in stores().await {
        let rig = Rig::new(&store);
        rig.helper_mock.push_text("helped").push_text("more help");
        rig.parent_mock
            .push_tool_calls(vec![ToolCall::new(
                "c1",
                "subagent_start",
                json!({"agent": "helper", "description": "look", "prompt": "go", "keep_open": true}),
            )])
            .push_text("started")
            .push_text("digested");
        let parent = rig
            .submit(&alice(), said("start a helper", Some("ctx-m")), None)
            .await
            .unwrap();
        rig.until(&alice(), &parent.id, TaskState::Completed).await;
        let parent = rig.task(&alice(), &parent.id).await;
        let sessions = object(&parent)["sessions"].clone();
        assert_eq!(sessions.as_array().unwrap().len(), 1, "{sessions}");
        let (child, round) = (
            sessions[0]["contextId"].as_str().unwrap().to_owned(),
            sessions[0]["round"]["taskId"].as_str().unwrap().to_owned(),
        );
        assert_eq!(sessions[0]["agent"], "planner");
        assert_eq!(sessions[0]["description"], "look");
        assert_eq!(sessions[0]["keepOpen"], true);

        let first = rig.task(&alice(), &round).await;
        assert_eq!(object(&first)["by"], "agent");
        let rec = store
            .load_run(RunId(round.parse().unwrap()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rec.parent_id, Some(RunId(parent.id.parse().unwrap())));

        let two = rig
            .submit(&alice(), said("one more thing", Some(&child)), None)
            .await
            .unwrap();
        assert_eq!(object(&two)["by"], "person");
        assert_eq!(object(&two)["round"], 2);
        let rec = store
            .load_run(RunId(two.id.parse().unwrap()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            rec.parent_id,
            Some(rec.id),
            "the person's round is its own parent"
        );
        rig.stop().await;
    }
}

/// The access tier: a round a person starts runs under the tier the message names, or the agent's
/// default when it does not name one, and says so.
#[tokio::test]
async fn a_round_a_person_starts_runs_under_a_tier() {
    for store in stores().await {
        let rig = Rig::with(
            &store,
            Options {
                tiers: true,
                ..Options::new()
            },
        );
        rig.helper_mock.push_text("a").push_text("b");
        let mut with_tiers = alice();
        with_tiers.extensions.push(ACCESS_TIER_EXTENSION.to_owned());

        // Not activated: the default is written.
        let one = rig.start(&alice(), None, "one", true).await;
        let said_tier = |task: &Task| {
            task.metadata
                .as_ref()
                .and_then(|m| m.get(ACCESS_TIER_EXTENSION))
                .map(|t| t["tier"].clone())
        };
        let read = rig.task(&with_tiers, &one.id).await;
        assert_eq!(said_tier(&read), Some(json!("auto")));
        rig.until(&alice(), &one.id, TaskState::Completed).await;

        // Activated: the message names a tier, or nothing is written.
        let none = rig
            .submit(&with_tiers, said("two", Some(&one.context_id)), None)
            .await;
        assert!(invalid(none));
        let mut named = said("two", Some(&one.context_id));
        named.metadata = Some(HashMap::from([(
            ACCESS_TIER_EXTENSION.to_owned(),
            json!({"tier": "plan"}),
        )]));
        let two = rig.submit(&with_tiers, named, None).await.unwrap();
        assert_eq!(said_tier(&two), Some(json!("plan")));
        rig.stop().await;
    }
}

/// `maxOpen` counts every open session, not the first page of the listing: with a page of two, a
/// closed session and three open ones, a fourth start is refused. (The closed one does not count.)
#[tokio::test]
async fn max_open_counts_across_pages() {
    for store in stores().await {
        let rig = Rig::with(
            &store,
            Options {
                max_open: 3,
                listing_page: 2,
                ..Options::new()
            },
        );
        rig.helper_mock.push_text("closed one");
        let closed = rig.start(&alice(), Some("ctx-p"), "c", false).await;
        rig.until(&alice(), &closed.id, TaskState::Completed).await;
        for n in 0..3 {
            rig.helper_mock.push_text(format!("open {n}"));
            let open = rig
                .start(&alice(), Some("ctx-p"), &format!("o{n}"), true)
                .await;
            rig.until(&alice(), &open.id, TaskState::Completed).await;
        }
        let mut message = said("one too many", Some("ctx-p"));
        message.metadata = Some(HashMap::from([(
            CHILD_SESSIONS_EXTENSION.to_owned(),
            json!({"start": {"agent": "planner", "description": "d", "keepOpen": true}}),
        )]));
        assert!(unsupported(rig.submit(&alice(), message, None).await));
        rig.stop().await;
    }
}

/// The refusals of a close, a start in a child context and the rounds limit; and a close is
/// allowed at the limit.
#[tokio::test]
async fn the_other_refusals_and_a_close_at_the_round_limit() {
    for store in stores().await {
        let rig = Rig::with(
            &store,
            Options {
                max_rounds: 2,
                ..Options::new()
            },
        );
        rig.helper_mock.push_text("a").push_text("b");
        let one = rig.start(&alice(), Some("ctx-q"), "one", true).await;
        rig.until(&alice(), &one.id, TaskState::Completed).await;

        // A start together with a close.
        let mut both = said("x", Some("ctx-q"));
        both.metadata = Some(HashMap::from([(
            CHILD_SESSIONS_EXTENSION.to_owned(),
            json!({"start": {"agent": "planner", "description": "d"}, "close": {}}),
        )]));
        assert!(invalid(rig.submit(&alice(), both, None).await));
        // A close that names a taskId, and a close to the parent context.
        assert!(invalid(
            rig.submit(
                &alice(),
                close_message(&one.context_id, false),
                Some(&one.id)
            )
            .await
        ));
        assert!(invalid(
            rig.submit(&alice(), close_message("ctx-q", false), None)
                .await
        ));
        // A start in a child context.
        let mut start = said("x", Some(&one.context_id));
        start.metadata = Some(HashMap::from([(
            CHILD_SESSIONS_EXTENSION.to_owned(),
            json!({"start": {"agent": "planner", "description": "d"}}),
        )]));
        assert!(invalid(rig.submit(&alice(), start, None).await));
        // ListTasks on a context nested too deep is an empty page.
        let deeper = adam_runtime::session_conversation_id(&one.context_id, RunId::new());
        let mut query = TaskQuery::new();
        query.context_id = Some(deeper);
        assert!(
            rig.backend
                .list(&alice(), &query)
                .await
                .unwrap()
                .tasks
                .is_empty()
        );

        // Round 2 is the last: a third message is refused, a close is not.
        let two = rig
            .submit(&alice(), said("two", Some(&one.context_id)), None)
            .await
            .unwrap();
        rig.until(&alice(), &two.id, TaskState::Completed).await;
        assert!(unsupported(
            rig.submit(&alice(), said("three", Some(&one.context_id)), None)
                .await
        ));
        let closed = rig
            .submit(&alice(), close_message(&one.context_id, false), None)
            .await
            .unwrap();
        assert_eq!(object(&closed)["state"], "closed");
        rig.stop().await;
    }
}

/// A session an embedder made with no header is continued with the contract's default, which is
/// not to keep it open: the next round closes it when it ends.
#[tokio::test]
async fn a_session_with_no_header_is_continued_as_not_kept_open() {
    for store in stores().await {
        let rig = Rig::new(&store);
        rig.helper_mock.push_text("a").push_text("b");
        let session = RunId::new();
        let conversation = adam_runtime::session_conversation_id("alice:ctx-h", session);
        rig.runtime
            .start_child_continuing(
                session,
                session,
                &rig.helper,
                adam_runtime::Inbound::new("message", json!({"text": "one"})),
                &conversation,
                None,
            )
            .await
            .unwrap();
        let child = adam_runtime::session_conversation_id("ctx-h", session);
        rig.until(&alice(), &session.to_string(), TaskState::Completed)
            .await;
        let two = rig
            .submit(&alice(), said("two", Some(&child)), None)
            .await
            .unwrap();
        assert_eq!(object(&two)["keepOpen"], false);
        let done = rig.until(&alice(), &two.id, TaskState::Completed).await;
        assert_eq!(object(&done)["state"], "closed");
        rig.stop().await;
    }
}

/// Once the closing round has ended its session is closed and not `closing`: that word is for a
/// session that is closed while a round still works.
#[tokio::test]
async fn a_closed_idle_session_does_not_say_closing() {
    for store in stores().await {
        let rig = Rig::new(&store);
        rig.helper_mock.push_text("answer");
        let one = rig.start(&alice(), None, "work", true).await;
        rig.until(&alice(), &one.id, TaskState::Completed).await;
        let closed = rig
            .submit(&alice(), close_message(&one.context_id, false), None)
            .await
            .unwrap();
        let ended = rig.until(&alice(), &closed.id, TaskState::Completed).await;
        assert_eq!(object(&ended)["state"], "closed");
        assert_eq!(object(&ended)["closing"], false);
        rig.stop().await;
    }
}

/// A session asked to close while its round works still counts against `maxOpen` until the round
/// ends; a second close with `cancel` then stops the round.
#[tokio::test]
async fn a_closing_session_counts_open_and_a_later_cancel_close_stops_its_round() {
    for store in stores().await {
        let rig = Rig::with(
            &store,
            Options {
                hold_helper_on: Some(0),
                max_open: 1,
                ..Options::new()
            },
        );
        rig.helper_mock.push_text("never").push_text("other");
        let one = rig.start(&alice(), Some("ctx-c"), "work", true).await;
        rig.gate.reached.notified().await;
        let asked = rig
            .submit(&alice(), close_message(&one.context_id, false), None)
            .await
            .unwrap();
        assert_eq!(object(&asked)["closing"], true);

        let mut second = said("another", Some("ctx-c"));
        second.metadata = Some(HashMap::from([(
            CHILD_SESSIONS_EXTENSION.to_owned(),
            json!({"start": {"agent": "planner", "description": "d", "keepOpen": true}}),
        )]));
        assert!(
            unsupported(rig.submit(&alice(), second, None).await),
            "a session that is closing but still working is open"
        );

        // A repeated plain close changes nothing; a close with cancel stops the round.
        let again = rig
            .submit(&alice(), close_message(&one.context_id, false), None)
            .await
            .unwrap();
        assert_eq!(again.id, one.id);
        let canceled = rig
            .submit(&alice(), close_message(&one.context_id, true), None)
            .await
            .unwrap();
        assert_eq!(canceled.id, one.id);
        let old = rig.task(&alice(), &one.id).await;
        assert_eq!(old.status.state, TaskState::Canceled);
        rig.gate.release.notify_one();
        rig.stop().await;
    }
}

/// A session the model starts with a helper the card does not list is not reachable as a task or
/// a session through this extension.
#[tokio::test]
async fn a_round_of_a_helper_the_card_does_not_list_is_not_found() {
    for store in stores().await {
        let rig = Rig::new(&store);
        rig.hidden_mock.push_text("hidden answer");
        rig.parent_mock
            .push_tool_calls(vec![ToolCall::new(
                "c1",
                "subagent_start",
                json!({"agent": "hidden", "description": "look", "prompt": "go", "keep_open": true}),
            )])
            .push_text("started")
            .push_text("digested");
        let parent = rig
            .submit(&alice(), said("start one", Some("ctx-h")), None)
            .await
            .unwrap();
        rig.until(&alice(), &parent.id, TaskState::Completed).await;
        let query = adam_core::RunQuery::new(
            rig.hidden.clone(),
            adam_core::ConversationScope::Prefix(String::new()),
            10,
        );
        let runs = store.list_runs(&query).await.unwrap();
        assert_eq!(runs.len(), 1, "the model started the hidden helper");
        let id = runs[0].id.to_string();
        assert!(rig.backend.get(&alice(), &id).await.unwrap().is_none());
        assert!(matches!(
            rig.backend.cancel(&alice(), &id).await,
            Err(BackendError::TaskNotFound(_))
        ));
        rig.stop().await;
    }
}

/// The first event of a subscription says the sessions of a parent task and the session of a round.
#[tokio::test]
async fn a_subscription_snapshot_says_the_sessions() {
    for store in stores().await {
        let rig = Rig::new(&store);
        rig.helper_mock.push_text("a");
        let one = rig.start(&alice(), Some("ctx-s"), "one", true).await;
        rig.until(&alice(), &one.id, TaskState::Completed).await;
        rig.parent_mock.push_text("parent says");
        let parent = rig
            .submit(&alice(), said("hello", Some("ctx-s")), None)
            .await
            .unwrap();
        rig.until(&alice(), &parent.id, TaskState::Completed).await;

        let snapshot = |event: Option<Result<adam_a2a::TaskEvent, BackendError>>| match event {
            Some(Ok(adam_a2a::TaskEvent::Snapshot(task))) => task,
            other => panic!("expected a snapshot, got {other:?}"),
        };
        let mut events = rig.backend.subscribe(&alice(), &parent.id);
        let task = snapshot(events.next().await);
        assert_eq!(object(&task)["sessions"].as_array().map(Vec::len), Some(1));
        let mut events = rig.backend.subscribe(&alice(), &one.id);
        let task = snapshot(events.next().await);
        assert_eq!(object(&task)["session"], one.id);
        rig.stop().await;
    }
}

// ---------------------------------------------------------------------------
// Events (slice 6, part b)
// ---------------------------------------------------------------------------

/// The stream of `id`, read up to its snapshot: what the subscription knows is then fixed.
async fn subscribed(
    rig: &Rig,
    caller: &Caller,
    id: &str,
) -> BoxStream<'static, Result<adam_a2a::TaskEvent, BackendError>> {
    let mut events = rig.backend.subscribe(caller, id);
    match events.next().await {
        Some(Ok(adam_a2a::TaskEvent::Snapshot(_))) => events,
        other => panic!("expected a snapshot, got {other:?}"),
    }
}

/// What a stream said under the extension's URI, in order: for each status update that carries
/// it, the object. Reads until the stream ends, or `until` says it has enough, or 20 s pass.
async fn said_on(
    events: &mut BoxStream<'static, Result<adam_a2a::TaskEvent, BackendError>>,
    until: impl Fn(&[Value]) -> bool,
) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut out = Vec::new();
    while Instant::now() < deadline && !until(&out) {
        let Ok(next) = tokio::time::timeout(Duration::from_millis(200), events.next()).await else {
            continue;
        };
        match next {
            None => break,
            Some(Err(e)) => panic!("the stream failed: {e}"),
            Some(Ok(adam_a2a::TaskEvent::Status(update))) => {
                if let Some(o) = update
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get(CHILD_SESSIONS_EXTENSION))
                {
                    out.push(o.clone());
                }
            }
            Some(Ok(_)) => {}
        }
    }
    out
}

/// [`said_on`] in a task of its own, for a test that acts while the stream is read.
fn spawn_said(
    mut events: BoxStream<'static, Result<adam_a2a::TaskEvent, BackendError>>,
    until: impl Fn(&[Value]) -> bool + Send + 'static,
) -> tokio::task::JoinHandle<Vec<Value>> {
    tokio::spawn(async move { said_on(&mut events, until).await })
}

fn names(said: &[Value]) -> Vec<String> {
    said.iter()
        .map(|o| match o["report"].as_str() {
            Some(report) => format!("{}:{report}", o["event"].as_str().unwrap_or("?")),
            None => o["event"].as_str().unwrap_or("?").to_owned(),
        })
        .collect()
}

fn event_count(n: usize) -> impl Fn(&[Value]) -> bool {
    move |said| said.len() >= n
}

/// A parent task that has a session its model starts says `opened`, `reported` and `closed` of it, in
/// that order, each as a status update of the parent with the members the contract lists.
#[tokio::test]
async fn an_activated_parent_stream_reports_opened_reported_and_closed_for_a_session_its_model_starts()
 {
    for store in stores().await {
        let rig = Rig::with(
            &store,
            Options {
                hold_parent_on: Some(0),
                hold_helper_on: Some(0),
                ..Options::new()
            },
        );
        rig.helper_mock.push_text("planned");
        rig.parent_mock
            .push_tool_calls(vec![ToolCall::new(
                "c1",
                "subagent_start",
                json!({"agent": "helper", "description": "look", "prompt": "go", "keep_open": false}),
            )])
            .push_text("started")
            .push_text("digested");
        let parent = rig
            .submit(&alice(), said("start a helper", Some("ctx-e")), None)
            .await
            .unwrap();
        rig.parent_gate.reached.notified().await;
        let events = subscribed(&rig, &alice(), &parent.id).await;
        let reader = spawn_said(events, |said| said.iter().any(|o| o["event"] == "closed"));
        rig.parent_gate.release.notify_one();
        rig.gate.reached.notified().await;
        // The helper has not answered yet: `opened` first, saying the round works.
        rig.gate.release.notify_one();
        let said = reader.await.unwrap();
        assert_eq!(
            names(&said),
            ["opened", "reported:done", "closed"],
            "{said:?}"
        );

        let opened = &said[0];
        assert_eq!(opened["agent"], "planner");
        assert_eq!(opened["description"], "look");
        assert_eq!(opened["keepOpen"], false);
        assert_eq!(opened["by"], "agent");
        assert_eq!(opened["round"]["number"], 1);
        assert_eq!(opened["round"]["state"], "TASK_STATE_WORKING");
        assert_eq!(opened["session"], opened["round"]["taskId"]);
        assert_eq!(
            opened["contextId"],
            adam_runtime::session_conversation_id(
                "ctx-e",
                RunId(opened["session"].as_str().unwrap().parse().unwrap())
            )
        );
        assert_eq!(said[1]["round"]["state"], "TASK_STATE_COMPLETED");
        assert_eq!(said[1]["session"], opened["session"]);
        assert!(
            said[2].get("round").is_none() && said[2].get("report").is_none(),
            "{:?}",
            said[2]
        );
        for o in &said {
            for member in [
                "session",
                "contextId",
                "agent",
                "description",
                "keepOpen",
                "by",
            ] {
                assert!(o.get(member).is_some(), "{member} missing in {o}");
            }
        }
        rig.until(&alice(), &parent.id, TaskState::Completed).await;
        rig.stop().await;
    }
}

/// A person who starts a session while the parent task runs is told to the parent's stream as
/// `opened` with `by: person`, and its round's end is `reported`.
#[tokio::test]
async fn a_persons_start_is_reported_opened_by_person_on_a_running_parent() {
    for store in stores().await {
        let rig = Rig::with(
            &store,
            Options {
                hold_parent_on: Some(0),
                ..Options::new()
            },
        );
        rig.helper_mock.push_text("a plan");
        rig.parent_mock.push_text("parent done");
        let parent = rig
            .submit(&alice(), said("work", Some("ctx-p")), None)
            .await
            .unwrap();
        rig.parent_gate.reached.notified().await;
        let events = subscribed(&rig, &alice(), &parent.id).await;
        let reader = spawn_said(events, event_count(2));
        let start = rig.start(&alice(), Some("ctx-p"), "plan it", true).await;
        let said = reader.await.unwrap();
        assert_eq!(names(&said), ["opened", "reported:done"], "{said:?}");
        assert_eq!(said[0]["by"], "person");
        assert_eq!(said[0]["session"], start.id);
        assert_eq!(said[0]["contextId"], start.context_id);
        assert_eq!(said[0]["keepOpen"], true);
        rig.parent_gate.release.notify_one();
        rig.until(&alice(), &parent.id, TaskState::Completed).await;
        rig.stop().await;
    }
}

/// A message to an idle session starts a round that is not the first: `round`, then its `reported`.
#[tokio::test]
async fn a_round_two_by_the_person_is_reported_as_round() {
    for store in stores().await {
        let rig = Rig::with(
            &store,
            Options {
                hold_parent_on: Some(0),
                ..Options::new()
            },
        );
        rig.helper_mock.push_text("one").push_text("two");
        rig.parent_mock.push_text("parent done");
        let parent = rig
            .submit(&alice(), said("work", Some("ctx-r")), None)
            .await
            .unwrap();
        rig.parent_gate.reached.notified().await;
        let mut events = subscribed(&rig, &alice(), &parent.id).await;
        let one = rig.start(&alice(), Some("ctx-r"), "plan it", true).await;
        // Round 1 is told before round 2 starts: a session first seen at round 2 is `opened`.
        let mut told = said_on(&mut events, event_count(2)).await;
        let two = rig
            .submit(&alice(), said("again", Some(&one.context_id)), None)
            .await
            .unwrap();
        told.extend(said_on(&mut events, |more| more.len() >= 2).await);
        let round_events: Vec<_> = told.iter().filter(|o| o["event"] == "round").collect();
        assert_eq!(
            names(&told),
            ["opened", "reported:done", "round", "reported:done"],
            "{told:?}"
        );
        assert_eq!(round_events[0]["round"]["number"], 2);
        assert_eq!(round_events[0]["round"]["taskId"], two.id);
        assert_eq!(round_events[0]["by"], "person");
        rig.parent_gate.release.notify_one();
        rig.stop().await;
    }
}

/// Without the extension the stream says nothing of sessions, though they happen.
#[tokio::test]
async fn an_unactivated_parent_stream_carries_no_event() {
    for store in stores().await {
        let rig = Rig::with(
            &store,
            Options {
                hold_parent_on: Some(0),
                ..Options::new()
            },
        );
        rig.helper_mock.push_text("a plan");
        rig.parent_mock.push_text("parent done");
        let parent = rig
            .submit(&plain("alice"), said("work", Some("ctx-q")), None)
            .await
            .unwrap();
        rig.parent_gate.reached.notified().await;
        let events = subscribed(&rig, &plain("alice"), &parent.id).await;
        let reader = spawn_said(events, |_| false);
        let start = rig.start(&alice(), Some("ctx-q"), "plan it", true).await;
        rig.until(&alice(), &start.id, TaskState::Completed).await;
        // Let the stream poll past the session's change before the parent ends.
        tokio::time::sleep(Duration::from_millis(1300)).await;
        rig.parent_gate.release.notify_one();
        let said = reader.await.unwrap();
        assert!(said.is_empty(), "{said:?}");
        rig.stop().await;
    }
}

/// The stream of a round says the session on the update that ends it: `idle` for a kept-open
/// session, `closed` for another.
#[tokio::test]
async fn a_round_stream_ends_with_the_session_idle_or_closed() {
    for (keep_open, state) in [(true, "idle"), (false, "closed")] {
        for store in stores().await {
            let rig = Rig::with(
                &store,
                Options {
                    hold_helper_on: Some(0),
                    ..Options::new()
                },
            );
            rig.helper_mock.push_text("a plan");
            let start = rig.start(&alice(), None, "plan it", keep_open).await;
            rig.gate.reached.notified().await;
            let events = subscribed(&rig, &alice(), &start.id).await;
            let reader = spawn_said(events, |_| false);
            rig.gate.release.notify_one();
            let said = reader.await.unwrap();
            let last = said.last().unwrap_or_else(|| panic!("nothing said"));
            assert_eq!(last["state"], state, "{said:?}");
            assert_eq!(last["closing"], false);
            assert_eq!(last["round"], 1);
            assert_eq!(last["session"], start.id);
            rig.stop().await;
        }
    }
}

/// A close asked of a working session turns `closing` on, and the stream of its round says so
/// before the round ends.
#[tokio::test]
async fn a_close_request_marks_the_round_stream_closing() {
    for store in stores().await {
        let rig = Rig::with(
            &store,
            Options {
                hold_helper_on: Some(0),
                ..Options::new()
            },
        );
        rig.helper_mock.push_text("a plan");
        let start = rig.start(&alice(), None, "plan it", true).await;
        rig.gate.reached.notified().await;
        let events = subscribed(&rig, &alice(), &start.id).await;
        let reader = spawn_said(events, |said| said.iter().any(|o| o["closing"] == true));
        let asked = rig
            .submit(&alice(), close_message(&start.context_id, false), None)
            .await
            .unwrap();
        assert_eq!(object(&asked)["closing"], true);
        let said = reader.await.unwrap();
        let closing = said.last().unwrap_or_else(|| panic!("nothing said"));
        assert_eq!(closing["closing"], true, "{said:?}");
        assert_eq!(closing["state"], "working");
        rig.gate.release.notify_one();
        let done = rig.until(&alice(), &start.id, TaskState::Completed).await;
        assert_eq!(object(&done)["state"], "closed");
        rig.stop().await;
    }
}
