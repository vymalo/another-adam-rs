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
    PendingWait, user_message,
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
        let mut rig = Self::build(mock, vec![agent], Arc::new(MemoryStore::new()));
        rig.spawn();
        rig
    }

    /// A runtime over `store` with `agents` registered, and no worker yet ([`spawn`](Self::spawn)).
    fn build(mock: Arc<MockModel>, agents: Vec<LlmAgent>, store: DynStore) -> Self {
        let clock = ManualClock::new();
        let mut builder = Runtime::builder(store.clone())
            .clock(clock.clone())
            .event_sink(CollectingSink::new())
            .worker_id("w")
            .poll_interval(Duration::from_millis(10))
            .lease_ttl(Duration::from_secs(10))
            .retry(RetryPolicy::new(3).with_backoff(Duration::ZERO, Duration::ZERO));
        for agent in agents {
            builder = builder.agent(agent);
        }
        Self {
            store,
            mock,
            clock,
            rt: builder.build(),
            stop: None,
            worker: None,
        }
    }

    fn spawn(&mut self) {
        let (stop, rx) = oneshot::channel::<()>();
        let rt = self.rt.clone();
        self.worker = Some(tokio::spawn(async move {
            rt.run_worker(async {
                let _ = rx.await;
            })
            .await
            .expect("worker result");
        }));
        self.stop = Some(stop);
    }

    /// The times the run's journal recorded for its clock reads (`Ctx::now_journaled`), in order.
    async fn journaled_nows(&self, run: RunId) -> Vec<Timestamp> {
        let entries = self.store.journal_list(run).await.expect("journal");
        entries
            .into_iter()
            .filter(|e| e.name == "ctx.now" && e.ok)
            .map(|e| serde_json::from_value(e.payload).expect("a time"))
            .collect()
    }

    /// The model's requests, once at least `at_least` have been made.
    async fn requests_at_least(&self, at_least: usize) -> Vec<ModelRequest> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let requests = self.mock.requests();
            if requests.len() >= at_least {
                return requests;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {at_least} model calls; {} were made",
                requests.len()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
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
            // A child run may not exist yet.
            let Some(view) = self.rt.view(run).await.expect("view") else {
                assert!(Instant::now() < deadline, "the run {run} never started");
                tokio::time::sleep(Duration::from_millis(5)).await;
                continue;
            };
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

/// Parks on a timer in a row back off (30 s, then 60 s, from the journaled time of each) and are
/// bounded by the limit from the first of them: past it the run fails with the last reason.
#[tokio::test]
async fn parks_in_a_row_back_off_and_past_the_limit_the_run_fails() {
    let rig = Rig::new(ModelParking::new(Duration::from_secs(600)));
    for _ in 0..3 {
        rig.mock
            .push_error(refused(403, Some("lease_expired"), timer(30)));
    }
    let run = rig.start(None).await;
    let (view, first) = rig.parked(run, waits_on_timer).await;
    let first = first.model_wait.unwrap();
    let nows = rig.journaled_nows(run).await;
    assert_eq!(nows, [first.parked_since], "dated by the journaled clock");
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
    let nows = rig.journaled_nows(run).await;
    assert_eq!(nows.len(), 2);
    assert_eq!(
        view.wake_at,
        Some(nows[1] + span_of(Duration::from_secs(60))),
        "the second park waits twice the first"
    );

    rig.clock.advance(Duration::from_secs(600));
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

/// A park the provider asks to be longer than the limit wakes at the limit, and the run fails then
/// if it is refused again: it does not wait a day to fail.
#[tokio::test]
async fn a_park_longer_than_the_limit_wakes_at_the_limit() {
    let rig = Rig::new(ModelParking::new(Duration::from_secs(3600)));
    for _ in 0..2 {
        rig.mock
            .push_error(refused(429, Some("rate_limited"), timer(24 * 3600)));
    }
    let run = rig.start(None).await;
    let (view, state) = rig.parked(run, waits_on_timer).await;
    let since = state.model_wait.unwrap().parked_since;
    assert_eq!(
        view.wake_at,
        Some(since + span_of(Duration::from_secs(3600))),
        "clamped to the limit"
    );
    rig.clock.advance(Duration::from_secs(3601));
    let error = rig.failed(run).await.error.unwrap();
    assert!(error.contains("MODEL_PARK_LIMIT"), "{error}");
    rig.stop().await;
}

/// A message to a park on a timer whose call is refused again: the second park of the row keeps
/// its first time, and the message's words stay in the history for the call after.
#[tokio::test]
async fn a_message_to_a_park_refused_again_is_kept_and_the_row_goes_on() {
    let rig = Rig::new(ModelParking::default());
    rig.mock
        .push_error(refused(503, Some("unavailable"), timer(30)))
        .push_error(refused(503, Some("unavailable"), timer(30)))
        .push_text("answered");
    let run = rig.start(None).await;
    let (_, first) = rig.parked(run, waits_on_timer).await;
    let first = first.model_wait.unwrap();
    rig.rt
        .deliver(run, user_message("still there?"))
        .await
        .expect("deliver");
    let (_, second) = rig
        .parked(run, |s| s.model_wait.as_ref().is_some_and(|w| w.parks == 2))
        .await;
    assert_eq!(
        second.model_wait.as_ref().unwrap().parked_since,
        first.parked_since
    );
    assert_eq!(
        second
            .messages
            .iter()
            .map(Message::text)
            .collect::<Vec<_>>(),
        ["go", "still there?"]
    );
    rig.clock.advance(Duration::from_secs(61));
    rig.done(run).await;
    let requests = rig.mock.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(texts(&requests[2]), ["go", "still there?"], "read once");
    rig.stop().await;
}

/// A park until woken and then a park on a timer are two rows: the time a person took to raise a
/// limit is not counted against the parks on a timer.
#[tokio::test]
async fn a_park_until_woken_then_a_timer_park_starts_a_new_row() {
    let rig = Rig::new(ModelParking::default());
    rig.mock
        .push_error(refused(402, Some("over_limit"), Park::UntilWoken))
        .push_error(refused(403, Some("run_not_bound"), timer(30)))
        .push_text("answered");
    let run = rig.start(None).await;
    let (_, woken) = rig
        .parked(run, |s| {
            s.model_wait
                .as_ref()
                .is_some_and(|w| w.park == ModelPark::UntilWoken)
        })
        .await;
    let woken = woken.model_wait.unwrap();
    rig.clock.advance(Duration::from_secs(5 * 3600));
    rig.rt
        .deliver(
            run,
            Inbound::new(MODEL_RESUME_KIND, json!({})).with_id("r-1"),
        )
        .await
        .expect("deliver");
    let (_, timed) = rig.parked(run, waits_on_timer).await;
    let timed = timed.model_wait.unwrap();
    assert_eq!(timed.parks, 1, "a new row");
    let nows = rig.journaled_nows(run).await;
    assert_eq!(nows, [woken.parked_since, timed.parked_since]);
    assert!(timed.parked_since >= woken.parked_since + span_of(Duration::from_secs(5 * 3600)));
    rig.clock.advance(Duration::from_secs(31));
    rig.done(run).await;
    rig.stop().await;
}

/// A crash between the model call's step and the commit of the park: the transition is replayed
/// from the journal, which says the same refusal and the same time, so the park is the same (no
/// second call, no non-determinism).
#[tokio::test]
async fn a_park_replayed_after_a_lost_commit_is_the_same_park() {
    use adam_store_testkit::fault::{FaultyStore, Method};

    let faulty = Arc::new(FaultyStore::new(Arc::new(MemoryStore::new())));
    let store: DynStore = faulty.clone();
    let mock = Arc::new(MockModel::new());
    let model: DynModel = mock.clone();
    let agent = LlmAgent::builder("llm", model, "test-model")
        .model_parking(ModelParking::default())
        .build();
    let mut rig = Rig::build(mock, vec![agent], store);
    rig.mock
        .push_error(refused(403, Some("run_not_bound"), timer(30)))
        .push_text("answered");
    let run = rig.start(None).await;
    // The first commit of the run is the park's; it is lost, and the worker replays the step.
    faulty.fail_run(Method::CommitRun, run, 1);
    rig.spawn();
    let (view, state) = rig.parked(run, waits_on_timer).await;
    assert_eq!(
        faulty.injected(Method::CommitRun),
        1,
        "the commit was lost once"
    );
    assert_eq!(rig.mock.requests().len(), 1, "the replay calls no model");
    let wait = state.model_wait.unwrap();
    assert_eq!(
        rig.journaled_nows(run).await,
        [wait.parked_since],
        "one clock read"
    );
    assert_eq!(
        view.wake_at,
        Some(wait.parked_since + span_of(Duration::from_secs(30)))
    );
    rig.clock.advance(Duration::from_secs(31));
    rig.done(run).await;
    rig.stop().await;
}

/// A schedule's firing has nobody to wake it: a park until woken fails it with the reason, and so
/// does a refusal of status 401 or 403 (nothing binds a firing yet). Its other parks on a timer still
/// wake on their own.
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

    // Nothing binds a firing at the gateway yet: a refusal of 401 or 403 fails it too.
    for (status, slug) in [(403, Some("run_not_bound")), (401, None)] {
        let rig = Rig::new(ModelParking::default());
        rig.mock.push_error(refused(status, slug, timer(30)));
        let run = rig.start(Some(&format!("{firing}:{status}"))).await;
        let error = rig.failed(run).await.error.unwrap();
        assert!(error.contains(&format!("HTTP {status}")), "{error}");
        assert!(error.contains("nothing binds a firing"), "{error}");
        rig.stop().await;
    }

    let rig = Rig::new(ModelParking::default());
    rig.mock
        .push_error(refused(503, Some("unavailable"), timer(30)))
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

/// A point a model call waits at until the test lets it go.
#[derive(Default)]
struct Gate {
    reached: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

/// Wraps the mock: the `hold_on`-th call (0-based) waits at the gate.
struct GateModel {
    inner: Arc<MockModel>,
    hold_on: usize,
    calls: std::sync::atomic::AtomicUsize,
    gate: Arc<Gate>,
}

impl GateModel {
    async fn hold(&self) {
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if n == self.hold_on {
            self.gate.reached.notify_one();
            self.gate.release.notified().await;
        }
    }
}

#[async_trait::async_trait]
impl adam_model::ModelClient for GateModel {
    async fn complete(&self, req: ModelRequest) -> Result<adam_model::ModelResponse, ModelError> {
        self.hold().await;
        self.inner.complete(req).await
    }

    async fn stream(
        &self,
        req: ModelRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, Result<adam_model::ModelDelta, ModelError>>,
        ModelError,
    > {
        self.hold().await;
        self.inner.stream(req).await
    }
}

/// A resume that lands while the woken call is in flight says nothing to the run: it is a wake-up
/// only, so the run does not take another turn after its answer. Two requests, and the run is
/// done once.
#[tokio::test]
async fn a_resume_that_lands_during_the_woken_call_adds_no_turn() {
    let mock = Arc::new(MockModel::new());
    let gate = Arc::new(Gate::default());
    let model: DynModel = Arc::new(GateModel {
        inner: mock.clone(),
        hold_on: 1,
        calls: Default::default(),
        gate: gate.clone(),
    });
    let rig = Rig::over(mock, model, |b| b.model_parking(ModelParking::default()));
    rig.mock
        .push_error(refused(402, Some("over_limit"), Park::UntilWoken))
        .push_text("answered");
    let run = rig.start(None).await;
    rig.parked(run, |s| s.model_wait.is_some()).await;
    rig.rt
        .deliver(
            run,
            Inbound::new(MODEL_RESUME_KIND, json!({})).with_id("r-1"),
        )
        .await
        .expect("deliver");
    tokio::time::timeout(Duration::from_secs(20), gate.reached.notified())
        .await
        .expect("the woken call is made");
    rig.rt
        .deliver(
            run,
            Inbound::new(MODEL_RESUME_KIND, json!({})).with_id("r-2"),
        )
        .await
        .expect("a resume during the call is delivered");
    gate.release.notify_one();
    let view = rig.done(run).await;
    assert_eq!(text_of(&view).as_deref(), Some("answered"));
    tokio::time::sleep(Duration::from_millis(200)).await;
    let view = rig.rt.view(run).await.unwrap().unwrap();
    assert_eq!(view.status, RunStatus::Done, "not reopened");
    assert_eq!(rig.mock.requests().len(), 2, "no turn after the answer");
    rig.stop().await;
}

/// The parent's subagent tool: starts the child `helper` and waits for it.
struct Spawn;

#[async_trait::async_trait]
impl adam_llm_agent::Tool for Spawn {
    fn spec(&self) -> adam_model::ToolSpec {
        adam_model::ToolSpec {
            name: "sub".into(),
            description: "hands the work to the helper".into(),
            parameters: json!({"type": "object"}),
        }
    }
    async fn call(
        &self,
        ctx: &adam_llm_agent::ToolCtx,
        _args: Value,
    ) -> Result<adam_llm_agent::ToolOutput, adam_llm_agent::ToolError> {
        let run = ctx.start_child("helper", "go").await?;
        Err(adam_llm_agent::ToolError::AwaitRun { run })
    }
}

/// The subagent tool of a run whose child is `agent`: the same call as [`Spawn`], for a tree.
struct SpawnOf(&'static str);

#[async_trait::async_trait]
impl adam_llm_agent::Tool for SpawnOf {
    fn spec(&self) -> adam_model::ToolSpec {
        adam_model::ToolSpec {
            name: "sub".into(),
            description: "hands the work to a helper".into(),
            parameters: json!({"type": "object"}),
        }
    }
    async fn call(
        &self,
        ctx: &adam_llm_agent::ToolCtx,
        _args: Value,
    ) -> Result<adam_llm_agent::ToolOutput, adam_llm_agent::ToolError> {
        let run = ctx.start_child(self.0, "go").await?;
        Err(adam_llm_agent::ToolError::AwaitRun { run })
    }
}

/// Answers a text of the size asked, so a history reaches its budget.
struct Big;

#[async_trait::async_trait]
impl adam_llm_agent::Tool for Big {
    fn spec(&self) -> adam_model::ToolSpec {
        adam_model::ToolSpec {
            name: "big".into(),
            description: "answers a text of the size asked".into(),
            parameters: json!({"type": "object"}),
        }
    }
    async fn call(
        &self,
        _ctx: &adam_llm_agent::ToolCtx,
        args: Value,
    ) -> Result<adam_llm_agent::ToolOutput, adam_llm_agent::ToolError> {
        let n = args["n"].as_u64().unwrap_or(10) as usize;
        let tag = args["tag"].as_str().unwrap_or("t");
        Ok(adam_llm_agent::ToolOutput::text(format!(
            "{tag}:{}",
            "x".repeat(n)
        )))
    }
}

fn tool_call(id: &str, name: &str, args: Value) -> adam_model::ToolCall {
    adam_model::ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
        ..Default::default()
    }
}

/// A parent `llm` with the subagent tool and a child `helper` with `big`, on one scripted model,
/// both parking on the model.
fn family(max_history_tokens: u32) -> Rig {
    let mock = Arc::new(MockModel::new());
    let model: DynModel = mock.clone();
    family_over(mock, model, max_history_tokens)
}

/// [`family`] over `model`, whose answers are `mock`'s: a model that holds a call at a gate is
/// what lets a test see a run while a call is in flight.
fn family_over(mock: Arc<MockModel>, model: DynModel, max_history_tokens: u32) -> Rig {
    let parent = LlmAgent::builder("llm", model.clone(), "test-model")
        .tool(Spawn)
        .model_parking(ModelParking::default())
        .build();
    let child = LlmAgent::builder("helper", model, "test-model")
        .tool(Big)
        .limits(adam_llm_agent::Limits {
            max_history_tokens,
            ..adam_llm_agent::Limits::default()
        })
        .model_parking(ModelParking::default())
        .build();
    let mut rig = Rig::build(mock, vec![parent, child], Arc::new(MemoryStore::new()));
    rig.spawn();
    rig
}

/// A root `llm` whose subagent is `helper`, whose own subagent is `leaf`: three runs, each parking
/// on the model, on one scripted model.
fn tree() -> Rig {
    let mock = Arc::new(MockModel::new());
    let model: DynModel = mock.clone();
    let root = LlmAgent::builder("llm", model.clone(), "test-model")
        .tool(SpawnOf("helper"))
        .model_parking(ModelParking::default())
        .build();
    let helper = LlmAgent::builder("helper", model.clone(), "test-model")
        .tool(SpawnOf("leaf"))
        .model_parking(ModelParking::default())
        .build();
    let leaf = LlmAgent::builder("leaf", model, "test-model")
        .model_parking(ModelParking::default())
        .build();
    let mut rig = Rig::build(mock, vec![root, helper, leaf], Arc::new(MemoryStore::new()));
    rig.spawn();
    rig
}

/// The model wait the run waits on, its own or its pending child's copy, if it has one.
fn wait_of(state: &Conversation) -> Option<adam_llm_agent::ModelWait> {
    match &state.pending_wait {
        Some(adam_llm_agent::PendingWait::Run(wait)) => wait.model_wait.clone(),
        _ => state.model_wait.clone(),
    }
}

/// A subagent's park until woken is shown on its parent, which waits with no timer and says why; a
/// message to the parent wakes the child through it, and its words stay at the parent (ADR 0067
/// decision 5).
#[tokio::test]
async fn a_parked_subagent_shows_on_its_parent_and_a_message_wakes_it_through() {
    let rig = family(0);
    rig.mock
        .push_tool_calls(vec![tool_call("c1", "sub", json!({}))])
        .push_error(refused(402, Some("over_limit"), Park::UntilWoken))
        .push_text("the helper's answer")
        .push_text("all done");
    let run = rig.start(None).await;
    let child = adam_runtime::child_run_id(run, "c1");

    // The child parks; the parent reads its wait and waits with no timer, as on a question.
    let (view, state) = rig
        .parked(
            run,
            |s| matches!(&s.pending_wait, Some(PendingWait::Run(w)) if w.model_wait.is_some()),
        )
        .await;
    assert!(view.waiting, "the parent waits like a question");
    assert_eq!(view.wake_at, None);
    let Some(PendingWait::Run(wait)) = state.pending_wait else {
        panic!("the parent waits on the child")
    };
    let copy = wait.model_wait.expect("the copy of the child's wait");
    assert_eq!(copy.park, ModelPark::UntilWoken);
    assert_eq!(copy.reason, "the model gateway's limit is reached");
    assert!(
        rig.rt
            .view(child)
            .await
            .unwrap()
            .expect("the child")
            .waiting,
        "the child parked until woken"
    );
    assert_eq!(
        rig.mock.requests().len(),
        2,
        "the parent and the child each called once"
    );

    // A message to the parent: its words stay there, and one resume goes down to the child.
    rig.rt
        .deliver(run, user_message("the limit is raised"))
        .await
        .expect("deliver");
    let done = rig.done(run).await;
    assert_eq!(text_of(&done).as_deref(), Some("all done"));
    let requests = rig.mock.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(
        texts(&requests[2]),
        texts(&requests[1]),
        "the child makes the same call"
    );
    let child = rig.state(child).await;
    assert!(
        child
            .messages
            .iter()
            .all(|m| m.text() != "the limit is raised"),
        "no message of the person's reached the child"
    );
    let parent = rig.state(run).await;
    assert!(
        parent
            .messages
            .iter()
            .any(|m| m.text() == "the limit is raised"),
        "the words stayed at the parent"
    );
    assert!(parent.pending_wait.is_none());
    rig.stop().await;
}

/// A resume to the parent adds nothing to it and wakes the child through it: the same call is made
/// again, and the parent's history has no turn for the resume (ADR 0067 decision 5).
#[tokio::test]
async fn a_resume_to_the_parent_adds_nothing_and_wakes_the_child_through_it() {
    let rig = family(0);
    rig.mock
        .push_tool_calls(vec![tool_call("c1", "sub", json!({}))])
        .push_error(refused(402, Some("over_limit"), Park::UntilWoken))
        .push_text("the helper's answer")
        .push_text("all done");
    let run = rig.start(None).await;
    rig.parked(
        run,
        |s| matches!(&s.pending_wait, Some(PendingWait::Run(w)) if w.model_wait.is_some()),
    )
    .await;

    rig.rt
        .deliver(
            run,
            Inbound::new(MODEL_RESUME_KIND, json!({})).with_id("r-1"),
        )
        .await
        .expect("deliver");
    let done = rig.done(run).await;
    assert_eq!(text_of(&done).as_deref(), Some("all done"));
    let requests = rig.mock.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(texts(&requests[2]), texts(&requests[1]));
    let parent = rig.state(run).await;
    assert_eq!(
        parent
            .messages
            .iter()
            .filter(|m| matches!(m, Message::User { .. }))
            .count(),
        1,
        "the resume is no message of the person's"
    );
    rig.stop().await;
}

/// The copy of a message the parent already read wakes the child once: a sender that delivers the
/// same id again costs no second resume, and a message with a new id still wakes the child through
/// the parent.
#[tokio::test]
async fn a_copy_of_the_message_that_woke_the_parent_wakes_the_child_once() {
    let rig = family(0);
    rig.mock
        .push_tool_calls(vec![tool_call("c1", "sub", json!({}))])
        .push_error(refused(402, Some("over_limit"), Park::UntilWoken))
        .push_error(refused(402, Some("over_limit"), Park::UntilWoken))
        .push_text("the helper's answer")
        .push_text("all done");
    let run = rig.start(None).await;
    let child = adam_runtime::child_run_id(run, "c1");
    rig.parked(
        run,
        |s| matches!(&s.pending_wait, Some(PendingWait::Run(w)) if w.model_wait.is_some()),
    )
    .await;

    // The first message wakes the child, which is refused again and parks.
    rig.rt
        .deliver(run, user_message("the limit is raised").with_id("m-1"))
        .await
        .expect("deliver");
    rig.requests_at_least(3).await;
    rig.wait_for(child, "the child parked again", |v| {
        v.status == RunStatus::Parked
    })
    .await;
    assert_eq!(rig.mock.requests().len(), 3);

    // The same id again is a copy the parent has read: it forwards nothing.
    rig.rt
        .deliver(run, user_message("the limit is raised").with_id("m-1"))
        .await
        .expect("a copy is delivered");
    let (view, _) = rig
        .parked(
            run,
            |s| matches!(&s.pending_wait, Some(PendingWait::Run(w)) if w.model_wait.is_some()),
        )
        .await;
    assert!(view.waiting);
    assert_eq!(
        rig.mock.requests().len(),
        3,
        "the copy woke nothing: the child read one resume"
    );
    assert_eq!(
        rig.rt.view(child).await.unwrap().unwrap().pending_inbox,
        0,
        "and no resume waits in its inbox"
    );

    // A message with a new id still reaches it.
    rig.rt
        .deliver(run, user_message("now really raised").with_id("m-2"))
        .await
        .expect("deliver");
    let done = rig.done(run).await;
    assert_eq!(text_of(&done).as_deref(), Some("all done"));
    assert_eq!(rig.mock.requests().len(), 5);
    rig.stop().await;
}

/// A forwarded resume clears the parent's copy while the child works, and the parent parks on its
/// 60 s look again (ADR 0067 decision 5).
#[tokio::test]
async fn a_forwarded_resume_clears_the_copy_while_the_child_works() {
    let mock = Arc::new(MockModel::new());
    let gate = Arc::new(Gate::default());
    let model: DynModel = Arc::new(GateModel {
        inner: mock.clone(),
        hold_on: 2,
        calls: Default::default(),
        gate: gate.clone(),
    });
    let rig = family_over(mock, model, 0);
    rig.mock
        .push_tool_calls(vec![tool_call("c1", "sub", json!({}))])
        .push_error(refused(402, Some("over_limit"), Park::UntilWoken))
        .push_text("the helper's answer")
        .push_text("all done");
    let run = rig.start(None).await;
    rig.parked(
        run,
        |s| matches!(&s.pending_wait, Some(PendingWait::Run(w)) if w.model_wait.is_some()),
    )
    .await;

    rig.rt
        .deliver(run, user_message("the limit is raised"))
        .await
        .expect("deliver");
    tokio::time::timeout(Duration::from_secs(20), gate.reached.notified())
        .await
        .expect("the child's woken call is made");
    let (view, state) = rig
        .parked(
            run,
            |s| matches!(&s.pending_wait, Some(PendingWait::Run(w)) if w.model_wait.is_none()),
        )
        .await;
    assert!(!view.waiting, "the child works: the parent keeps its timer");
    assert!(view.wake_at.is_some());
    let Some(PendingWait::Run(wait)) = state.pending_wait else {
        panic!("the parent waits on the child")
    };
    assert!(wait.model_wait.is_none(), "the forwarded resume cleared it");
    gate.release.notify_one();
    let done = rig.done(run).await;
    assert_eq!(text_of(&done).as_deref(), Some("all done"));
    rig.stop().await;
}

/// A child on a timer keeps the parent's 60 s look, and the child's reason is on the parent's head
/// for a front to read (ADR 0067 decision 5).
#[tokio::test]
async fn a_child_on_a_timer_keeps_the_parents_look_and_its_reason() {
    let rig = family(0);
    rig.mock
        .push_tool_calls(vec![tool_call("c1", "sub", json!({}))])
        .push_error(refused(503, Some("unavailable"), timer(600)))
        .push_text("the helper's answer")
        .push_text("all done");
    let run = rig.start(None).await;
    let (view, state) = rig
        .parked(run, |s| {
            matches!(&s.pending_wait, Some(PendingWait::Run(w))
                if w.model_wait.as_ref().is_some_and(|wait| wait.park == ModelPark::Timer))
        })
        .await;
    assert!(!view.waiting, "a child on a timer is no question");
    assert!(view.wake_at.is_some(), "the parent keeps its 60 s look");
    let copy = wait_of(&state).expect("the copy");
    assert_eq!(copy.reason, "the model gateway is busy");
    let head = rig.rt.view(run).await.unwrap().unwrap().state;
    assert_eq!(
        head.pointer("/pending_wait/model_wait/reason"),
        Some(&json!("the model gateway is busy")),
        "a front reads the reason from the head"
    );
    rig.stop().await;
}

/// A resume reaches a grandchild one level at a time: each run forwards one to the child it waits
/// on, and the words of the message stay at the root (ADR 0067 decision 5).
#[tokio::test]
async fn a_resume_reaches_a_grandchild_one_level_at_a_time() {
    let rig = tree();
    rig.mock
        .push_tool_calls(vec![tool_call("c1", "sub", json!({}))])
        .push_tool_calls(vec![tool_call("c1", "sub", json!({}))])
        .push_error(refused(402, Some("over_limit"), Park::UntilWoken))
        .push_text("the leaf's answer")
        .push_text("helper done")
        .push_text("all done");
    let run = rig.start(None).await;
    let helper = adam_runtime::child_run_id(run, "c1");
    let leaf = adam_runtime::child_run_id(helper, "c1");

    // The leaf parks, the helper copies its wait, and the root copies the helper's copy.
    let (view, state) = rig
        .parked(
            run,
            |s| matches!(&s.pending_wait, Some(PendingWait::Run(w)) if w.model_wait.is_some()),
        )
        .await;
    assert!(view.waiting, "the root waits with no timer");
    let copy = wait_of(&state).expect("the root's copy");
    assert_eq!(copy.park, ModelPark::UntilWoken);
    let helper_state = rig.state(helper).await;
    let copied = wait_of(&helper_state).expect("the helper's copy");
    assert_eq!(copied.reason, "the model gateway's limit is reached");
    assert!(rig.rt.view(leaf).await.unwrap().expect("the leaf").waiting);

    rig.rt
        .deliver(run, user_message("the limit is raised").with_id("m-1"))
        .await
        .expect("deliver");
    let done = rig.done(run).await;
    assert_eq!(text_of(&done).as_deref(), Some("all done"));
    let requests = rig.mock.requests();
    assert_eq!(requests.len(), 6);
    assert_eq!(
        texts(&requests[3]),
        texts(&requests[2]),
        "the leaf makes the same call"
    );
    let leaf_state = rig.state(leaf).await;
    assert!(
        leaf_state
            .messages
            .iter()
            .all(|m| m.text() != "the limit is raised"),
        "the words stayed at the root"
    );
    let root = rig.state(run).await;
    assert!(
        root.messages
            .iter()
            .any(|m| m.text() == "the limit is raised")
    );
    rig.stop().await;
}

/// A run parked on its summary call reads the messages that woke it, keeping their words for after
/// the summary: a child, which the runtime looks at every minute, is not stepped again (and its
/// summary not asked for) before its own wake.
#[tokio::test]
async fn a_summary_parked_with_a_message_is_not_stepped_before_its_wake() {
    let rig = family(1200);
    let busy = || refused(503, Some("unavailable"), timer(600));
    rig.mock
        .push_tool_calls(vec![tool_call("c1", "sub", json!({}))])
        .push_tool_calls(vec![tool_call(
            "b0",
            "big",
            json!({"n": 1200, "tag": "b0"}),
        )])
        .push_tool_calls(vec![tool_call(
            "b1",
            "big",
            json!({"n": 1200, "tag": "b1"}),
        )])
        .push_tool_calls(vec![tool_call(
            "b2",
            "big",
            json!({"n": 1200, "tag": "b2"}),
        )])
        .push_tool_calls(vec![tool_call(
            "b3",
            "big",
            json!({"n": 1200, "tag": "b3"}),
        )])
        .push_error(busy())
        .push_error(busy())
        .push_text("SUMMARY")
        .push_text("helper done")
        .push_text("all done")
        .push_text("spare");
    let run = rig.start(None).await;
    let child = adam_runtime::child_run_id(run, "c1");
    let (_, first) = rig.parked(child, waits_on_timer).await;
    assert_eq!(first.model_wait.unwrap().parks, 1);
    rig.rt
        .deliver(child, user_message("one more thing"))
        .await
        .expect("deliver");
    let (_, parked) = rig
        .parked(child, |s| {
            s.model_wait.as_ref().is_some_and(|w| w.parks == 2)
        })
        .await;
    assert_eq!(
        parked
            .deferred
            .iter()
            .map(Message::text)
            .collect::<Vec<_>>(),
        ["one more thing"],
        "the words wait for the summary"
    );
    assert_eq!(rig.rt.view(child).await.unwrap().unwrap().pending_inbox, 0);
    let made = rig.mock.requests().len();
    // The runtime's looks at the child come every minute: none of them steps it.
    for _ in 0..3 {
        rig.clock.advance(Duration::from_secs(61));
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(rig.mock.requests().len(), made, "no call before the wake");
    rig.clock.advance(Duration::from_secs(1200));
    let view = rig.done(run).await;
    assert_eq!(text_of(&view).as_deref(), Some("all done"));
    let child = rig.state(child).await;
    assert!(
        child.messages.iter().any(|m| m.text() == "one more thing"),
        "read after the summary"
    );
    rig.stop().await;
}
