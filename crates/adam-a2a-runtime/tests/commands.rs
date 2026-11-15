//! `commands/v1` on the A2A backend: a message that names a command the agent serves, in its
//! metadata (when the request activated the extension) or as `/name arguments` at the start of its
//! text, is expanded in the request and the expansion is what is stored; a name the agent does not
//! serve is refused before anything is written; a `/word` that is no command is plain text.
//!
//! A real `LlmAgent` on a real `Runtime` over `MemoryStore`, as in `access_tier.rs`; what the run
//! was given is read from the first request the model received.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use a2a::{Message, Part, Role, Task, TaskState};
use adam_a2a::{BackendError, COMMANDS_EXTENSION, Caller, TaskBackend};
use adam_a2a_runtime::{Commands, RuntimeTaskBackend, task_id_for};
use adam_core::{DynStore, MemoryStore, RunId};
use adam_llm_agent::LlmAgent;
use adam_model::{DynModel, Message as ModelMessage, MockModel};
use adam_runtime::{BroadcastSink, Runtime};
use serde_json::{Value, json};
use tokio::sync::oneshot;

fn uniq(prefix: &str) -> String {
    format!("{prefix}-{}", RunId::new())
}

fn commands(review: &str) -> Commands {
    Commands::new([
        ("review", review),
        ("echo", "[$ARGUMENTS]"),
        ("pair", "$2 then $1"),
    ])
}

struct Rig {
    name: String,
    mock: Arc<MockModel>,
    runtime: Runtime,
    events: BroadcastSink,
    backend: RuntimeTaskBackend,
}

impl Rig {
    fn new(commands: Option<Commands>) -> Self {
        let name = uniq("commands");
        let store: DynStore = Arc::new(MemoryStore::new());
        let mock = Arc::new(MockModel::new());
        let model: DynModel = mock.clone();
        let agent = LlmAgent::builder(&name, model, "m").build();
        let events = BroadcastSink::default();
        let runtime = Runtime::builder(store)
            .agent(agent)
            .event_sink(events.clone())
            .poll_interval(Duration::from_millis(10))
            .build();
        let rig = Self {
            backend: RuntimeTaskBackend::new(runtime.clone(), events.clone(), name.clone())
                .with_poll_interval(Duration::from_millis(10)),
            name,
            mock,
            runtime,
            events,
        };
        match commands {
            Some(commands) => rig.with(commands),
            None => rig,
        }
    }

    /// The same runtime and store, a backend serving `commands`.
    fn with(mut self, commands: Commands) -> Self {
        self.backend = self.backend.with_commands(commands);
        self
    }

    /// Another backend over the same runtime: what a restart with an edited folder is.
    fn backend_with(&self, commands: Commands) -> RuntimeTaskBackend {
        RuntimeTaskBackend::new(self.runtime.clone(), self.events.clone(), self.name.clone())
            .with_poll_interval(Duration::from_millis(10))
            .with_commands(commands)
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

    /// Run `task` to its end and say what the model's `n`th request began with.
    async fn first_text(&self, who: &Caller, task: &Task, n: usize) -> ModelMessage {
        let (stop, handle) = self.worker();
        for _ in 0..2000 {
            let got = self.backend.get(who, &task.id).await.unwrap().unwrap();
            if got.status.state == TaskState::Completed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let _ = stop.send(());
        handle.await.unwrap();
        self.mock.requests()[n].messages[0].clone()
    }
}

fn activated(who: &str) -> Caller {
    Caller::new(format!("token-{who}")).with_extensions([COMMANDS_EXTENSION])
}

fn plain(who: &str) -> Caller {
    Caller::new(format!("token-{who}"))
}

fn said(text: &str, id: &str, entry: Option<Value>) -> Message {
    let mut m = Message::new(Role::User, vec![Part::text(text)]);
    m.message_id = id.into();
    m.metadata = entry.map(|e| HashMap::from([(COMMANDS_EXTENSION.to_owned(), e)]));
    m
}

#[tokio::test]
async fn metadata_and_slash_expand_identically() {
    let rig = Rig::new(Some(commands("Review $1 now. Notes: $ARGUMENTS")));
    rig.mock.push_text("one").push_text("two");
    let who = uniq("alice");
    let expected = ModelMessage::user_text("Review acme/app now. Notes: acme/app fast");

    let by_slash = rig
        .backend
        .submit(
            plain(&who),
            said("/review acme/app fast", "m-0", None),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(rig.first_text(&plain(&who), &by_slash, 0).await, expected);

    let by_metadata = rig
        .backend
        .submit(
            activated(&who),
            said(
                "/review acme/app fast",
                "m-1",
                Some(json!({"command": "review", "arguments": "acme/app fast"})),
            ),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        rig.first_text(&activated(&who), &by_metadata, 1).await,
        expected
    );
}

#[tokio::test]
async fn unknown_metadata_command_is_invalid_params_and_writes_nothing() {
    let rig = Rig::new(Some(commands("Review $1")));
    let who = uniq("bob");
    for (id, entry, wanted) in [
        (
            "m-unknown",
            json!({"command": "nope"}),
            "unknown command `nope`",
        ),
        ("m-number", json!({"command": 3}), "`command` is a string"),
        ("m-none", json!({}), "`command` is a string"),
        (
            "m-args",
            json!({"command": "echo", "arguments": ["x"]}),
            "not text",
        ),
        (
            "m-long",
            json!({"command": "echo", "arguments": "x".repeat(4097)}),
            "at most 4096",
        ),
    ] {
        let err = rig
            .backend
            .submit(activated(&who), said("go", id, Some(entry)), None, None)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, BackendError::InvalidParams(m) if m.contains(wanted)),
            "{id}: {err:?}"
        );
        let run = task_id_for(&rig.name, &format!("token-{who}"), None, id);
        assert!(rig.runtime.view(run).await.unwrap().is_none(), "{id}");
    }
}

#[tokio::test]
async fn unknown_slash_is_plain_text() {
    let rig = Rig::new(Some(commands("Review $1")));
    rig.mock.push_text("ok");
    let who = uniq("carol");
    let task = rig
        .backend
        .submit(plain(&who), said("/tmp is full", "m-0", None), None, None)
        .await
        .unwrap();
    assert_eq!(
        rig.first_text(&plain(&who), &task, 0).await,
        ModelMessage::user_text("/tmp is full")
    );
}

#[tokio::test]
async fn a_doubled_slash_is_a_literal_slash() {
    let rig = Rig::new(Some(commands("Review $1")));
    rig.mock.push_text("ok");
    let who = uniq("dan");
    let task = rig
        .backend
        .submit(
            plain(&who),
            said("//review is the command I mean", "m-0", None),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        rig.first_text(&plain(&who), &task, 0).await,
        ModelMessage::user_text("/review is the command I mean")
    );
}

#[tokio::test]
async fn metadata_is_ignored_without_activation() {
    let rig = Rig::new(Some(commands("Review $1")));
    rig.mock.push_text("ok");
    let who = uniq("erin");
    // The request did not activate the extension: the member is not read, not even a name the
    // agent does not serve, and the text is taken as it is.
    let task = rig
        .backend
        .submit(
            plain(&who),
            said("hello", "m-0", Some(json!({"command": "nope"}))),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        rig.first_text(&plain(&who), &task, 0).await,
        ModelMessage::user_text("hello")
    );
}

#[tokio::test]
async fn a_backend_without_commands_reads_none() {
    let rig = Rig::new(None);
    rig.mock.push_text("ok");
    let who = uniq("fay");
    let task = rig
        .backend
        .submit(
            activated(&who),
            said(
                "/review x",
                "m-0",
                Some(json!({"command": "review", "arguments": "x"})),
            ),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        rig.first_text(&activated(&who), &task, 0).await,
        ModelMessage::user_text("/review x")
    );
}

/// The inbound is stored with the expansion in it. A backend over the same store whose folder was
/// edited since (a restart) sees the same message again, and the run still reads the old text.
#[tokio::test]
async fn replay_uses_the_stored_expansion() {
    let rig = Rig::new(Some(commands("OLD $ARGUMENTS")));
    rig.mock.push_text("ok");
    let who = uniq("gus");
    let message = said("/review the thing", "m-0", None);
    let task = rig
        .backend
        .submit(plain(&who), message.clone(), None, None)
        .await
        .unwrap();

    // The folder is edited and the process restarts before the run is stepped; the same message
    // (same `messageId`) arrives again at the new backend.
    let edited = rig.backend_with(commands("NEW $ARGUMENTS"));
    let again = edited
        .submit(plain(&who), message, None, None)
        .await
        .unwrap();
    assert_eq!(again.id, task.id);

    assert_eq!(
        rig.first_text(&plain(&who), &task, 0).await,
        ModelMessage::user_text("OLD the thing")
    );
    // A new message of the edited backend reads the new template.
    rig.mock.push_text("ok");
    let fresh = edited
        .submit(plain(&who), said("/review other", "m-1", None), None, None)
        .await
        .unwrap();
    assert_eq!(
        rig.first_text(&plain(&who), &fresh, 1).await,
        ModelMessage::user_text("NEW other")
    );
}

#[tokio::test]
async fn an_argument_that_names_a_placeholder_is_not_expanded_again() {
    let rig = Rig::new(Some(commands("Review $1")));
    rig.mock.push_text("ok");
    let who = uniq("hal");
    let task = rig
        .backend
        .submit(
            plain(&who),
            said("/pair $ARGUMENTS $2", "m-0", None),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        rig.first_text(&plain(&who), &task, 0).await,
        ModelMessage::user_text("$2 then $ARGUMENTS")
    );
}

#[tokio::test]
async fn arguments_over_the_limit_and_expansions_over_the_limit_are_refused() {
    let big = Commands::new([(
        "big",
        "$ARGUMENTS $ARGUMENTS $ARGUMENTS $ARGUMENTS $ARGUMENTS $ARGUMENTS $ARGUMENTS $ARGUMENTS $ARGUMENTS",
    )]);
    let rig = Rig::new(Some(big));
    let who = uniq("ivy");
    for (id, text, wanted) in [
        (
            "m-args",
            format!("/big {}", "x".repeat(4097)),
            "at most 4096",
        ),
        (
            "m-out",
            format!("/big {}", "x".repeat(4096)),
            "more than 32768",
        ),
    ] {
        let err = rig
            .backend
            .submit(plain(&who), said(&text, id, None), None, None)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, BackendError::InvalidParams(m) if m.contains(wanted)),
            "{id}: {err:?}"
        );
        let run = task_id_for(&rig.name, &format!("token-{who}"), None, id);
        assert!(rig.runtime.view(run).await.unwrap().is_none(), "{id}");
    }
}

#[tokio::test]
async fn quoted_arguments_split_as_documented() {
    let rig = Rig::new(Some(commands("Review $1")));
    rig.mock.push_text("ok");
    let who = uniq("jon");
    let task = rig
        .backend
        .submit(
            plain(&who),
            said(r#"/pair "acme app" fast"#, "m-0", None),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        rig.first_text(&plain(&who), &task, 0).await,
        ModelMessage::user_text("fast then acme app")
    );
}
