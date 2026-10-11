//! What the coder takes from the person's words, through a real `CoderAgent` on a `Runtime`: the
//! repositories and the pull requests they named, the merge they asked for, the bare names, and what a
//! run inherits from the one it continues (`RunNotes::hear`, `CoderAgent::record_grants`).
//!
//! The rule under test is that only what the person **typed** counts. A form's answer reaches the run
//! as `The person answered through the interface:` and a line per question, whose ids and option values
//! are what the *model* wrote when it drew the form: a click names nothing, asks for nothing and says
//! no bare name. The answers here are made by the A2A layer itself (`vymalo_inbound`) from A2UI actions.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use a2a::{Message as A2aMessage, Part, Role};
use adam_a2a::{A2UI_MEDIA_TYPE, AccessTier};
use adam_a2a_runtime::{CONTEXT_ACCESS_TIER, vymalo_inbound};
use adam_coder::tools::notes::{PullRef, PushedBranch, RunNotes};
use adam_coder::{AGENT_NAME, CoderAgent};
use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_model::{DynModel, Message, MockModel, ToolCall};
use adam_runtime::{Inbound, Runtime};
use adam_workspace::{CodeHost, DynCodeHost, MemoryCodeHost, NewPullRequest, RepoRef};
use common::Fixture;
use serde_json::{Value, json};

const WIDGETS: &str = "github.com/acme/widgets";
const GADGETS: &str = "github.com/acme/gadgets";

fn pull(repo: &str, number: u64) -> PullRef {
    PullRef {
        repo: Some(repo.to_owned()),
        number,
    }
}

fn words(text: &str) -> Inbound {
    Inbound::new("message", json!({ "text": text }))
}

/// Words said under the `auto` tier, where the merge tool is shown and may be called.
fn words_under_auto(text: &str) -> Inbound {
    Inbound::new(
        "message",
        json!({
            "text": text,
            "context": { CONTEXT_ACCESS_TIER: { "tier": AccessTier::Auto.as_str() } },
        }),
    )
}

/// A code host that holds pull request 1 of `octo/widgets` and records what it is asked.
async fn with_a_pull_request() -> (Fixture, Arc<MemoryCodeHost>) {
    let host = Arc::new(MemoryCodeHost::new());
    host.open_pull_request(NewPullRequest {
        repo: RepoRef::new("https://github.com/octo/widgets", "main"),
        head: "agent/x".into(),
        title: "t".into(),
        body: "b".into(),
        draft: false,
    })
    .await
    .unwrap();
    let dyn_host: DynCodeHost = host.clone();
    (Fixture::new("hello\n").await.hosted_by(dyn_host), host)
}

/// The person acts on a form the model drew: the answer as the A2A layer writes it.
fn answered(answers: Value, note: Option<&str>) -> Inbound {
    let mut context = json!({ "answers": answers });
    if let Some(note) = note {
        context["note"] = json!(note);
    }
    let action = json!({
        "name": "answer", "surfaceId": "ask-1", "sourceComponentId": "root",
        "timestamp": "2026-10-11T10:00:00Z", "context": context,
    });
    let mut part = Part::data(json!([{ "version": "v0.9.1", "action": action }]))
        .with_media_type(A2UI_MEDIA_TYPE);
    part.metadata = Some(HashMap::from([(
        "mimeType".to_owned(),
        json!(A2UI_MEDIA_TYPE),
    )]));
    let message = A2aMessage::new(Role::User, vec![part]);
    vymalo_inbound(&message).expect("an answer reads as text")
}

/// A coder on a `Runtime`, one run of it, and the model's script.
struct Session {
    mock: Arc<MockModel>,
    rt: Runtime,
    fx: Fixture,
    run: RunId,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    worker: Option<tokio::task::JoinHandle<()>>,
}

impl Session {
    async fn start(fx: Fixture, script: impl FnOnce(&MockModel), first: Inbound) -> Self {
        let mock = Arc::new(MockModel::new());
        script(&mock);
        let model: DynModel = mock.clone();
        let agent = CoderAgent::new(model, "test-model", fx.env.clone());
        let store: DynStore = Arc::new(MemoryStore::new());
        let rt = Runtime::builder(store)
            .agent(agent)
            .poll_interval(Duration::from_millis(20))
            .build();
        let (stop, rx) = tokio::sync::oneshot::channel::<()>();
        let worker = {
            let rt = rt.clone();
            tokio::spawn(async move {
                rt.run_worker(async {
                    let _ = rx.await;
                })
                .await
                .unwrap();
            })
        };
        let run = rt.start(AGENT_NAME, first, None).await.unwrap();
        Self {
            mock,
            rt,
            fx,
            run,
            stop: Some(stop),
            worker: Some(worker),
        }
    }

    /// Wait until the run is done or waits for the person.
    async fn settle(&self, run: RunId) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let view = self.rt.view(run).await.unwrap().expect("run exists");
            match view.status {
                RunStatus::Done => return,
                RunStatus::Parked if view.waiting => return,
                RunStatus::Failed => panic!("the run failed: {view:#?}"),
                _ => {}
            }
            assert!(Instant::now() < deadline, "timed out: {view:#?}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// What the model was told a call returned, in the newest request that has it.
    fn result_of(&self, id: &str) -> String {
        self.mock
            .requests()
            .iter()
            .rev()
            .flat_map(|request| request.messages.iter())
            .find_map(|message| match message {
                Message::Tool {
                    call_id, content, ..
                } if call_id == id => Some(content.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no result for {id}"))
    }

    async fn notes_of(&self, run: RunId) -> RunNotes {
        self.fx.env.notes.load(&run.to_string()).await.unwrap()
    }

    /// The person says or does `inbound` while the run waits, and the run goes on to its end.
    async fn then(&self, inbound: Inbound) -> RunNotes {
        self.rt.deliver(self.run, inbound).await.unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let view = self.rt.view(self.run).await.unwrap().unwrap();
            match view.status {
                RunStatus::Done => break,
                RunStatus::Failed => panic!("the run failed: {view:#?}"),
                _ => {}
            }
            assert!(Instant::now() < deadline, "timed out: {view:#?}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        self.notes_of(self.run).await
    }

    async fn finish(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            worker.await.unwrap();
        }
    }
}

fn call(id: &str, name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
    }
}

/// A model that asks the person one thing and then says it is done.
fn asks_then_ends(model: &MockModel) {
    model
        .push_tool_calls(vec![call(
            "q1",
            "ask_user",
            json!({"question": "Which one?"}),
        )])
        .push_text("Done.");
}

#[tokio::test]
async fn what_the_person_says_names_the_repositories_and_pull_requests_they_wrote() {
    let fx = Fixture::new("hello\n").await;
    let s = Session::start(
        fx,
        |m| {
            m.push_text("Understood.");
        },
        words("Please merge https://github.com/acme/widgets/pull/7, and look at acme/gadgets#3"),
    )
    .await;
    s.settle(s.run).await;
    let notes = s.notes_of(s.run).await;
    assert_eq!(notes.named_repos, [WIDGETS, GADGETS]);
    assert_eq!(notes.named_pulls, [pull(WIDGETS, 7), pull(GADGETS, 3)]);
    // A message that says to merge asks about each pull request it names.
    assert_eq!(notes.merge_asks, [pull(WIDGETS, 7), pull(GADGETS, 3)]);
    s.finish().await;
}

/// A form's click is what the model wrote, not what the person said: nothing in the ids and values
/// below names a repository or a pull request, asks for a merge or is a bare name.
#[tokio::test]
async fn a_form_the_model_wrote_names_nothing_and_asks_for_nothing() {
    let s = Session::start(
        Fixture::new("hello\n").await,
        asks_then_ends,
        words("Look around, please"),
    )
    .await;
    s.settle(s.run).await;
    let notes = s
        .then(answered(
            json!([
                // The ids and the values are the model's. Plain or quoted, they say nothing.
                {"id": "merge_acme_widgets_7", "values": ["https://github.com/acme/widgets/pull/7"]},
                {"id": "pull/7", "values": ["merge #7 now", "acme/widgets#7", "forged"]},
                {"id": "repository", "values": ["acme/widgets"]},
            ]),
            None,
        ))
        .await;
    assert!(notes.named_repos.is_empty(), "{:?}", notes.named_repos);
    assert!(notes.named_pulls.is_empty(), "{:?}", notes.named_pulls);
    assert!(notes.merge_asks.is_empty(), "{:?}", notes.merge_asks);
    // The bare names are the words of the person's last message that has any, not the model's.
    assert!(notes.said("look") && !notes.said("forged") && !notes.said("widgets"));
    s.finish().await;
}

/// What the person typed into a form is theirs: an "other", a typed question and the note.
#[tokio::test]
async fn what_the_person_types_into_a_form_counts() {
    let s = Session::start(
        Fixture::new("hello\n").await,
        asks_then_ends,
        words("Look around, please"),
    )
    .await;
    s.settle(s.run).await;
    let notes = s
        .then(answered(
            json!([
                {"id": "forged", "values": ["acme/gadgets"], "other": "https://github.com/acme/widgets/pull/9"}
            ]),
            Some("please merge it, thanks"),
        ))
        .await;
    assert_eq!(notes.named_repos, [WIDGETS]);
    assert_eq!(notes.named_pulls, [pull(WIDGETS, 9)]);
    // The note says to merge and the other names the pull request: both are theirs.
    assert_eq!(notes.merge_asks, [pull(WIDGETS, 9)]);
    s.finish().await;

    // Without the note it names the pull request and does not ask for the merge.
    let s = Session::start(
        Fixture::new("hello\n").await,
        asks_then_ends,
        words("Look around, please"),
    )
    .await;
    s.settle(s.run).await;
    let notes = s
        .then(answered(
            json!([{"id": "forged", "values": [], "other": "https://github.com/acme/widgets/pull/9"}]),
            None,
        ))
        .await;
    assert_eq!(notes.named_pulls, [pull(WIDGETS, 9)]);
    assert!(notes.merge_asks.is_empty(), "{:?}", notes.merge_asks);
    s.finish().await;
}

/// The person's newest words include their latest answer: "merge #7", the model asks to be sure, and
/// "no, wait" takes the ask away, as does a click that says nothing.
#[tokio::test]
async fn an_answer_that_names_nothing_takes_an_earlier_ask_away() {
    let first = "In https://github.com/acme/widgets merge #7";
    for (label, answer) in [
        (
            "typed no",
            answered(
                json!([{"id": "sure", "values": ["no"], "other": "no, wait"}]),
                None,
            ),
        ),
        (
            "a click",
            answered(json!([{"id": "sure", "values": ["no"]}]), None),
        ),
        ("typed in a reply", words("no, wait")),
    ] {
        let s = Session::start(Fixture::new("hello\n").await, asks_then_ends, words(first)).await;
        s.settle(s.run).await;
        let before = s.notes_of(s.run).await;
        assert_eq!(
            before.merge_asks,
            [pull(WIDGETS, 7)],
            "{label}: the first message asks"
        );
        assert_eq!(before.named_pulls, [pull(WIDGETS, 7)], "{label}");
        let after = s.then(answer).await;
        assert!(
            after.merge_asks.is_empty(),
            "{label}: {:?}",
            after.merge_asks
        );
        // What was named stays named: it is the person's, and no one took it back.
        assert_eq!(after.named_pulls, [pull(WIDGETS, 7)], "{label}");
        s.finish().await;
    }
    // And an answer that says it again puts it back.
    let s = Session::start(Fixture::new("hello\n").await, asks_then_ends, words(first)).await;
    s.settle(s.run).await;
    let after = s
        .then(answered(
            json!([{"id": "sure", "values": ["yes"], "other": "yes, merge #7"}]),
            None,
        ))
        .await;
    assert_eq!(after.merge_asks, [pull(WIDGETS, 7)]);
    s.finish().await;
}

/// A bare `#N` is the pull request of the one repository in play, in the newest words; with two
/// repositories it names none.
#[tokio::test]
async fn a_bare_number_is_the_pull_request_of_the_one_repository_in_play() {
    for (said, named, asks) in [
        // One repository in the same message.
        (
            "In acme/widgets merge #7 and PR 8",
            vec![pull(WIDGETS, 7), pull(WIDGETS, 8)],
            vec![pull(WIDGETS, 7), pull(WIDGETS, 8)],
        ),
        // Two: a bare number names nothing, a written one does.
        (
            "Compare acme/widgets with acme/gadgets, merge #7",
            vec![],
            vec![],
        ),
        (
            "Compare acme/widgets with acme/gadgets, merge acme/gadgets#7",
            vec![pull(GADGETS, 7)],
            vec![pull(GADGETS, 7)],
        ),
        // None: there is no repository to bind it to.
        ("merge #7", vec![], vec![]),
    ] {
        let s = Session::start(
            Fixture::new("hello\n").await,
            |m| {
                m.push_text("Understood.");
            },
            words(said),
        )
        .await;
        s.settle(s.run).await;
        let notes = s.notes_of(s.run).await;
        assert_eq!(notes.named_pulls, named, "{said}");
        assert_eq!(notes.merge_asks, asks, "{said}");
        s.finish().await;
    }
    // A number of an older message does not become the pull request of a repository named later.
    let s = Session::start(
        Fixture::new("hello\n").await,
        asks_then_ends,
        words("merge #7"),
    )
    .await;
    s.settle(s.run).await;
    let notes = s.then(words("the repository is acme/widgets")).await;
    assert!(notes.named_pulls.is_empty(), "{:?}", notes.named_pulls);
    s.finish().await;
}

/// A bare name is a standalone word of the newest words, never a piece of a URL, and a form's click
/// is not a word of the person's.
#[tokio::test]
async fn bare_names_are_the_standalone_words_the_person_typed_last() {
    let s = Session::start(
        Fixture::new("hello\n").await,
        asks_then_ends,
        words("fix widgets, see https://github.com/stranger/sprockets/issues/3"),
    )
    .await;
    s.settle(s.run).await;
    let before = s.notes_of(s.run).await;
    assert!(before.said("widgets") && before.said("fix"));
    assert!(!before.said("sprockets") && !before.said("stranger"));
    // A click that names a repository by its value is not a word of theirs; the words stand.
    let after = s
        .then(answered(
            json!([{"id": "which", "values": ["gadgets", "sprockets"]}]),
            None,
        ))
        .await;
    assert!(after.said("widgets") && !after.said("gadgets") && !after.said("sprockets"));
    s.finish().await;
}

/// What the tools wrote about the conversation's work follows a task to the one that continues it.
#[tokio::test]
async fn a_task_that_continues_another_inherits_its_pushed_commits_red_pulls_and_verified_commits()
{
    let fx = Fixture::new("hello\n").await;
    let first = Session::start(
        fx,
        |m| {
            m.push_text("First done.").push_text("Second done.");
        },
        words("work on acme/widgets"),
    )
    .await;
    first.settle(first.run).await;
    // What `commit_and_push` and `open_pull_request` wrote in the first task.
    let run1 = first.run.to_string();
    let mut notes = first.notes_of(first.run).await;
    notes.name_pushed_branches([PushedBranch {
        repo: WIDGETS.to_owned(),
        branch: "agent/x".to_owned(),
        base: Some("main".to_owned()),
        shas: vec!["c0ffee".to_owned(), "decaf".to_owned()],
    }]);
    notes.name_red_pulls([pull(WIDGETS, 7)]);
    notes.verify_sha("decaf");
    first.fx.env.notes.save(&run1, &notes).await.unwrap();

    // The next task of the conversation names nothing, and starts from the first.
    let second = first
        .rt
        .start_continuing(AGENT_NAME, words("go on"), None, first.run)
        .await
        .unwrap();
    first.settle(second).await;
    let inherited = first.notes_of(second).await;
    assert_eq!(inherited.pushed_branches, notes.pushed_branches);
    assert!(inherited.pushed_commit(WIDGETS, "agent/x", "decaf"));
    assert!(!inherited.pushed_commit(WIDGETS, "agent/x", "badc0de"));
    assert!(inherited.red_accepted(WIDGETS, 7));
    assert!(inherited.is_verified("decaf") && !inherited.is_verified("c0ffee"));
    // And what the person said before is not said again: a new task has its own words.
    assert!(inherited.said("go") && !inherited.said("work"));
    first.finish().await;
}

const MERGE_1: &str = "merge octo/widgets#1";

fn merges_it(model: &MockModel) {
    model.push_tool_calls(vec![call(
        "m1",
        "merge_pull_request",
        json!({"repo": "octo/widgets", "number": 1}),
    )]);
}

/// A failed `ask_user` repeats the model's own values in its words (`the default ... is not one of
/// its options`): they are the model's, so they name nothing and the merge stays refused.
#[tokio::test]
async fn the_error_of_a_form_repeats_the_model_and_names_nothing() {
    let (fx, host) = with_a_pull_request().await;
    let s = Session::start(
        fx,
        |m| {
            m.push_tool_calls(vec![call(
                "q1",
                "ask_user",
                json!({"question": "Sure?", "choices": [
                    {"id": "q", "question": "Merge?", "options": ["yes", "no"], "default": MERGE_1}
                ]}),
            )]);
            merges_it(m);
            m.push_text("I may not.");
        },
        words_under_auto("Look around, please"),
    )
    .await;
    s.settle(s.run).await;
    // The premise: the error did carry the model's words back to the model.
    assert!(s.result_of("q1").contains(MERGE_1), "{}", s.result_of("q1"));
    let notes = s.notes_of(s.run).await;
    assert!(notes.named_repos.is_empty(), "{:?}", notes.named_repos);
    assert!(notes.named_pulls.is_empty(), "{:?}", notes.named_pulls);
    assert!(notes.merge_asks.is_empty(), "{:?}", notes.merge_asks);
    assert!(
        s.result_of("m1").contains("not one to merge"),
        "{}",
        s.result_of("m1")
    );
    assert!(host.merges().is_empty(), "the host was never asked");
    s.finish().await;
}

/// The model fills a typed question in for the person (`default`), and the web sends it back on a
/// submit that touched nothing: those words are the model's. What the person changes is theirs.
#[tokio::test]
async fn a_typed_question_the_person_left_as_the_model_wrote_it_names_nothing() {
    let asks = |m: &MockModel| {
        m.push_tool_calls(vec![call(
            "q1",
            "ask_user",
            json!({"question": "Anything else?", "choices": [
                {"id": "why", "question": "Why?", "type": "text", "default": MERGE_1}
            ]}),
        )]);
        merges_it(m);
        m.push_text("Done.");
    };
    // Untouched: the same words, back.
    let (fx, host) = with_a_pull_request().await;
    let s = Session::start(fx, asks, words_under_auto("Look around, please")).await;
    s.settle(s.run).await;
    let notes = s
        .then(answered(
            json!([{"id": "why", "values": [], "text": MERGE_1}]),
            None,
        ))
        .await;
    assert!(notes.named_repos.is_empty(), "{:?}", notes.named_repos);
    assert!(notes.named_pulls.is_empty(), "{:?}", notes.named_pulls);
    assert!(notes.merge_asks.is_empty(), "{:?}", notes.merge_asks);
    assert!(
        s.result_of("m1").contains("not one to merge"),
        "{}",
        s.result_of("m1")
    );
    assert!(host.merges().is_empty(), "the host was never asked");
    s.finish().await;

    // Edited: what the person typed after the model's words is theirs.
    let (fx, host) = with_a_pull_request().await;
    let s = Session::start(fx, asks, words_under_auto("Look around, please")).await;
    s.settle(s.run).await;
    let notes = s
        .then(answered(
            json!([{"id": "why", "values": [], "text": format!("{MERGE_1}, yes please")}]),
            None,
        ))
        .await;
    assert_eq!(notes.named_pulls, [pull("github.com/octo/widgets", 1)]);
    assert_eq!(notes.merge_asks, [pull("github.com/octo/widgets", 1)]);
    assert_eq!(host.merges().len(), 1, "{}", s.result_of("m1"));
    s.finish().await;
}

/// The answers of an earlier form are paired with their own call: after a second form, the first
/// one's untouched default is still the model's.
#[tokio::test]
async fn a_default_is_paired_with_the_form_that_wrote_it() {
    let (fx, host) = with_a_pull_request().await;
    let s = Session::start(
        fx,
        |m| {
            m.push_tool_calls(vec![call(
                "q1",
                "ask_user",
                json!({"question": "First?", "choices": [
                    {"id": "why", "question": "Why?", "type": "text", "default": MERGE_1}
                ]}),
            )]);
            m.push_tool_calls(vec![call(
                "q2",
                "ask_user",
                json!({"question": "Second?", "choices": [
                    {"id": "why", "question": "Why?", "type": "text", "default": "something else"}
                ]}),
            )]);
            merges_it(m);
            m.push_text("Done.");
        },
        words_under_auto("Look around, please"),
    )
    .await;
    s.settle(s.run).await;
    let untouched = |text: &str| answered(json!([{"id": "why", "values": [], "text": text}]), None);
    s.rt.deliver(s.run, untouched(MERGE_1)).await.unwrap();
    // The run asks again and waits.
    let deadline = Instant::now() + Duration::from_secs(30);
    while s.mock.remaining() > 2 {
        assert!(Instant::now() < deadline, "the second form never came");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    s.settle(s.run).await;
    let notes = s.then(untouched("something else")).await;
    assert!(notes.named_repos.is_empty(), "{:?}", notes.named_repos);
    assert!(notes.merge_asks.is_empty(), "{:?}", notes.merge_asks);
    assert!(host.merges().is_empty(), "the host was never asked");
    s.finish().await;
}
