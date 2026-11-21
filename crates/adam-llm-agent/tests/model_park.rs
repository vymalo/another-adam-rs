//! A run parked on the model (ADR 0067 decisions 4, 6, 7 and 8): a refusal whose provider parks
//! keeps the conversation and parks the run, on a timer with a backoff or until woken; the same
//! call is made again after the timer, a message (whose text is the person's turn) or a resume
//! (which adds nothing); a call that is answered clears the wait; `MODEL_PARK_LIMIT` bounds parks on
//! a timer; a run nobody can wake fails; `ModelLocked` and a provider that does not park fail as
//! before; nothing of a refusal's body reaches the journal. A scripted `MockModel` with the model's
//! error injected, a `ManualClock`, and a real `Runtime` with a worker; the journal test goes
//! through `adam-model-openai` to a mock HTTP server.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::time::{Duration, Instant};

use adam_core::time::{Timestamp, span_of};
use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{
    Conversation, LlmAgent, LlmAgentBuilder, MODEL_RESUME_KIND, ModelPark, ModelParking,
    user_message,
};
use adam_model::{DynModel, Message, MockModel, ModelError, ModelRequest, Park};
use adam_runtime::{CollectingSink, Inbound, ManualClock, RetryPolicy, RunView, Runtime};
use serde_json::{Value, json};
use tokio::sync::oneshot;

fn refused(status: u16, slug: Option<&str>, park: Park) -> ModelError {
    ModelError::Refused {
        status,
        slug: slug.map(str::to_owned),
        retry_after: None,
        park,
    }
}

fn timer(secs: u64) -> Park {
    Park::OnTimer(Duration::from_secs(secs))
}

struct Rig {
    store: DynStore,
    mock: Arc<MockModel>,
    clock: ManualClock,
    rt: Runtime,
    stop: Option<oneshot::Sender<()>>,
    worker: Option<tokio::task::JoinHandle<()>>,
}

impl Rig {
    fn new(parking: ModelParking) -> Self {
        Self::with(Arc::new(MockModel::new()), |b| b.model_parking(parking))
    }

    fn with(
        mock: Arc<MockModel>,
        configure: impl FnOnce(LlmAgentBuilder) -> LlmAgentBuilder,
    ) -> Self {
        let model: DynModel = mock.clone();
        Self::over(mock, model, configure)
    }

    fn over(
        mock: Arc<MockModel>,
        model: DynModel,
        configure: impl FnOnce(LlmAgentBuilder) -> LlmAgentBuilder,
    ) -> Self {
        let agent = configure(LlmAgent::builder("llm", model, "test-model")).build();
        let store: DynStore = Arc::new(MemoryStore::new());
        let clock = ManualClock::new();
        let rt = Runtime::builder(store.clone())
            .agent(agent)
            .clock(clock.clone())
            .event_sink(CollectingSink::new())
            .worker_id("w")
            .poll_interval(Duration::from_millis(10))
            .lease_ttl(Duration::from_secs(10))
            .retry(RetryPolicy::new(3).with_backoff(Duration::ZERO, Duration::ZERO))
            .build();
        let (stop, rx) = oneshot::channel::<()>();
        let worker = {
            let rt = rt.clone();
            tokio::spawn(async move {
                rt.run_worker(async {
                    let _ = rx.await;
                })
                .await
                .expect("worker result");
            })
        };
        Self {
            store,
            mock,
            clock,
            rt,
            stop: Some(stop),
            worker: Some(worker),
        }
    }

    async fn start(&self, conversation: Option<&str>) -> RunId {
        self.rt
            .start("llm", user_message("go"), conversation)
            .await
            .expect("start")
    }

    async fn wait_for(&self, run: RunId, what: &str, pred: impl Fn(&RunView) -> bool) -> RunView {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let view = self.rt.view(run).await.expect("view").expect("run exists");
            if pred(&view) {
                return view;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; last view: {view:#?}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// The run parked, with its model wait as `pred` wants it.
    async fn parked(
        &self,
        run: RunId,
        pred: impl Fn(&Conversation) -> bool,
    ) -> (RunView, Conversation) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let view = self.rt.view(run).await.expect("view").expect("run exists");
            if view.status == RunStatus::Parked {
                let state = self.state(run).await;
                if pred(&state) {
                    return (view, state);
                }
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for a park; last view: {view:#?}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn state(&self, run: RunId) -> Conversation {
        let state = self.rt.state(run).await.expect("state").expect("the run");
        serde_json::from_value(state).expect("a Conversation")
    }

    async fn done(&self, run: RunId) -> RunView {
        self.wait_for(run, "done", |v| v.status == RunStatus::Done)
            .await
    }

    async fn failed(&self, run: RunId) -> RunView {
        self.wait_for(run, "failed", |v| v.status == RunStatus::Failed)
            .await
    }

    async fn journal(&self, run: RunId) -> String {
        let entries = self.store.journal_list(run).await.expect("journal");
        serde_json::to_string(
            &entries
                .iter()
                .map(|e| json!({"name": e.name, "ok": e.ok, "payload": e.payload}))
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }

    async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            tokio::time::timeout(Duration::from_secs(10), worker)
                .await
                .expect("worker stops in time")
                .expect("worker task");
        }
    }
}

fn texts(request: &ModelRequest) -> Vec<String> {
    request.messages.iter().map(Message::text).collect()
}

fn text_of(view: &RunView) -> Option<String> {
    view.output
        .as_ref()
        .and_then(|o| o.get("text"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn waits_on_timer(state: &Conversation) -> bool {
    state
        .model_wait
        .as_ref()
        .is_some_and(|w| w.park == ModelPark::Timer)
}

/// Each kind of park, in turn: the run parks with the wake the park says, keeps its conversation as
/// it was before the call, and the same call is made again after the timer; the call that is
/// answered clears the wait.
#[tokio::test]
async fn each_park_keeps_the_conversation_and_the_same_call_is_made_after_the_timer() {
    let cases: [(ModelError, &str, u64); 3] = [
        (
            refused(403, Some("run_not_bound"), timer(30)),
            "the model gateway has not bound this run yet",
            30,
        ),
        (
            refused(401, None, timer(30)),
            "not authorized by the model gateway",
            30,
        ),
        // A park the provider asked to be longer than the backoff's first step is that long.
        (
            refused(429, Some("rate_limited"), timer(120)),
            "the model gateway's rate limit is reached",
            120,
        ),
    ];
    for (error, reason, secs) in cases {
        let rig = Rig::new(ModelParking::default());
        rig.mock.push_error(error).push_text("answered");
        let run = rig.start(None).await;
        let (view, state) = rig.parked(run, waits_on_timer).await;
        let wait = state.model_wait.clone().unwrap();
        assert_eq!(wait.reason, reason);
        assert_eq!(wait.parks, 1);
        assert!(!view.waiting, "a park on a timer is not a question");
        assert_eq!(
            view.wake_at,
            Some(wait.parked_since + span_of(Duration::from_secs(secs))),
            "{reason}"
        );
        assert_eq!(wait.wake_at, view.wake_at);
        // The conversation is as it was before the call: the person's words, no answer, no turn.
        assert_eq!(state.turns, 0);
        assert_eq!(
            state.messages.iter().map(Message::text).collect::<Vec<_>>(),
            ["go"]
        );
        assert_eq!(rig.mock.requests().len(), 1);

        rig.clock.advance(Duration::from_secs(secs + 1));
        let view = rig.done(run).await;
        assert_eq!(text_of(&view).as_deref(), Some("answered"));
        let requests = rig.mock.requests();
        assert_eq!(requests.len(), 2, "the call is made once more");
        assert_eq!(texts(&requests[0]), texts(&requests[1]), "the same call");
        assert_eq!(requests[0].run, requests[1].run);
        assert!(
            rig.state(run).await.model_wait.is_none(),
            "an answered call clears the wait"
        );
        rig.stop().await;
    }
}

/// A park until woken waits like a question: no timer, and the run says why.
#[tokio::test]
async fn a_park_until_woken_has_no_timer_and_a_resume_wakes_it_adding_nothing() {
    let rig = Rig::new(ModelParking::default());
    rig.mock
        .push_error(refused(402, Some("over_limit"), Park::UntilWoken))
        .push_text("answered");
    let run = rig.start(None).await;
    let (view, state) = rig
        .parked(run, |s| {
            s.model_wait
                .as_ref()
                .is_some_and(|w| w.park == ModelPark::UntilWoken)
        })
        .await;
    assert!(view.waiting, "it waits like a question");
    assert_eq!(view.wake_at, None);
    let wait = state.model_wait.unwrap();
    assert_eq!(wait.reason, "the model gateway's limit is reached");
    assert_eq!(
        (wait.status, wait.slug.as_deref()),
        (Some(402), Some("over_limit"))
    );
    assert_eq!(wait.wake_at, None);

    // Nothing wakes it on its own.
    rig.clock.advance(Duration::from_secs(48 * 3600));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(rig.mock.requests().len(), 1);

    // A resume wakes it and adds nothing: the same call is made again.
    rig.rt
        .deliver(
            run,
            Inbound::new(MODEL_RESUME_KIND, json!({})).with_id("resume-1"),
        )
        .await
        .expect("deliver");
    let view = rig.done(run).await;
    assert_eq!(text_of(&view).as_deref(), Some("answered"));
    let requests = rig.mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        texts(&requests[1]),
        texts(&requests[0]),
        "no turn was added"
    );
    let state = rig.state(run).await;
    assert!(state.model_wait.is_none());
    assert_eq!(
        state
            .messages
            .iter()
            .filter(|m| matches!(m, Message::User { .. }))
            .count(),
        1,
        "the resume is no message of the person's"
    );
    rig.stop().await;
}

/// A message wakes a park on a timer before its time, and its text is the person's turn, added
/// before the call.
#[tokio::test]
async fn a_message_wakes_a_timer_park_and_its_text_is_added() {
    let rig = Rig::new(ModelParking::default());
    rig.mock
        .push_error(refused(503, Some("unavailable"), timer(30)))
        .push_text("answered");
    let run = rig.start(None).await;
    rig.parked(run, waits_on_timer).await;
    rig.rt
        .deliver(run, user_message("are you there?"))
        .await
        .expect("deliver");
    let view = rig.done(run).await;
    assert_eq!(text_of(&view).as_deref(), Some("answered"));
    let requests = rig.mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(texts(&requests[1]), ["go", "are you there?"]);
    rig.stop().await;
}

/// Parks on a timer in a row back off (30 s, then 60 s) and are bounded by the limit from the first
/// of them: past it the run fails with the last reason.
#[tokio::test]
async fn parks_in_a_row_back_off_and_past_the_limit_the_run_fails() {
    let rig = Rig::new(ModelParking::new(Duration::from_secs(60)));
    for _ in 0..3 {
        rig.mock
            .push_error(refused(403, Some("lease_expired"), timer(30)));
    }
    let run = rig.start(None).await;
    let (view, first) = rig.parked(run, waits_on_timer).await;
    let first = first.model_wait.unwrap();
    assert_eq!(
        view.wake_at,
        Some(first.parked_since + span_of(Duration::from_secs(30)))
    );

    rig.clock.advance(Duration::from_secs(31));
    let (view, second) = rig
        .parked(run, |s| s.model_wait.as_ref().is_some_and(|w| w.parks == 2))
        .await;
    let second = second.model_wait.unwrap();
    assert_eq!(second.parked_since, first.parked_since, "one row of parks");
    let now = view.wake_at.unwrap() - span_of(Duration::from_secs(60));
    assert!(
        now >= first.parked_since + span_of(Duration::from_secs(31)),
        "the second park waits twice the first: {:?} from {:?}",
        view.wake_at,
        first.parked_since
    );

    rig.clock.advance(Duration::from_secs(61));
    let view = rig.failed(run).await;
    let error = view.error.unwrap();
    assert!(
        error.contains("the model gateway's lease for this run has ended (HTTP 403 lease_expired)"),
        "{error}"
    );
    assert!(error.contains("MODEL_PARK_LIMIT"), "{error}");
    assert_eq!(rig.mock.requests().len(), 3);
    rig.stop().await;
}

/// A schedule's firing has nobody to wake it: a park until woken fails it with the reason. A park
/// on a timer still wakes on its own.
#[tokio::test]
async fn an_unattended_run_fails_on_a_park_until_woken_and_waits_on_a_timer() {
    let firing = format!("{}nightly:1", adam_runtime::SCHEDULE_CONVERSATION_PREFIX);

    let rig = Rig::new(ModelParking::default());
    rig.mock
        .push_error(refused(402, Some("over_limit"), Park::UntilWoken));
    let run = rig.start(Some(&firing)).await;
    let view = rig.failed(run).await;
    let error = view.error.unwrap();
    assert!(
        error.contains("the model gateway's limit is reached (HTTP 402 over_limit)"),
        "{error}"
    );
    assert!(error.contains("nobody can wake it"), "{error}");
    rig.stop().await;

    let rig = Rig::new(ModelParking::default());
    rig.mock
        .push_error(refused(403, Some("run_not_bound"), timer(30)))
        .push_text("answered");
    let run = rig.start(Some(&format!("{firing}:b"))).await;
    rig.parked(run, waits_on_timer).await;
    rig.clock.advance(Duration::from_secs(31));
    rig.done(run).await;
    rig.stop().await;
}

/// An endpoint that cannot be reached is retried as before, and parks on a timer once the retries
/// are spent when the agent parks on it; without that, the run fails as before.
#[tokio::test]
async fn an_unreachable_endpoint_parks_once_its_retries_are_spent() {
    let rig = Rig::new(ModelParking::default().when_unreachable(true));
    for _ in 0..3 {
        rig.mock.push_error(ModelError::unreachable());
    }
    rig.mock.push_text("answered");
    let run = rig.start(None).await;
    let (_, state) = rig.parked(run, waits_on_timer).await;
    let wait = state.model_wait.unwrap();
    assert_eq!(wait.reason, "the model gateway cannot be reached");
    assert_eq!((wait.status, wait.slug.as_deref()), (None, None));
    assert_eq!(rig.mock.requests().len(), 3, "three attempts first");
    rig.clock.advance(Duration::from_secs(31));
    rig.done(run).await;
    rig.stop().await;

    let rig = Rig::new(ModelParking::default());
    for _ in 0..3 {
        rig.mock.push_error(ModelError::unreachable());
    }
    let run = rig.start(None).await;
    let view = rig.failed(run).await;
    assert!(
        view.error.unwrap().contains("could not be reached"),
        "fails as before"
    );
    rig.stop().await;
}

/// `ModelLocked` is no park: the run fails naming the model.
#[tokio::test]
async fn a_locked_model_fails_the_run_naming_it() {
    let rig = Rig::new(ModelParking::default().when_unreachable(true));
    rig.mock.push_error(ModelError::ModelLocked {
        model: "test-model".into(),
    });
    let run = rig.start(None).await;
    let error = rig.failed(run).await.error.unwrap();
    assert_eq!(
        error,
        "model call failed: the model endpoint does not allow the model `test-model`"
    );
    rig.stop().await;
}

/// A provider that does not park (`StaticBearer`) answers a `401` with `ModelError::Auth` and a
/// `402` with `InvalidRequest`: the run fails as it did before.
#[tokio::test]
async fn a_provider_that_does_not_park_fails_as_before() {
    for (error, said) in [
        (
            ModelError::Auth("HTTP 401: invalid key".into()),
            "model call failed: authentication failed: HTTP 401: invalid key",
        ),
        (
            ModelError::invalid_request("HTTP 402"),
            "model call failed: invalid request: HTTP 402",
        ),
    ] {
        let rig = Rig::new(ModelParking::default().when_unreachable(true));
        rig.mock.push_error(error);
        let run = rig.start(None).await;
        assert_eq!(rig.failed(run).await.error.as_deref(), Some(said));
        assert!(rig.state(run).await.model_wait.is_none());
        rig.stop().await;
    }
}

/// A cancel ends a park, as it ends any wait.
#[tokio::test]
async fn a_cancel_ends_a_park() {
    let rig = Rig::new(ModelParking::default());
    rig.mock
        .push_error(refused(402, Some("over_limit"), Park::UntilWoken));
    let run = rig.start(None).await;
    rig.parked(run, |s| s.model_wait.is_some()).await;
    rig.rt.cancel(run, "stop").await.expect("cancel");
    let view = rig.failed(run).await;
    assert_eq!(view.error.as_deref(), Some("cancelled: stop"));
    rig.stop().await;
}

/// The journal of a parked run holds the status and the slug of the refusal and nothing of its
/// body, through the real client: a problem document whose words and credential-looking values no
/// record may hold.
#[tokio::test]
async fn nothing_of_a_refusals_body_reaches_the_journal() {
    use adam_model_openai::{OpenAiCompatible, OpenAiConfig};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const SECRET: &str = "sk-live-SECRET-0f9e8d";
    const WORDS: &str = "the lease of this person";
    for (status, slug) in [(402, "over_limit"), (403, "run_not_bound")] {
        let server = MockServer::start().await;
        let body = format!(
            r#"{{"type":"https://gateway.example.test/problems/{slug}","title":"{WORDS}","detail":"Authorization: Bearer {SECRET}","credential":"{SECRET}"}}"#
        );
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(status).set_body_raw(body, "application/problem+json"),
            )
            .mount(&server)
            .await;
        let client = OpenAiCompatible::new(OpenAiConfig::new(
            format!("{}/v1", server.uri()),
            secrecy::SecretString::from(""),
        ))
        .expect("client")
        .with_credentials(Arc::new(adam_model::InjectedRun::new()));
        let model: DynModel = Arc::new(client);
        let rig = Rig::over(Arc::new(MockModel::new()), model, |b| {
            b.model_parking(ModelParking::default().when_unreachable(true))
        });
        let run = rig.start(None).await;
        let (_, state) = rig.parked(run, |s| s.model_wait.is_some()).await;
        let journal = rig.journal(run).await;
        assert!(
            journal.contains(&format!("HTTP {status} {slug}")),
            "{journal}"
        );
        let view = rig.rt.view(run).await.unwrap().unwrap();
        let everything = format!(
            "{journal} | {} | {:?} | {:?}",
            serde_json::to_string(&state).unwrap(),
            view.error,
            view.state
        );
        for hidden in [
            SECRET,
            WORDS,
            "Bearer",
            "credential",
            "gateway.example.test",
        ] {
            assert!(
                !everything.contains(hidden),
                "{hidden} leaked: {everything}"
            );
        }
        rig.stop().await;
    }
}

/// The time a run waits on the model is said in `parked_since`, a journaled time.
#[tokio::test]
async fn the_first_park_of_a_row_is_dated_by_the_journaled_clock() {
    let rig = Rig::new(ModelParking::default());
    rig.mock
        .push_error(refused(403, Some("run_not_bound"), timer(30)));
    let before = rig.clock_now();
    let run = rig.start(None).await;
    let (_, state) = rig.parked(run, waits_on_timer).await;
    let since = state.model_wait.unwrap().parked_since;
    assert!(since >= before, "{since} before {before}");
    rig.stop().await;
}

impl Rig {
    fn clock_now(&self) -> Timestamp {
        adam_runtime::Clock::now(&self.clock)
    }
}
