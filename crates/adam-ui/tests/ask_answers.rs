//! The agent reads the person's answer to a form before the model does: a whole agent behind A2A,
//! as the screen's orchestrator drives it, on the published catalogs of versions 7 and 6. An answer
//! that breaks a rule of the catalog is not used: the person is told why in a message of the same
//! task and asked again on a new surface with what they got right filled in; the model is not
//! asked again until an answer fits. Words, a form that went out as text and a form with only the
//! older types are the model's to read, as before.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use a2a::{Message, Part, PartContent, Role, Task, TaskState};
use adam_a2a::{A2UI_MEDIA_TYPE, Caller, TaskBackend, UI_CATALOG_EXTENSION};
use adam_a2a_runtime::{PAYLOAD_FORM, RuntimeTaskBackend, vymalo_inbound};
use adam_core::{DynStore, MemoryStore};
use adam_llm_agent::{AnswerCheck, LlmAgent, ToolCtx, ToolError};
use adam_mcp::McpPolicy;
use adam_model::{MockModel, ToolCall};
use adam_runtime::{BroadcastSink, NoopSink, Runtime};
use adam_ui::{Claimed, Ui};
use serde_json::{Value, json};
use tokio::sync::oneshot;

const CATALOG_ID: &str = "https://agents.vymalo.com/a2ui/catalogs/chat";
const V7: &str = include_str!("fixtures/catalog-v7.json");
const V7_LOCK: &str = include_str!("fixtures/catalog-v7.lock.json");
const V6: &str = include_str!("fixtures/catalog-v6.json");
const V6_LOCK: &str = include_str!("fixtures/catalog-v6.lock.json");
const VECTORS: &str = include_str!("fixtures/choices-v7-vectors.json");

struct Screen {
    document: Value,
    claimed: Claimed,
}

fn screen(document: &str, lock: &str) -> Screen {
    let lock: Value = serde_json::from_str(lock).unwrap();
    Screen {
        document: serde_json::from_str(document).unwrap(),
        claimed: Claimed {
            catalog_id: CATALOG_ID.to_owned(),
            version: u32::try_from(lock["version"].as_u64().unwrap()).unwrap(),
            digest: lock["digest"].as_str().unwrap().to_owned(),
        },
    }
}

fn as_doubles(value: &Value) -> Value {
    match value {
        Value::Number(n) if n.is_i64() || n.is_u64() => json!(n.as_f64().unwrap()),
        Value::Array(items) => Value::Array(items.iter().map(as_doubles).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), as_doubles(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

struct Rig {
    runtime: Runtime,
    backend: RuntimeTaskBackend,
    model: Arc<MockModel>,
}

impl Rig {
    fn new() -> Self {
        let store: DynStore = Arc::new(MemoryStore::new());
        let model = Arc::new(MockModel::new());
        let ui = Ui::new(McpPolicy::default());
        let agent = LlmAgent::builder("asker", model.clone(), "m")
            .instructions("You are a test agent.")
            .tools(ui.tools())
            .tool_source(ui.source())
            .build();
        let events = BroadcastSink::default();
        let runtime = Runtime::builder(store)
            .agent(agent)
            .event_sink(events.clone())
            .poll_interval(Duration::from_millis(10))
            .build();
        let backend = RuntimeTaskBackend::new(runtime.clone(), events, "asker")
            .with_poll_interval(Duration::from_millis(10))
            .with_inbound(vymalo_inbound);
        Self {
            runtime,
            backend,
            model,
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

    /// The task once `done` holds of it.
    async fn until(&self, task: &Task, what: &str, done: impl Fn(&Task) -> bool) -> Task {
        for _ in 0..1000 {
            let got = self.backend.get(&alice(), &task.id).await.unwrap().unwrap();
            if done(&got) {
                return got;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("task {} never reached {what}", task.id);
    }

    async fn asking(&self, task: &Task, first_words: &str) -> Task {
        self.until(task, first_words, |t| {
            t.status.state == TaskState::InputRequired
                && t.status
                    .message
                    .as_ref()
                    .and_then(Message::text)
                    .is_some_and(|text| text.starts_with(first_words))
        })
        .await
    }

    async fn send(&self, task: &Task, message: Message) {
        self.backend
            .submit(alice(), message, Some(task.id.clone()), Some("ctx".into()))
            .await
            .unwrap();
    }
}

fn alice() -> Caller {
    Caller::new("token-0")
}

fn from_the_screen(text: &str, screen: &Screen) -> Message {
    let c = &screen.claimed;
    let metadata = json!({
        UI_CATALOG_EXTENSION: {
            "catalogId": c.catalog_id, "version": f64::from(c.version), "digest": c.digest,
            "inline": true},
        "a2uiClientCapabilities": {"v0.9.1": {
            "supportedCatalogIds": [CATALOG_ID],
            "inlineCatalogs": [as_doubles(&screen.document)]}},
    });
    let mut message = Message::new(Role::User, vec![Part::text(text)]);
    message.metadata = Some(
        metadata
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    );
    message
}

fn action(surface: &str, answers: Value) -> Message {
    let mut part = Part::data(json!([{"version": "v0.9.1", "action": {
        "name": "answer", "surfaceId": surface, "sourceComponentId": "root",
        "timestamp": "2026-10-01T10:00:00Z", "context": {"answers": answers}}}]))
    .with_media_type(A2UI_MEDIA_TYPE);
    part.metadata = Some(HashMap::from([(
        "mimeType".to_owned(),
        json!(A2UI_MEDIA_TYPE),
    )]));
    Message::new(Role::User, vec![part])
}

/// The five questions of the contract's own examples, asked by one call.
fn five(call_id: &str) -> ToolCall {
    let vectors: Value = serde_json::from_str(VECTORS).unwrap();
    ToolCall {
        id: call_id.into(),
        name: "ask_user".into(),
        arguments: json!({
            "question": "Before I ship it.",
            "choices": vectors["questions"],
        }),
        ..Default::default()
    }
}

fn good() -> Value {
    json!([
        {"id": "ship", "values": ["yes"]},
        {"id": "budget", "values": ["250.5"]},
        {"id": "nps", "values": ["8"]},
        {"id": "when", "values": ["2026-12-15"]},
        {"id": "order", "values": ["risk", "speed", "cost"]}
    ])
}

/// `good()` with the answer to `id` replaced.
fn with(id: &str, values: Value) -> Value {
    let mut answers = good();
    for entry in answers.as_array_mut().unwrap() {
        if entry["id"] == id {
            entry["values"] = values.clone();
        }
    }
    answers
}

fn interface(task: &Task) -> Value {
    let status = task.status.message.as_ref().expect("a status message");
    let PartContent::Data(ui) = &status.parts[1].content else {
        panic!("the second part is the interface: {status:?}");
    };
    ui.clone()
}

fn root(ui: &Value) -> &Value {
    &ui[1]["updateComponents"]["components"][0]
}

#[tokio::test]
async fn a_refused_answer_is_told_to_the_person_asked_again_with_what_was_right_and_never_reaches_the_model()
 {
    let rig = Rig::new();
    rig.model
        .push_tool_calls(vec![five("q-call")])
        .push_text("Shipping on the 15th.");
    let (stop, handle) = rig.worker();
    let task = rig
        .backend
        .submit(
            alice(),
            from_the_screen("ship it", &screen(V7, V7_LOCK)),
            None,
            Some("ctx".into()),
        )
        .await
        .unwrap();
    let asked = rig.asking(&task, "Before I ship it.").await;
    let first = interface(&asked);
    assert_eq!(first[0]["createSurface"]["surfaceId"], "ask-q-call");

    // A budget above its most, with everything else right.
    rig.send(&task, action("ask-q-call", with("budget", json!(["1500"]))))
        .await;
    let again = rig.asking(&task, "The answer to \"budget\"").await;
    assert_eq!(
        again.status.message.as_ref().and_then(Message::text),
        Some(
            "The answer to \"budget\" (\"What is the monthly budget?\") was not used (above_max): \
             1500 is above the most, 1000 EUR.\nBefore I ship it."
        )
    );
    let second = interface(&again);
    assert_eq!(second[0]["createSurface"]["surfaceId"], "ask-q-call.r1");
    assert_eq!(second[1]["updateComponents"]["surfaceId"], "ask-q-call.r1");
    assert_eq!(second[0]["createSurface"]["catalogId"], CATALOG_ID);
    let questions = root(&second)["questions"].as_array().unwrap();
    // What was right is where the person left it; what was wrong starts where the model put it.
    assert_eq!(questions[0]["default"], "yes");
    assert_eq!(questions[1]["default"], "250");
    assert_eq!(questions[2]["default"], "8");
    assert_eq!(questions[3]["default"], "2026-12-15");
    assert_eq!(questions[4]["default"], json!(["risk", "speed", "cost"]));
    // Nothing else of the form changed.
    let mut before = root(&first).clone();
    let mut after = root(&second).clone();
    for form in [&mut before, &mut after] {
        for q in form["questions"].as_array_mut().unwrap() {
            q.as_object_mut().unwrap().remove("default");
        }
    }
    assert_eq!(before, after);
    // The model was not woken and has no result for the call.
    assert_eq!(rig.model.requests().len(), 1);

    // A second refusal is `.r2`, and an answer that fits is the call's result.
    rig.send(
        &task,
        action("ask-q-call.r1", with("order", json!(["risk", "speed"]))),
    )
    .await;
    let third = rig.asking(&task, "The answer to \"order\"").await;
    assert!(
        third
            .status
            .message
            .as_ref()
            .and_then(Message::text)
            .is_some_and(|t| t.contains("(not_a_permutation)")),
    );
    assert_eq!(
        interface(&third)[0]["createSurface"]["surfaceId"],
        "ask-q-call.r2"
    );
    rig.send(&task, action("ask-q-call.r2", good())).await;
    let done = rig
        .until(&task, "completed", |t| {
            t.status.state == TaskState::Completed
        })
        .await;
    stop.send(()).unwrap();
    handle.await.unwrap();
    assert_eq!(
        done.status.message.as_ref().and_then(Message::text),
        Some("Shipping on the 15th.")
    );
    let requests = rig.model.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].messages.last(),
        Some(&adam_model::Message::tool_result(
            "q-call",
            "The person answered through the interface (form ask-q-call.r2):\n\
             - ship: yes\n- budget: 250.5\n- nps: 8\n- when: 2026-12-15\n- order: risk, speed, cost"
        ))
    );
    // Nothing of the refused answers is in what the model read.
    let history = format!("{:?}", requests[1].messages);
    assert!(!history.contains("1500"), "{history}");
}

#[tokio::test]
async fn words_are_the_answer_and_the_model_reads_them_unchecked() {
    let rig = Rig::new();
    rig.model
        .push_tool_calls(vec![five("q-call")])
        .push_text("Understood.");
    let (stop, handle) = rig.worker();
    let task = rig
        .backend
        .submit(
            alice(),
            from_the_screen("ship it", &screen(V7, V7_LOCK)),
            None,
            Some("ctx".into()),
        )
        .await
        .unwrap();
    rig.asking(&task, "Before I ship it.").await;
    rig.send(
        &task,
        Message::new(Role::User, vec![Part::text("ship it, whatever the budget")]),
    )
    .await;
    rig.until(&task, "completed", |t| {
        t.status.state == TaskState::Completed
    })
    .await;
    stop.send(()).unwrap();
    handle.await.unwrap();
    assert_eq!(
        rig.model.requests()[1].messages.last(),
        Some(&adam_model::Message::tool_result(
            "q-call",
            "ship it, whatever the budget"
        ))
    );
}

#[tokio::test]
async fn a_screen_of_version_6_is_asked_the_nearest_form_and_its_answer_is_judged_by_the_same_rules()
 {
    let rig = Rig::new();
    rig.model
        .push_tool_calls(vec![five("q-call")])
        .push_text("Done.");
    let (stop, handle) = rig.worker();
    let task = rig
        .backend
        .submit(
            alice(),
            from_the_screen("ship it", &screen(V6, V6_LOCK)),
            None,
            Some("ctx".into()),
        )
        .await
        .unwrap();
    let asked = rig.asking(&task, "Before I ship it.").await;
    let drawn = root(&interface(&asked))["questions"].clone();
    assert_eq!(drawn[0]["type"], "single");
    // A yes or no and a scale come back as the radio button's value, and are judged as such.
    let older = json!([
        {"id": "ship", "values": ["maybe"]},
        {"id": "budget", "values": [], "text": "250.5"},
        {"id": "nps", "values": ["8"]},
        {"id": "when", "values": [], "text": "2026-12-15"},
        {"id": "order", "values": [], "text": "3, 1, 2"}
    ]);
    rig.send(&task, action("ask-q-call", older)).await;
    let again = rig.asking(&task, "The answer to \"ship\"").await;
    assert!(
        again
            .status
            .message
            .as_ref()
            .and_then(Message::text)
            .is_some_and(|t| t.contains("(not_offered)")),
    );
    let questions = root(&interface(&again))["questions"].clone();
    // The scale was right and keeps its point; the typed words are the person's own and are kept
    // as the words the boxes start with; the refused yes or no starts where the model put it.
    assert_eq!(questions[2]["default"], "8");
    assert_eq!(questions[1]["default"], "250.5");
    assert_eq!(questions[3]["default"], "2026-12-15");
    assert_eq!(questions[0]["default"], "no");
    // The typed kinds the screen lacked are the model's to read, as the older types are.
    let fixed = json!([
        {"id": "ship", "values": ["yes"]},
        {"id": "budget", "values": [], "text": "a bit over a thousand"},
        {"id": "nps", "values": ["8"]},
        {"id": "when", "values": [], "text": "soon"},
        {"id": "order", "values": [], "text": "3, 1, 2"}
    ]);
    rig.send(&task, action("ask-q-call.r1", fixed)).await;
    rig.until(&task, "completed", |t| {
        t.status.state == TaskState::Completed
    })
    .await;
    stop.send(()).unwrap();
    handle.await.unwrap();
    assert_eq!(rig.model.requests().len(), 2);
}

// ---- the check by itself ----

/// The call's interface, and the tool.
async fn asked_with_v7() -> (adam_llm_agent::DynTool, Value, Value) {
    let ui = Ui::new(McpPolicy::default());
    let tool = ui.tools().get("ask_user").cloned().unwrap();
    let call = five("c1");
    let screen = screen(V7, V7_LOCK);
    let c = &screen.claimed;
    let context = json!({
        "vymalo.ui.ref": {"catalogId": c.catalog_id, "version": c.version, "digest": c.digest},
        "vymalo.ui.catalog": {
            "catalogId": c.catalog_id, "version": c.version, "digest": c.digest,
            "catalog": as_doubles(&screen.document)},
    })
    .as_object()
    .cloned()
    .unwrap();
    let Err(ToolError::NeedsInput { ui: Some(ui), .. }) = tool
        .call(
            &ToolCtx::detached("ask_user", "c1", Arc::new(NoopSink)).with_context(context),
            call.arguments.clone(),
        )
        .await
    else {
        panic!("the call did not ask with a form");
    };
    (tool, call.arguments, ui)
}

fn payload(surface: &str, context: Value) -> Value {
    json!({"text": "The person answered", PAYLOAD_FORM: {"surface": surface, "context": context}})
}

#[tokio::test]
async fn each_rule_of_the_catalog_is_the_reason_an_answer_is_refused() {
    let (tool, args, ui) = asked_with_v7().await;
    let rule_of = |context: Value| match tool.read_answer(
        "c1",
        &args,
        Some(&ui),
        &payload("ask-c1", context),
    ) {
        AnswerCheck::Accept => None,
        AnswerCheck::Refuse(refusal) => Some((refusal.subject.unwrap_or_default(), refusal.rule)),
        _ => unreachable!("only two answers"),
    };
    let ctx = |answers: Value| json!({"answers": answers});
    let want = |question: &str, rule: &str| Some((question.to_owned(), rule.to_owned()));
    assert_eq!(rule_of(ctx(good())), None);
    assert_eq!(rule_of(json!({})), want("", "unknown"));
    assert_eq!(
        rule_of(ctx(json!([{"id": "ship", "values": ["yes"]}]))),
        want("", "unknown")
    );
    assert_eq!(
        rule_of(json!({"answers": good(), "note": "hi"})),
        want("", "unexpected")
    );
    let mut with_other = good();
    with_other[0]["other"] = json!("");
    assert_eq!(rule_of(ctx(with_other)), want("ship", "unexpected"));
    assert_eq!(
        rule_of(ctx(with("order", json!([])))),
        want("order", "required")
    );
    assert_eq!(
        rule_of(ctx(with("ship", json!(["yes", "no"])))),
        want("ship", "too_many")
    );
    assert_eq!(
        rule_of(ctx(with("ship", json!(["maybe"])))),
        want("ship", "not_offered")
    );
    assert_eq!(
        rule_of(ctx(with("budget", json!(["1e3"])))),
        want("budget", "grammar")
    );
    assert_eq!(
        rule_of(ctx(with("budget", json!([250.5])))),
        want("budget", "grammar")
    );
    assert_eq!(
        rule_of(ctx(with("budget", json!(["-1"])))),
        want("budget", "below_min")
    );
    assert_eq!(
        rule_of(ctx(with("budget", json!(["1000.5"])))),
        want("budget", "above_max")
    );
    assert_eq!(
        rule_of(ctx(with("budget", json!(["250.25"])))),
        want("budget", "off_step")
    );
    assert_eq!(
        rule_of(ctx(with("when", json!(["2027-01-01"])))),
        want("when", "above_max")
    );
    assert_eq!(
        rule_of(ctx(with("order", json!(["risk", "risk", "cost"])))),
        want("order", "not_a_permutation")
    );
    // The optional scale may be left alone.
    assert_eq!(rule_of(ctx(with("nps", json!([])))), None);
}

#[tokio::test]
async fn the_same_answer_is_refused_the_same_way_every_time() {
    let (tool, args, ui) = asked_with_v7().await;
    let bad = payload(
        "ask-c1",
        json!({"answers": with("budget", json!(["1500"]))}),
    );
    let first = tool.read_answer("c1", &args, Some(&ui), &bad);
    let second = tool.read_answer("c1", &args, Some(&ui), &bad);
    assert_eq!(first, second);
    assert!(matches!(first, AnswerCheck::Refuse(_)));
    // Asked again, from the form it was shown, it gives the next attempt.
    let AnswerCheck::Refuse(refusal) = first else {
        unreachable!()
    };
    let shown = refusal.ui.unwrap();
    let again = tool.read_answer("c1", &args, Some(&shown), &bad);
    let AnswerCheck::Refuse(again) = again else {
        panic!("refused again")
    };
    assert_eq!(
        again.ui.unwrap()[0]["createSurface"]["surfaceId"],
        "ask-c1.r2"
    );
}

#[tokio::test]
async fn a_form_with_no_answer_to_judge_is_accepted_as_it_came() {
    let (tool, args, ui) = asked_with_v7().await;
    // No `form` in the payload: words.
    assert_eq!(
        tool.read_answer("c1", &args, Some(&ui), &json!({"text": "anything"})),
        AnswerCheck::Accept
    );
    // A question that went out as text had no interface.
    assert_eq!(
        tool.read_answer(
            "c1",
            &args,
            None,
            &payload("ask-c1", json!({"answers": []}))
        ),
        AnswerCheck::Accept
    );
    // A call whose arguments cannot be read again is not judged.
    assert_eq!(
        tool.read_answer(
            "c1",
            &json!({"question": 3}),
            Some(&ui),
            &payload("ask-c1", json!({"answers": []}))
        ),
        AnswerCheck::Accept
    );
}
