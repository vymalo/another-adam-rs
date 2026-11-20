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
    // The published catalogs of versions 2 to 8 have no `protocolVersion`; the screen sends them as
    // `ui-catalog/v2` documents (the digest is of what is sent).
    let mut document: Value = serde_json::from_str(document).unwrap();
    document["protocolVersion"] = json!("1.0");
    Screen {
        claimed: Claimed {
            catalog_id: CATALOG_ID.to_owned(),
            version: u32::try_from(lock["version"].as_u64().unwrap()).unwrap(),
            digest: adam_ui::catalog_digest(&document).unwrap(),
        },
        document,
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
        "a2uiRendererCapabilities": {"1.0": {
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
    let mut part = Part::data(json!([{"version": "1.0", "action": {
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
    assert_drawable(&screen(V7, V7_LOCK), &second);
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
    // A number, a day and a rank that the screen drew as text boxes are judged as the kinds they
    // stand for: words that are no number are refused like a number that is no number.
    let words = json!([
        {"id": "ship", "values": ["yes"]},
        {"id": "budget", "values": [], "text": "a bit over a thousand"},
        {"id": "nps", "values": ["8"]},
        {"id": "when", "values": [], "text": "soon"},
        {"id": "order", "values": [], "text": "3, 1, 2"}
    ]);
    rig.send(&task, action("ask-q-call.r1", words)).await;
    let third = rig.asking(&task, "The answer to \"budget\"").await;
    assert!(
        third
            .status
            .message
            .as_ref()
            .and_then(Message::text)
            .is_some_and(|t| t.contains("(grammar)")),
    );
    assert_eq!(
        interface(&third)[0]["createSurface"]["surfaceId"],
        "ask-q-call.r2"
    );
    let questions = root(&interface(&third))["questions"].clone();
    // What fit is kept; the words that did not are not carried, and the form starts those boxes
    // where the last form that was shown had them.
    assert_eq!(questions[0]["default"], "yes");
    assert_eq!(questions[4]["default"], "3, 1, 2");
    assert_eq!(questions[1]["default"], "250.5");
    assert_eq!(questions[3]["default"], "2026-12-15");
    // The model has not been asked again.
    assert_eq!(rig.model.requests().len(), 1);
    let fixed = json!([
        {"id": "ship", "values": ["yes"]},
        {"id": "budget", "values": [], "text": "250.5"},
        {"id": "nps", "values": ["8"]},
        {"id": "when", "values": [], "text": "2026-12-15"},
        {"id": "order", "values": [], "text": "3, 1, 2"}
    ]);
    rig.send(&task, action("ask-q-call.r2", fixed)).await;
    rig.until(&task, "completed", |t| {
        t.status.state == TaskState::Completed
    })
    .await;
    stop.send(()).unwrap();
    handle.await.unwrap();
    assert_eq!(rig.model.requests().len(), 2);
}

// ---- the check by itself ----

/// The tool, the call's arguments, and the interface of the call `c1` on `screen`.
async fn asked_on(screen: &Screen, args: Value) -> (adam_llm_agent::DynTool, Value, Value) {
    let ui = Ui::new(McpPolicy::default());
    let tool = ui.tools().get("ask_user").cloned().unwrap();
    let c = &screen.claimed;
    let context = json!({
        "vymalo.ui.ref": {"catalogId": c.catalog_id, "version": c.version, "digest": c.digest},
        "vymalo.a2ui.renderer": {"catalogIds": [c.catalog_id]},
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
            args.clone(),
        )
        .await
    else {
        panic!("the call did not ask with a form");
    };
    (tool, args, ui)
}

/// The call's interface, and the tool.
async fn asked_with_v7() -> (adam_llm_agent::DynTool, Value, Value) {
    asked_on(&screen(V7, V7_LOCK), five("c1").arguments).await
}

/// `ui` is a form the screen's own catalog draws: every refusal asks one that is valid again.
fn assert_drawable(screen: &Screen, ui: &Value) {
    let catalog = adam_ui::Catalog::from_document(screen.document.clone(), &screen.claimed)
        .expect("the screen's catalog");
    catalog
        .validate(root(ui))
        .unwrap_or_else(|problem| panic!("the form asked again is not drawable: {problem}\n{ui}"));
}

/// What the check says about `context` as the answer to the form on `surface`: the rule and the
/// question it was about (`*` for the whole answer), or `None` when it is accepted.
fn verdict(
    tool: &adam_llm_agent::DynTool,
    args: &Value,
    ui: &Value,
    surface: &str,
    context: Value,
) -> Option<(String, String)> {
    match tool.read_answer("c1", args, Some(ui), &payload(surface, context)) {
        AnswerCheck::Accept => None,
        AnswerCheck::Refuse(r) | AnswerCheck::End(r) => {
            Some((r.subject.unwrap_or_else(|| "*".into()), r.rule))
        }
        other => panic!("unexpected {other:?}"),
    }
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

/// A form is asked again on a new surface only when every operation of the interface it copies says the
/// version adam writes. One from before the cutover is judged the same way, but the question comes back in
/// words: its operations are never copied into a new surface or edited into the current version.
#[tokio::test]
async fn a_form_from_before_the_cutover_is_asked_again_in_words_not_on_a_new_surface() {
    let (tool, args, ui) = asked_with_v7().await;
    let bad = payload(
        "ask-c1",
        json!({"answers": with("budget", json!(["1500"]))}),
    );
    let current = |ui: &Value| match tool.read_answer("c1", &args, Some(ui), &bad) {
        AnswerCheck::Refuse(refusal) => refusal,
        other => panic!("refused: {other:?}"),
    };
    // As it was drawn: every operation says "1.0" and the question comes back on `.r1`.
    assert!(current(&ui).ui.is_some());
    // One operation that does not say "1.0", whatever it says instead, and then all of them.
    for (what, changed) in [
        ("the pre-cutover version", json!("v0.9.1")),
        ("0.9", json!("0.9")),
        ("a number", json!(1)),
        ("none", Value::Null),
    ] {
        let mut old = ui.clone();
        old[1]["version"] = changed;
        let words = current(&old);
        assert!(words.ui.is_none(), "{what}: not copied");
        assert_eq!(words.rule, "above_max", "{what}: judged as before");
        assert!(words.question.contains("1500 is above the most"), "{what}");
    }
    let mut every = ui.clone();
    for operation in every.as_array_mut().unwrap() {
        operation["version"] = json!("v0.9.1");
    }
    assert!(current(&every).ui.is_none());
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

// ---- the answer belongs to its form ----

#[tokio::test]
async fn an_answer_for_another_form_is_not_this_questions_even_when_its_ids_and_values_fit() {
    // Two calls that both ask a `confirm` with the default id `q1`: a stale click on the first form
    // must not stand for the consent the second one asks.
    let screen = screen(V7, V7_LOCK);
    let args =
        json!({"question": "Delete it?", "choices": [{"type": "confirm", "question": "Sure?"}]});
    let (tool, args, ui) = asked_on(&screen, args).await;
    let yes = json!({"answers": [{"id": "q1", "values": ["yes"]}]});
    assert_eq!(verdict(&tool, &args, &ui, "ask-c1", yes.clone()), None);
    let other = verdict(&tool, &args, &ui, "ask-c0", yes.clone());
    assert_eq!(other, Some(("*".into(), "other_surface".into())));
    // The exact surface decides, not one that merely names the call: a form that has not been asked
    // yet is no more this question's than another call's.
    for surface in ["ask-c1.r1", "ask-c1.r3", "ask-c10", "ask-c", "ask-c1x.r1"] {
        assert_eq!(
            verdict(&tool, &args, &ui, surface, yes.clone()),
            Some(("*".into(), "other_surface".into())),
            "{surface}"
        );
    }
    // The question is asked again, as itself, and nothing of the stray answer is carried.
    let AnswerCheck::Refuse(refusal) =
        tool.read_answer("c1", &args, Some(&ui), &payload("ask-c0", yes))
    else {
        panic!("refused")
    };
    assert!(
        refusal.question.starts_with(
            "The answers were not used (other_surface): the answer is for another form than the one asked.\n"
        ),
        "{}",
        refusal.question
    );
    let again = refusal.ui.unwrap();
    assert_eq!(again[0]["createSurface"]["surfaceId"], "ask-c1.r1");
    assert!(root(&again)["questions"][0].get("default").is_none());
    assert_drawable(&screen, &again);
    // Asked again, only that form's answer is the answer.
    assert_eq!(
        verdict(
            &tool,
            &args,
            &again,
            "ask-c1.r1",
            json!({"answers": [{"id": "q1", "values": ["yes"]}]})
        ),
        None
    );
    assert_eq!(
        verdict(
            &tool,
            &args,
            &again,
            "ask-c1",
            json!({"answers": [{"id": "q1", "values": ["yes"]}]})
        ),
        Some(("*".into(), "other_surface".into()))
    );
}

#[tokio::test]
async fn a_form_of_the_older_types_is_tied_to_its_surface_and_accepts_any_answer_on_it() {
    let args = json!({"question": "Which?", "choices": [
        {"id": "db", "question": "Database?", "options": ["Postgres", "SQLite"]}]});
    let (tool, args, ui) = asked_on(&screen(V7, V7_LOCK), args).await;
    // Its answers are not covered by the contract: whatever its shape, on its own surface.
    for context in [
        json!({}),
        json!({"answers": []}),
        json!({"answers": 3, "x": 1}),
        json!(null),
    ] {
        assert_eq!(
            verdict(&tool, &args, &ui, "ask-c1", context.clone()),
            None,
            "{context}"
        );
    }
    assert_eq!(
        verdict(&tool, &args, &ui, "ask-other", json!({})),
        Some(("*".into(), "other_surface".into()))
    );
}

#[tokio::test]
async fn a_message_that_holds_several_form_answers_is_refused_whole() {
    let (tool, args, ui) = asked_with_v7().await;
    let mut message = payload("ask-c1", json!({"answers": good()}));
    assert_eq!(
        tool.read_answer("c1", &args, Some(&ui), &message),
        AnswerCheck::Accept
    );
    message[PAYLOAD_FORM]["actions"] = json!(1);
    assert_eq!(
        tool.read_answer("c1", &args, Some(&ui), &message),
        AnswerCheck::Accept,
        "one is one"
    );
    message[PAYLOAD_FORM]["actions"] = json!(2);
    let AnswerCheck::Refuse(refusal) = tool.read_answer("c1", &args, Some(&ui), &message) else {
        panic!("a message with two answers was read")
    };
    assert_eq!(refusal.rule, "several_answers");
    assert_eq!(refusal.subject, None);
    assert!(
        refusal
            .question
            .contains("the message holds more than one answer to a form"),
        "{}",
        refusal.question
    );
}

#[tokio::test]
async fn two_form_actions_in_one_message_are_asked_again_and_never_half_checked() {
    let rig = Rig::new();
    rig.model
        .push_tool_calls(vec![five("q-call")])
        .push_text("Done.");
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
    // The first action is right, the second is the same form with another budget.
    let both = {
        let mut first = action("ask-q-call", good());
        let second = action("ask-q-call", with("budget", json!(["1500"])));
        first.parts.extend(second.parts);
        first
    };
    rig.send(&task, both).await;
    let again = rig
        .asking(&task, "The answers were not used (several_answers)")
        .await;
    assert_eq!(
        interface(&again)[0]["createSurface"]["surfaceId"],
        "ask-q-call.r1"
    );
    assert_eq!(rig.model.requests().len(), 1, "the model was not asked");
    rig.send(&task, action("ask-q-call.r1", good())).await;
    rig.until(&task, "completed", |t| {
        t.status.state == TaskState::Completed
    })
    .await;
    stop.send(()).unwrap();
    handle.await.unwrap();
}

// ---- the number of times a form is asked again ----

#[tokio::test]
async fn a_form_is_asked_again_five_times_and_the_sixth_bad_answer_ends_the_question() {
    let (tool, args, mut ui) = asked_with_v7().await;
    let bad = |surface: &str| payload(surface, json!({"answers": with("budget", json!(["1500"]))}));
    for attempt in 1..=5 {
        let surface = ui[0]["createSurface"]["surfaceId"]
            .as_str()
            .unwrap()
            .to_owned();
        let AnswerCheck::Refuse(refusal) = tool.read_answer("c1", &args, Some(&ui), &bad(&surface))
        else {
            panic!("the answer was not refused again at try {attempt}");
        };
        ui = refusal.ui.unwrap();
        assert_eq!(
            ui[0]["createSurface"]["surfaceId"],
            format!("ask-c1.r{attempt}")
        );
    }
    // Five times asked again; the next bad answer is the sixth.
    let AnswerCheck::End(ending) = tool.read_answer("c1", &args, Some(&ui), &bad("ask-c1.r5"))
    else {
        panic!("the sixth bad answer did not end the question");
    };
    assert_eq!(ending.rule, "above_max");
    assert_eq!(ending.subject.as_deref(), Some("budget"));
    assert!(ending.ui.is_none());
    assert_eq!(
        ending.question,
        "The person's answers did not fit the form after 5 tries, so it is not asked again. \
         The last one broke the rule `above_max` for question \"budget\" (\"What is the monthly \
         budget?\"): 1500 is above the most, 1000 EUR. Ask another way: in plain words, with a \
         simpler form or fewer questions."
    );
    // A whole answer that does not fit ends it as well, and says so.
    let AnswerCheck::End(whole) =
        tool.read_answer("c1", &args, Some(&ui), &payload("ask-c1.r5", json!({})))
    else {
        panic!("a whole answer that does not fit did not end the question")
    };
    assert!(
        whole.question.contains("for the whole answer: "),
        "{}",
        whole.question
    );
}

#[tokio::test]
async fn the_sixth_bad_answer_gives_the_model_an_error_and_the_run_goes_on() {
    let rig = Rig::new();
    rig.model
        .push_tool_calls(vec![five("q-call")])
        .push_text("I will ask in words instead.");
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
    let mut surface = "ask-q-call".to_owned();
    for attempt in 1..=5 {
        rig.send(&task, action(&surface, with("budget", json!(["1500"]))))
            .await;
        let again = rig.asking(&task, "The answer to \"budget\"").await;
        surface = format!("ask-q-call.r{attempt}");
        assert_eq!(
            interface(&again)[0]["createSurface"]["surfaceId"],
            surface.as_str()
        );
        assert_eq!(rig.model.requests().len(), 1);
    }
    rig.send(&task, action(&surface, with("budget", json!(["1500"]))))
        .await;
    let done = rig
        .until(&task, "completed", |t| {
            t.status.state == TaskState::Completed
        })
        .await;
    stop.send(()).unwrap();
    handle.await.unwrap();
    assert_eq!(
        done.status.message.as_ref().and_then(Message::text),
        Some("I will ask in words instead.")
    );
    let requests = rig.model.requests();
    assert_eq!(requests.len(), 2);
    let Some(adam_model::Message::Tool {
        call_id,
        content,
        is_error,
        ..
    }) = requests[1].messages.last()
    else {
        panic!(
            "the model was not given a tool result: {:?}",
            requests[1].messages
        );
    };
    assert_eq!(call_id, "q-call");
    assert!(is_error, "the ending is an error result");
    assert!(
        content.contains("did not fit the form after 5 tries")
            && content.contains("`above_max`")
            && content.contains("\"budget\""),
        "{content}"
    );
    assert!(
        !format!("{:?}", requests[1].messages).contains("1500\"]"),
        "no refused answer"
    );
}

// ---- a screen that has no control for a kind ----

#[tokio::test]
async fn what_is_typed_for_a_number_a_day_or_a_rank_is_judged_as_the_kind() {
    let screen = screen(V6, V6_LOCK);
    let (tool, args, ui) = asked_on(&screen, five("c1").arguments).await;
    let answer = |text: &str, id: &str| {
        let mut answers = json!([
            {"id": "ship", "values": ["yes"]},
            {"id": "budget", "values": [], "text": "250.5"},
            {"id": "nps", "values": ["8"]},
            {"id": "when", "values": [], "text": "2026-12-15"},
            {"id": "order", "values": [], "text": "3, 1, 2"}
        ]);
        for entry in answers.as_array_mut().unwrap() {
            if entry["id"] == id {
                entry["text"] = json!(text);
            }
        }
        json!({"answers": answers})
    };
    let check = |text: &str, id: &str| verdict(&tool, &args, &ui, "ask-c1", answer(text, id));
    let is = |id: &str, rule: &str| Some((id.to_owned(), rule.to_owned()));
    assert_eq!(check("250.5", "budget"), None);
    assert_eq!(
        check("a bit over a thousand", "budget"),
        is("budget", "grammar")
    );
    assert_eq!(check("1000.5", "budget"), is("budget", "above_max"));
    assert_eq!(check("250.25", "budget"), is("budget", "off_step"));
    assert_eq!(check("1,5", "budget"), is("budget", "grammar"));
    assert_eq!(check("2026-12-15", "when"), None);
    assert_eq!(check("soon", "when"), is("when", "grammar"));
    assert_eq!(check("2027-01-01", "when"), is("when", "above_max"));
    assert_eq!(check("2026-02-30", "when"), is("when", "grammar"));
    // A rank is typed as the numbers of the options, in order.
    for good in ["3, 1, 2", "3 1 2", "3,1,2", "2,3,1", " 1  2 3"] {
        assert_eq!(check(good, "order"), None, "{good}");
    }
    assert_eq!(check("1, 1, 2", "order"), is("order", "not_a_permutation"));
    assert_eq!(check("1, 2", "order"), is("order", "not_a_permutation"));
    assert_eq!(check("1 2 3 3", "order"), is("order", "not_a_permutation"));
    assert_eq!(check("1 2 4", "order"), is("order", "not_offered"));
    assert_eq!(check("0 1 2", "order"), is("order", "not_offered"));
    assert_eq!(check("01 2 3", "order"), is("order", "not_offered"));
    assert_eq!(
        check("risk speed cost", "order"),
        is("order", "not_offered")
    );
    // Nothing typed is no answer, and a required question needs one.
    let mut none = answer("x", "budget");
    none["answers"][1] = json!({"id": "budget", "values": []});
    assert_eq!(
        verdict(&tool, &args, &ui, "ask-c1", none),
        is("budget", "required")
    );
    // Typed words beside values are an entry no question of the five takes.
    let mut both = answer("250.5", "budget");
    both["answers"][1]["values"] = json!(["250.5"]);
    assert_eq!(
        verdict(&tool, &args, &ui, "ask-c1", both),
        is("budget", "unexpected")
    );
}

#[tokio::test]
async fn a_scale_asked_as_radio_buttons_is_judged_by_its_points() {
    let screen = screen(V6, V6_LOCK);
    let (tool, args, ui) = asked_on(&screen, five("c1").arguments).await;
    let nps = |point: &str| {
        verdict(
            &tool,
            &args,
            &ui,
            "ask-c1",
            json!({"answers": [
                {"id": "ship", "values": ["yes"]},
                {"id": "budget", "values": [], "text": "250.5"},
                {"id": "nps", "values": [point]},
                {"id": "when", "values": [], "text": "2026-12-15"},
                {"id": "order", "values": [], "text": "3, 1, 2"}]}),
        )
    };
    assert_eq!(nps("8"), None);
    assert_eq!(nps("99"), Some(("nps".into(), "above_max".into())));
    assert_eq!(nps("seven"), Some(("nps".into(), "grammar".into())));
}

// ---- a refusal keeps the whole of the form ----

#[tokio::test]
async fn what_is_carried_into_the_form_asked_again_is_something_that_form_accepts() {
    let screen = screen(V7, V7_LOCK);
    let args = json!({"question": "Set up", "choices": [
        {"id": "pick", "question": "Pick up to two", "type": "multiple", "max": 2,
         "options": ["a", "b", "c"]},
        {"id": "one", "question": "Which one?", "options": ["a", "b"]},
        {"id": "name", "question": "Name?", "type": "text"},
        {"id": "story", "question": "Story?", "type": "longText"},
        {"id": "n", "question": "How many?", "type": "number", "maxValue": "10"}]});
    let (tool, args, ui) = asked_on(&screen, args).await;
    let refuse = |answers: Value| {
        let AnswerCheck::Refuse(refusal) = tool.read_answer(
            "c1",
            &args,
            Some(&ui),
            &payload("ask-c1", json!({"answers": answers})),
        ) else {
            panic!("not refused")
        };
        refusal.ui.unwrap()
    };
    let defaults = |ui: &Value| -> Vec<Value> {
        root(ui)["questions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|q| q.get("default").cloned().unwrap_or(Value::Null))
            .collect()
    };
    let answers = |pick: Value, one: Value, name: &str, story: &str| {
        json!([
            {"id": "pick", "values": pick},
            {"id": "one", "values": one},
            {"id": "name", "values": [], "text": name},
            {"id": "story", "values": [], "text": story},
            {"id": "n", "values": ["11"]}])
    };
    // Right answers of the older types are carried.
    let carried = refuse(answers(json!(["a", "b"]), json!(["b"]), "Ada", "A life"));
    assert_eq!(
        defaults(&carried),
        [
            json!(["a", "b"]),
            json!("b"),
            json!("Ada"),
            json!("A life"),
            Value::Null
        ]
    );
    assert_drawable(&screen, &carried);
    // Answers that are not unlike what the form takes are not: too many picks, a repeated pick, a
    // value that is no option, words nobody can see or that the box would refuse.
    let hidden = "Ada\u{202e}evil";
    let long = "x".repeat(501);
    let longer = "y".repeat(2001);
    for (pick, one, name, story) in [
        (json!(["a", "b", "c"]), json!(["zzz"]), hidden, "ok\u{200b}"),
        (
            json!(["a", "a"]),
            json!(["a", "b"]),
            long.as_str(),
            longer.as_str(),
        ),
        (json!(["a", "q"]), json!(["zzz"]), "", ""),
    ] {
        let again = refuse(answers(pick, one, name, story));
        assert_eq!(
            defaults(&again),
            [
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null
            ]
        );
        assert_drawable(&screen, &again);
    }
    // What is hidden or long is the only thing left out: a text a box takes is carried.
    let mixed = refuse(answers(json!(["a"]), json!(["zzz"]), "Ada", &longer));
    assert_eq!(
        defaults(&mixed),
        [
            json!(["a"]),
            Value::Null,
            json!("Ada"),
            Value::Null,
            Value::Null
        ]
    );
    assert_drawable(&screen, &mixed);
}

#[tokio::test]
async fn the_form_asked_again_on_a_screen_without_a_description_keeps_the_description_in_its_words()
{
    let args = json!({"question": "Sure?", "description": "Because of the data.",
        "choices": [{"id": "ok", "question": "Go?", "type": "confirm"}]});
    let wrong = json!({"answers": [{"id": "ok", "values": ["maybe"]}]});
    let say = |refusal: adam_llm_agent::Refusal| refusal.question;
    // Version 2's Choices has no description: it is the second line of the question.
    let v2 = screen(
        include_str!("fixtures/catalog-v2.json"),
        include_str!("fixtures/catalog-v2.lock.json"),
    );
    let (tool, args2, ui) = asked_on(&v2, args.clone()).await;
    let AnswerCheck::Refuse(refusal) =
        tool.read_answer("c1", &args2, Some(&ui), &payload("ask-c1", wrong.clone()))
    else {
        panic!("not refused")
    };
    assert_eq!(
        say(refusal),
        "The answer to \"ok\" (\"Go?\") was not used (not_offered): the answer is neither yes nor no.\n\
         Sure?\nBecause of the data."
    );
    // Version 7's has one, and the words are not said twice.
    let (tool, args7, ui) = asked_on(&screen(V7, V7_LOCK), args).await;
    let AnswerCheck::Refuse(refusal) =
        tool.read_answer("c1", &args7, Some(&ui), &payload("ask-c1", wrong))
    else {
        panic!("not refused")
    };
    assert!(say(refusal).ends_with("the answer is neither yes nor no.\nSure?"));
}

// ---- a yes or no said in words ----

#[tokio::test]
async fn a_yes_or_no_the_person_types_for_a_form_that_went_out_as_words_is_that_answer() {
    let ask = |extra: Value| {
        let mut q = json!({"id": "go", "question": "Delete it?", "type": "confirm"});
        q.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        json!({"question": "Sure?", "choices": [q]})
    };
    let tool = Ui::new(McpPolicy::default())
        .tools()
        .get("ask_user")
        .cloned()
        .unwrap();
    let said =
        |args: &Value, text: &str| tool.read_answer("c1", args, None, &json!({"text": text}));
    let plain = ask(json!({}));
    for (text, want) in [
        ("yes", "yes"),
        ("YES", "yes"),
        ("no", "no"),
        ("No", "no"),
        ("Yes", "yes"),
    ] {
        assert_eq!(said(&plain, text), AnswerCheck::Read(want.into()), "{text}");
    }
    for text in [
        "yes please",
        "y",
        "maybe",
        " yes",
        "yes ",
        "",
        "no way",
        "yes\nno",
    ] {
        assert_eq!(said(&plain, text), AnswerCheck::Accept, "{text:?}");
    }
    // The person's own labels say the same.
    let labelled = ask(json!({"yesLabel": "Delete it", "noLabel": "Keep it"}));
    assert_eq!(
        said(&labelled, "delete it"),
        AnswerCheck::Read("yes".into())
    );
    assert_eq!(said(&labelled, "KEEP IT"), AnswerCheck::Read("no".into()));
    assert_eq!(said(&labelled, "yes"), AnswerCheck::Read("yes".into()));
    assert_eq!(said(&labelled, "delete"), AnswerCheck::Accept);
    // A label that is the other word's is both, and so neither.
    let tangled = ask(json!({"yesLabel": "No", "noLabel": "Yes"}));
    assert_eq!(said(&tangled, "yes"), AnswerCheck::Accept);
    // Only a form of one question that is a yes or no is read so; a form that went out as a form is
    // answered with its form, and what is typed beside it is words.
    let two = json!({"question": "Sure?", "choices": [
        {"id": "go", "question": "Delete it?", "type": "confirm"},
        {"id": "n", "question": "How many?", "type": "number"}]});
    assert_eq!(said(&two, "yes"), AnswerCheck::Accept);
    let single = json!({"question": "Sure?", "choices": [
        {"id": "go", "question": "Which?", "options": ["Yes", "No"]}]});
    assert_eq!(said(&single, "yes"), AnswerCheck::Accept);
    let (tool7, args7, ui7) = asked_on(&screen(V7, V7_LOCK), plain.clone()).await;
    assert_eq!(
        tool7.read_answer("c1", &args7, Some(&ui7), &json!({"text": "yes"})),
        AnswerCheck::Accept
    );
}

#[tokio::test]
async fn the_model_reads_the_value_of_a_yes_or_no_that_was_typed_to_words() {
    let rig = Rig::new();
    rig.model
        .push_tool_calls(vec![ToolCall {
            id: "q-call".into(),
            name: "ask_user".into(),
            arguments: json!({"question": "Delete the branch?", "choices": [
                {"id": "go", "question": "Delete it?", "type": "confirm",
                 "yesLabel": "Delete it", "noLabel": "Keep it"}]}),
            ..Default::default()
        }])
        .push_text("Kept.");
    let (stop, handle) = rig.worker();
    // A screen with no catalog gets the question in words.
    let task = rig
        .backend
        .submit(
            alice(),
            Message::new(Role::User, vec![Part::text("clean up")]),
            None,
            Some("ctx".into()),
        )
        .await
        .unwrap();
    rig.asking(&task, "Delete the branch?").await;
    rig.send(&task, Message::new(Role::User, vec![Part::text("Keep it")]))
        .await;
    rig.until(&task, "completed", |t| {
        t.status.state == TaskState::Completed
    })
    .await;
    stop.send(()).unwrap();
    handle.await.unwrap();
    assert_eq!(
        rig.model.requests()[1].messages.last(),
        Some(&adam_model::Message::tool_result("q-call", "no"))
    );
}

// ---- what is not judged ----

#[tokio::test]
async fn a_call_that_cannot_be_read_back_is_accepted() {
    let (tool, _, ui) = asked_with_v7().await;
    // Nine questions are more than a form holds: the arguments cannot be normalized again.
    let many: Vec<Value> = (0..9)
        .map(|n| json!({"id": format!("q{n}"), "question": "Q?", "type": "confirm"}))
        .collect();
    assert_eq!(
        tool.read_answer(
            "c1",
            &json!({"question": "Q", "choices": many}),
            Some(&ui),
            &payload("ask-c1", json!({"answers": []}))
        ),
        AnswerCheck::Accept
    );
}

#[tokio::test]
async fn a_note_is_the_words_of_the_forms_box_and_nothing_else() {
    let screen = screen(V7, V7_LOCK);
    let ask = |free: bool| {
        json!({"question": "Q", "freeInput": free,
               "choices": [{"id": "ok", "question": "Sure?", "type": "confirm"}]})
    };
    let yes = |note: Value| json!({"answers": [{"id": "ok", "values": ["yes"]}], "note": note});
    for (free, note, want) in [
        (true, json!("hi"), None),
        (true, json!(null), Some("unexpected")),
        (true, json!(5), Some("unexpected")),
        (false, json!("hi"), Some("unexpected")),
        (false, json!(null), Some("unexpected")),
    ] {
        let (tool, args, ui) = asked_on(&screen, ask(free)).await;
        assert_eq!(
            verdict(&tool, &args, &ui, "ask-c1", yes(note.clone())),
            want.map(|rule| ("*".to_owned(), rule.to_owned())),
            "free box {free}, note {note}"
        );
    }
    // No note at all is fine either way.
    for free in [true, false] {
        let (tool, args, ui) = asked_on(&screen, ask(free)).await;
        let none = json!({"answers": [{"id": "ok", "values": ["yes"]}]});
        assert_eq!(verdict(&tool, &args, &ui, "ask-c1", none), None);
    }
}

#[tokio::test]
async fn an_answer_out_of_order_that_also_has_a_foreign_member_is_unknown_first() {
    let (tool, args, ui) = asked_with_v7().await;
    let mut answers = good();
    answers.as_array_mut().unwrap().swap(0, 1);
    assert_eq!(
        verdict(
            &tool,
            &args,
            &ui,
            "ask-c1",
            json!({"answers": answers, "extra": 1})
        ),
        Some(("*".into(), "unknown".into()))
    );
    assert_eq!(
        verdict(
            &tool,
            &args,
            &ui,
            "ask-c1",
            json!({"answers": good(), "extra": 1})
        ),
        Some(("*".into(), "unexpected".into()))
    );
}
