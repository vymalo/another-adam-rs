//! `child-sessions/v1` from a definition (ADR 0045, slice 6 part c): the helper list the card, the
//! A2A front and the session tools share, the starters a control plane registers for the helpers,
//! and a person's session started on a control plane and worked by a worker, on every store.
#![cfg(feature = "a2a")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use a2a::{Message, Part, Role, Task, TaskState};
use adam_a2a::{CHILD_SESSIONS_EXTENSION, Caller, TaskBackend};
use adam_a2a_runtime::{ChildSessionAgent, ChildSessions, RuntimeTaskBackend};
use adam_assembly::AgentDef;
use adam_core::{DynStore, MemoryStore, RunId};
use adam_llm_agent::{LlmStarter, ToolSet};
use adam_model::MockModel;
use adam_runtime::{BroadcastSink, Runtime, SessionLimits};
use common::{def, instructions, spawn_worker};
use proptest::prelude::*;
use serde_json::{Value, json};

const CARD: &str = "http://localhost:1/.well-known/agent-card.json";

async fn stores() -> Vec<(&'static str, DynStore)> {
    let mut all: Vec<(&'static str, DynStore)> = vec![("memory", Arc::new(MemoryStore::new()))];
    if let Some(url) = adam_core::testing::test_env("ADAM_TEST_POSTGRES_URL") {
        let store = adam_store_postgres::PgStore::connect(&url)
            .await
            .expect("connect to postgres");
        adam_core::Store::migrate(&store).await.expect("migrate");
        all.push(("postgres", Arc::new(store)));
    }
    if let Some(uri) = adam_core::testing::test_env("ADAM_TEST_MONGODB_URI") {
        let db = std::env::var("ADAM_TEST_MONGODB_DB").unwrap_or_else(|_| "adam_test".into());
        let store = adam_store_mongodb::MongoStore::connect(&uri, &db)
            .await
            .expect("connect to mongodb")
            .with_collection_prefix("adam_asm_")
            .expect("collection prefix");
        adam_core::Store::migrate(&store).await.expect("migrate");
        all.push(("mongodb", Arc::new(store)));
    }
    all
}

fn uniq(prefix: &str) -> String {
    format!("{prefix}-{}", RunId::new().0.simple())
}

/// A root with a local `explorer` (a 5-minute limit), a local `reviewer` (the default limit) and a
/// remote `billing`; the folder loader orders them by name.
fn files(root: &str, frontmatter: &str) -> Vec<(String, String)> {
    vec![
        (
            "agent/instructions.md".into(),
            instructions(&format!("name: {root}\n{frontmatter}"), "You are the root."),
        ),
        (
            "agent/subagents/explorer.md".into(),
            instructions(
                "description: Looks around.\nlimits:\n  max_minutes: 5",
                "You explore.",
            ),
        ),
        (
            "agent/subagents/reviewer.md".into(),
            instructions("description: Reviews a change.", "You review."),
        ),
        (
            "agent/subagents/billing.md".into(),
            format!("---\ndescription: Handles billing.\na2a: {CARD}\n---\n"),
        ),
    ]
}

fn def_of(files: &[(String, String)]) -> AgentDef {
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect();
    def(&refs)
}

fn expected(root: &str, limits: SessionLimits, remote_wait: Duration) -> ChildSessions {
    ChildSessions::new(
        [
            ChildSessionAgent::new("billing", format!("{root}/billing"), "Handles billing.")
                .remote()
                .within(remote_wait),
            ChildSessionAgent::new("explorer", format!("{root}/explorer"), "Looks around.")
                .within(Duration::from_secs(300)),
            ChildSessionAgent::new("reviewer", format!("{root}/reviewer"), "Reviews a change.")
                .within(Duration::from_secs(30 * 60)),
        ],
        limits.max_open(),
    )
    .unwrap()
    .with_limits(limits)
}

fn names(sessions: &ChildSessions) -> Vec<String> {
    let card = sessions.card_extension().expect("a card entry");
    card.params["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn the_helpers_are_the_roots_subagents_in_file_order_with_their_time_limits() {
    let root = uniq("root");
    let def = def_of(&files(&root, "")).remote_timeout(Duration::from_secs(90));
    let sessions = def.child_sessions().expect("a root with subagents");
    let got: Vec<_> = names(&sessions);
    assert_eq!(got, ["billing", "explorer", "reviewer"]);
    // Compared whole: names, registered names, descriptions, limits and the file's limits.
    let want = ChildSessions::new(
        [
            ChildSessionAgent::new("billing", format!("{root}/billing"), "Handles billing.")
                .remote()
                .within(Duration::from_secs(90)),
            ChildSessionAgent::new("explorer", format!("{root}/explorer"), "Looks around.")
                .within(Duration::from_secs(300)),
            ChildSessionAgent::new("reviewer", format!("{root}/reviewer"), "Reviews a change.")
                .within(Duration::from_secs(1800)),
        ],
        16,
    )
    .unwrap()
    .with_limits(SessionLimits::default());
    assert_eq!(sessions, want);
}

#[test]
fn no_subagent_means_no_sessions_and_grandchildren_are_not_listed() {
    let root = uniq("root");
    let alone = def(&[(
        "agent/instructions.md",
        &instructions(&format!("name: {root}"), "Alone."),
    )]);
    assert!(alone.child_sessions().is_none());

    let nested = def(&[
        (
            "agent/instructions.md",
            &instructions(&format!("name: {root}"), "Root."),
        ),
        (
            "agent/subagents/researcher/instructions.md",
            &instructions("description: Researches.", "Research."),
        ),
        (
            "agent/subagents/researcher/subagents/summarizer.md",
            &instructions("description: Summarises.", "Summarise."),
        ),
    ]);
    let sessions = nested.child_sessions().unwrap();
    assert_eq!(names(&sessions), ["researcher"]);
}

#[test]
fn the_file_lowers_the_deployments_limits_and_never_raises_them() {
    let root = uniq("root");
    let deployment = SessionLimits::new(10, 3, 12).unwrap();
    let lowered = def_of(&files(
        &root,
        "limits:\n  max_open_sessions: 4\n  max_working_sessions: 2\n  max_session_rounds: 6",
    ))
    .session_limits(deployment);
    let low = SessionLimits::new(4, 2, 6).unwrap();
    assert_eq!(lowered.effective_session_limits(), low);
    assert_eq!(
        lowered.child_sessions().unwrap(),
        expected(&root, low, Duration::from_secs(3600)),
        "the card's maxOpen and the front's limits are the effective ones"
    );

    let raised =
        def_of(&files(&root, "limits:\n  max_open_sessions: 50")).session_limits(deployment);
    assert_eq!(raised.effective_session_limits().max_open(), 10);
    assert_eq!(
        raised.child_sessions().unwrap(),
        expected(&root, deployment, Duration::from_secs(3600)),
        "a file cannot raise what the deployment allows"
    );
}

/// The limits and the helpers the root's own session tools use are the ones the card and the front
/// are made from. Read from the bound root, not from the definition twice: the tools are asked to
/// open a second session where the effective limit is one, and the spec of `subagent_start` is
/// compared with the card's helpers.
#[tokio::test]
async fn the_bound_session_tools_hold_the_effective_limits_and_helpers_of_the_definition() {
    let root = uniq("root");
    let build = || {
        def_of(&files(&root, "limits:\n  max_open_sessions: 1"))
            .session_limits(SessionLimits::new(8, 4, 20).unwrap())
    };
    let effective = build().effective_session_limits();
    assert_eq!(effective, SessionLimits::new(1, 1, 20).unwrap());
    let model = Arc::new(MockModel::new());
    let assembly = build()
        .bind(ToolSet::new())
        .unwrap()
        .model(model.clone(), "m")
        .unwrap();
    assert_eq!(assembly.session_limits(), effective);
    let card = assembly.child_sessions().expect("sessions").clone();
    assert_eq!(card.card_extension().unwrap().params["maxOpen"], 1);

    let store: DynStore = Arc::new(MemoryStore::new());
    let rt = assembly
        .register(Runtime::builder(store))
        .poll_interval(Duration::from_millis(10))
        .build();
    let start = |id: &str, agent: &str| adam_model::ToolCall {
        id: id.into(),
        name: "subagent_start".into(),
        arguments: json!({"agent": agent, "description": "d", "prompt": "go", "keep_open": true}),
        ..Default::default()
    };
    model
        .push_tool_calls(vec![start("c1", "explorer"), start("c2", "reviewer")])
        .push_text("helper")
        .push_text("helper")
        .push_text("done");
    let run = rt
        .start(&root, adam_llm_agent::user_message("go"), None)
        .await
        .unwrap();
    let worker = spawn_worker(&rt);
    common::wait_done(&rt, run).await;
    worker.stop().await;

    let requests = model.requests();
    let spec = requests
        .iter()
        .flat_map(|r| &r.tools)
        .find(|t| t.name == "subagent_start")
        .expect("the root has the session tools");
    let listed: Vec<String> = spec.parameters["properties"]["agent"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(listed, names(&card), "the tool lists the card's helpers");
    let results: Vec<(String, bool, String)> = requests
        .iter()
        .flat_map(|r| &r.messages)
        .filter_map(|m| match m {
            adam_model::Message::Tool {
                call_id,
                content,
                is_error,
                ..
            } => Some((call_id.clone(), *is_error, content.clone())),
            _ => None,
        })
        .collect();
    let refused: Vec<_> = results
        .iter()
        .filter(|(id, is_error, _)| (id == "c1" || id == "c2") && *is_error)
        .collect();
    assert_eq!(
        refused.len(),
        1,
        "one of two starts is refused: {results:?}"
    );
    assert!(
        refused[0].2.contains("1 open sessions") || refused[0].2.contains("1 subagent rounds"),
        "the refusal names the limit of one: {}",
        refused[0].2
    );
}

#[test]
fn the_starters_are_exactly_the_helpers_registered_names() {
    let root = uniq("root");
    let with_helpers = def_of(&files(&root, ""));
    let rt = with_helpers
        .session_starters(Runtime::builder(Arc::new(MemoryStore::new())))
        .build();
    assert_eq!(
        rt.agent_names(),
        [
            format!("{root}/billing"),
            format!("{root}/explorer"),
            format!("{root}/reviewer"),
        ]
    );
    // An agent without subagents registers nothing.
    let single = def(&[(
        "agent/instructions.md",
        &instructions(&format!("name: {root}"), "Alone."),
    )]);
    assert!(
        single
            .session_starters(Runtime::builder(Arc::new(MemoryStore::new())))
            .build()
            .agent_names()
            .is_empty()
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    /// Over random folders the card lists the root's subagents in file order, each registered as
    /// `<root>/<name>`, and the assembly holds what the definition computes.
    #[test]
    fn the_card_and_the_assembly_agree_over_random_folders(
        local in prop::collection::vec(0u32..=40, 0..4),
        remote in 0usize..3,
        open in 1u32..=16,
    ) {
        let root = uniq("root");
        let mut owned = vec![(
            "agent/instructions.md".to_owned(),
            instructions(&format!("name: {root}\nlimits:\n  max_open_sessions: {open}"), "Root."),
        )];
        let mut listed = Vec::new();
        for (at, minutes) in local.iter().enumerate() {
            let name = format!("l{at}");
            owned.push((
                format!("agent/subagents/{name}.md"),
                instructions(
                    &format!("description: Local {at}.\nlimits:\n  max_minutes: {minutes}"),
                    "Work.",
                ),
            ));
            listed.push(name);
        }
        for at in 0..remote {
            let name = format!("r{at}");
            owned.push((
                format!("agent/subagents/{name}.md"),
                format!("---\ndescription: Remote {at}.\na2a: {CARD}\n---\n"),
            ));
            listed.push(name);
        }
        listed.sort();
        let make = || def_of(&owned);
        let sessions = make().child_sessions();
        if listed.is_empty() {
            prop_assert!(sessions.is_none());
        } else {
            let sessions = sessions.unwrap();
            prop_assert_eq!(names(&sessions), listed);
            let card = sessions.card_extension().unwrap().params;
            prop_assert_eq!(card["maxOpen"].as_u64().unwrap(), u64::from(open.min(16)));
            let assembly = make()
                .bind(ToolSet::new())
                .unwrap()
                .model(Arc::new(MockModel::new()), "m")
                .unwrap();
            // What the session tools enforce, read from the bound root, is what the card says.
            prop_assert_eq!(
                u64::from(assembly.session_limits().max_open()),
                card["maxOpen"].as_u64().unwrap()
            );
        }
    }
}

// --- a person's session, started on a control plane and worked by a worker -----------------

fn activated(subject: &str) -> Caller {
    let mut caller = Caller::new(subject);
    caller.extensions = vec![CHILD_SESSIONS_EXTENSION.to_owned()];
    caller
}

fn said(text: &str, context: Option<&str>, metadata: Value) -> Message {
    let mut message = Message::new(Role::User, vec![Part::text(text)]);
    message.context_id = context.map(str::to_owned);
    message.metadata = Some(HashMap::from([(
        CHILD_SESSIONS_EXTENSION.to_owned(),
        metadata,
    )]));
    message
}

async fn until(backend: &RuntimeTaskBackend, caller: &Caller, id: &str, state: TaskState) -> Task {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let task = backend.get(caller, id).await.unwrap().expect("the round");
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

fn object(task: &Task) -> Value {
    task.metadata
        .as_ref()
        .and_then(|m| m.get(CHILD_SESSIONS_EXTENSION))
        .cloned()
        .unwrap_or(Value::Null)
}

/// The control plane has the root's starter and the helpers' starters and serves the front; the
/// worker has the assembly. A person starts a session on the first, the second works it.
#[tokio::test]
async fn a_session_started_on_a_control_plane_is_worked_by_a_worker() {
    for (store_name, store) in stores().await {
        let root = uniq("root");
        let owned = vec![
            (
                "agent/instructions.md".to_owned(),
                instructions(&format!("name: {root}"), "Root."),
            ),
            (
                "agent/subagents/planner.md".to_owned(),
                instructions("description: Plans a piece of work.", "You plan."),
            ),
        ];
        let model = Arc::new(MockModel::new());
        model.push_text("first plan").push_text("second plan");

        let worker_rt = def_of(&owned)
            .bind(ToolSet::new())
            .unwrap()
            .model(model.clone(), "m")
            .unwrap()
            .register(Runtime::builder(store.clone()))
            .poll_interval(Duration::from_millis(10))
            .build();
        let events = BroadcastSink::default();
        let control = def_of(&owned);
        let sessions = control.child_sessions().unwrap();
        let control_rt = control
            .session_starters(Runtime::builder(store.clone()).starter(LlmStarter::new(&root)))
            .event_sink(events.clone())
            .poll_interval(Duration::from_millis(10))
            .build();
        assert_eq!(
            control_rt.agent_names(),
            [root.clone(), format!("{root}/planner")],
            "{store_name}: starters only, no model"
        );
        let backend = RuntimeTaskBackend::new(control_rt, events, root.clone())
            .with_poll_interval(Duration::from_millis(10))
            .with_child_sessions(sessions);
        let worker = spawn_worker(&worker_rt);

        let alice = activated("alice");
        let start = backend
            .submit(
                alice.clone(),
                said(
                    "plan the move",
                    Some("ctx-1"),
                    json!({"start": {"agent": "planner", "description": "Plan it", "keepOpen": true}}),
                ),
                None,
                Some("ctx-1".into()),
            )
            .await
            .unwrap_or_else(|e| panic!("{store_name}: start refused: {e:?}"));
        assert_eq!(object(&start)["round"], 1, "{store_name}");
        let done = until(&backend, &alice, &start.id, TaskState::Completed).await;
        assert_eq!(object(&done)["state"], "idle", "{store_name}: kept open");

        let two = backend
            .submit(
                alice.clone(),
                said("and the second floor", Some(&start.context_id), json!({})),
                None,
                Some(start.context_id.clone()),
            )
            .await
            .unwrap();
        until(&backend, &alice, &two.id, TaskState::Completed).await;
        let last = model.requests().pop().expect("a model request");
        let seen = serde_json::to_string(&last.messages).unwrap();
        assert!(
            seen.contains("plan the move") && seen.contains("first plan"),
            "{store_name}: round 2 carries round 1's conversation: {seen}"
        );

        let closed = backend
            .submit(
                alice.clone(),
                said(
                    "done",
                    Some(&start.context_id),
                    json!({"close": {"cancel": false}}),
                ),
                None,
                Some(start.context_id.clone()),
            )
            .await
            .unwrap();
        until(&backend, &alice, &closed.id, TaskState::Completed).await;
        // Closed, a later message is refused.
        let later = backend
            .submit(
                alice.clone(),
                said("one more thing", Some(&start.context_id), json!({})),
                None,
                Some(start.context_id.clone()),
            )
            .await;
        assert!(
            matches!(&later, Err(adam_a2a::BackendError::UnsupportedOperation(_))),
            "{store_name}: a closed session takes no more messages: {later:?}"
        );
        worker.stop().await;
    }
}
