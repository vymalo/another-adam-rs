//! Dev reload (feature `dev`): the agents of a directory, swapped when a file changes. Run end to
//! end on `MockModel` through the runtime: a temp dir is edited and `LiveAssembly::reload` (the
//! same code the watcher runs) loads it; one test uses a real `notify` watcher. The memory store
//! always, PostgreSQL when `ADAM_TEST_POSTGRES_URL` is set, for the tests where a run outlives a
//! reload.
#![cfg(feature = "dev")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::io::Write;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use a2a::{Message, Part, Role, Task, TaskState, TaskStatus};
use adam_a2a::{
    A2aServer, AgentCardConfig, AuthConfig, BackendError, Caller, InMemoryBackend, TaskBackend,
    TaskEvent,
};
use adam_assembly::{Error, LiveAssembly, LiveBuilder, ReloadError, ToolChange};
use adam_core::{DynStore, MemoryStore, RunId, RunStatus, Store};
use adam_llm_agent::{
    FnTool, StateKey, Tool, ToolCtx, ToolError, ToolOutput, ToolSet, user_message,
};
use adam_model::{MockModel, ModelRequest, ToolCall, ToolSpec};
use adam_runtime::{Runtime, RuntimeBuilder};
use async_trait::async_trait;
use axum::extract::{Request, State};
use axum::middleware::{self, Next};
use common::{instructions, spawn_worker, wait_done, wait_for, write};
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::net::TcpListener;

const FILE: &str = "agent/instructions.md";

// --- harness -------------------------------------------------------------------------------

async fn stores() -> Vec<(&'static str, DynStore)> {
    let mut all: Vec<(&'static str, DynStore)> = vec![("memory", Arc::new(MemoryStore::new()))];
    if let Some(url) = adam_core::testing::test_env("ADAM_TEST_POSTGRES_URL") {
        let store = adam_store_postgres::PgStore::connect(&url)
            .await
            .expect("connect to postgres");
        store.migrate().await.expect("migrate");
        all.push(("postgres", Arc::new(store)));
    }
    all
}

/// A name no other test (or run of this test on a shared database) uses.
fn uniq(prefix: &str) -> String {
    format!("{prefix}-{}", RunId::new().0.simple())
}

fn call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: json!({}),
    }
}

fn tool_names(request: &ModelRequest) -> Vec<&str> {
    request.tools.iter().map(|t| t.name.as_str()).collect()
}

/// A tool that asks the user a question: the run parks until it is answered.
fn asking(name: &str) -> FnTool {
    FnTool::raw(
        name,
        "Ask the user.",
        json!({"type": "object", "properties": {}}),
        |_ctx, _args| async move {
            Err::<ToolOutput, _>(ToolError::NeedsInput {
                question: "which one?".into(),
            })
        },
    )
}

/// A tool that counts its calls.
fn counting(name: &str, calls: &Arc<AtomicUsize>) -> FnTool {
    let calls = Arc::clone(calls);
    FnTool::raw(
        name,
        format!("the {name} tool"),
        json!({"type": "object", "properties": {}}),
        move |_ctx, _args| {
            calls.fetch_add(1, SeqCst);
            async move { Ok::<_, ToolError>(ToolOutput::text("counted")) }
        },
    )
}

/// The root agent `name` with this frontmatter (after its name) and prompt.
fn agent_file(name: &str, frontmatter: &str, prompt: &str) -> String {
    let extra = if frontmatter.is_empty() {
        String::new()
    } else {
        format!("\n{frontmatter}")
    };
    instructions(&format!("name: {name}{extra}"), prompt)
}

fn edit(root: &Path, file: &str, text: &str) {
    write(root, &[(file, text)]);
}

fn builder(root: &Path, model: &Arc<MockModel>) -> LiveBuilder {
    LiveAssembly::builder(root, model.clone(), "alias")
}

fn runtime(live: &LiveAssembly, store: &DynStore) -> Runtime {
    runtime_with(live, Runtime::builder(store.clone()))
}

fn runtime_with(live: &LiveAssembly, builder: RuntimeBuilder) -> Runtime {
    live.register(builder)
        .poll_interval(Duration::from_millis(20))
        .build()
}

/// Wait until the run is parked (its question is out).
async fn wait_parked(rt: &Runtime, run: RunId) {
    wait_for("the run to park", || async {
        let view = rt.view(run).await.unwrap().expect("run exists");
        assert_ne!(view.status, RunStatus::Failed, "{view:#?}");
        (view.status == RunStatus::Parked).then_some(())
    })
    .await;
}

/// Log lines of the test's thread, as text.
#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl Write for Logs {
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

impl Logs {
    fn capture() -> (Self, tracing::subscriber::DefaultGuard) {
        let logs = Self::default();
        let guard = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::TRACE)
                .with_ansi(false)
                .with_writer(logs.clone())
                .finish(),
        );
        (logs, guard)
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

// --- an edit reaches the next step ---------------------------------------------------------

#[tokio::test]
async fn an_edit_reaches_the_next_step_of_a_running_run() {
    for (backend, store) in stores().await {
        let name = uniq("helper");
        let dir = tempfile::tempdir().unwrap();
        edit(
            dir.path(),
            FILE,
            &agent_file(&name, "tools: [ask]", "Old prompt."),
        );
        let model = Arc::new(MockModel::new());
        model
            .push_tool_calls(vec![call("c1", "ask")])
            .push_text("done");
        let live = builder(dir.path(), &model)
            .tools(ToolSet::new().tool(asking("ask")))
            .load()
            .unwrap();
        let rt = runtime(&live, &store);
        let worker = spawn_worker(&rt);

        let run = rt.start(&name, user_message("go"), None).await.unwrap();
        wait_parked(&rt, run).await;
        assert_eq!(model.requests()[0].system.as_deref(), Some("Old prompt."));

        // The run is in the middle of its conversation. The file changes, and the run's next step
        // (after the answer) is made with the new text.
        edit(
            dir.path(),
            FILE,
            &agent_file(&name, "tools: [ask]", "New prompt."),
        );
        let reloaded = live.reload().unwrap();
        assert_eq!(reloaded.generation, 2, "{backend}");
        assert_eq!(reloaded.changed, std::slice::from_ref(&name), "{backend}");
        assert!(reloaded.tool_changes.is_empty(), "{backend}");
        assert_eq!(live.info()[0].prompt, "New prompt.");

        rt.deliver(run, user_message("the second one"))
            .await
            .unwrap();
        wait_done(&rt, run).await;
        worker.stop().await;
        let requests = model.requests();
        assert_eq!(requests.len(), 2, "{backend}");
        assert_eq!(
            requests[1].system.as_deref(),
            Some("New prompt."),
            "{backend}"
        );
        assert_eq!(live.runs_on_previous_tools(), 0, "{backend}");
    }
}

/// The stable agent the runtime holds under a dev reload is a wrapper, and a wrapper that only
/// delegated `init` would silently start every continued run from nothing.
#[tokio::test]
async fn a_run_that_continues_another_carries_the_conversation_through_the_live_agent() {
    for (backend, store) in stores().await {
        let name = uniq("helper");
        let dir = tempfile::tempdir().unwrap();
        edit(dir.path(), FILE, &agent_file(&name, "", "A prompt."));
        let model = Arc::new(MockModel::new());
        model.push_text("answer one").push_text("answer two");
        let live = builder(dir.path(), &model).load().unwrap();
        let rt = runtime(&live, &store);
        let worker = spawn_worker(&rt);

        let first = rt
            .start(&name, user_message("first task"), None)
            .await
            .unwrap();
        wait_done(&rt, first).await;
        let second = RunId::new();
        assert!(
            rt.start_with_id_continuing(second, &name, user_message("second task"), None, first)
                .await
                .unwrap(),
            "{backend}"
        );
        wait_done(&rt, second).await;
        worker.stop().await;

        let requests = model.requests();
        assert_eq!(requests.len(), 2, "{backend}");
        let said: Vec<String> = requests[1].messages.iter().map(|m| m.text()).collect();
        assert_eq!(
            said,
            ["first task", "answer one", "second task"],
            "{backend}"
        );
    }
}

#[tokio::test]
async fn a_reload_of_unchanged_files_swaps_and_says_nothing_changed() {
    let name = uniq("helper");
    let dir = tempfile::tempdir().unwrap();
    edit(dir.path(), FILE, &agent_file(&name, "", "Same."));
    let model = Arc::new(MockModel::new());
    let live = builder(dir.path(), &model).load().unwrap();
    assert_eq!(live.generation(), 1);
    let reloaded = live.reload().unwrap();
    assert_eq!(reloaded.generation, 2);
    assert!(reloaded.changed.is_empty());
    assert!(reloaded.retired.is_empty());
    assert!(live.last_error().is_none());
}

// --- an invalid edit ------------------------------------------------------------------------

#[tokio::test]
async fn an_invalid_edit_keeps_the_old_version_logs_and_exposes_the_diagnostic() {
    let (logs, _guard) = Logs::capture();
    let name = uniq("helper");
    let store: DynStore = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    edit(
        dir.path(),
        FILE,
        &agent_file(&name, "tools: [ask]", "Old prompt."),
    );
    let model = Arc::new(MockModel::new());
    model
        .push_tool_calls(vec![call("c1", "ask")])
        .push_tool_calls(vec![call("c2", "ask")])
        .push_text("done");
    let live = builder(dir.path(), &model)
        .tools(ToolSet::new().tool(asking("ask")))
        .load()
        .unwrap();
    let rt = runtime(&live, &store);
    let worker = spawn_worker(&rt);
    let run = rt.start(&name, user_message("go"), None).await.unwrap();
    wait_parked(&rt, run).await;

    // The frontmatter is not closed: the loader's diagnostic, with the file.
    edit(
        dir.path(),
        FILE,
        "---\nname: broken\nNew prompt, no closing line.\n",
    );
    let error = live.reload().unwrap_err();
    assert!(
        matches!(&*error, ReloadError::Load(Error::Manifest(_))),
        "{error}"
    );
    let diagnostics = error.diagnostics();
    assert!(!diagnostics.is_empty(), "{error}");
    assert!(
        diagnostics[0].to_string().contains("agent/instructions.md"),
        "{}",
        diagnostics[0]
    );
    assert_eq!(live.generation(), 1);
    assert_eq!(live.info()[0].prompt, "Old prompt.");
    let kept = live.last_error().expect("the error is kept");
    assert_eq!(kept.to_string(), error.to_string());
    let text = logs.text();
    assert!(text.contains("agent/instructions.md"), "{text}");
    assert!(
        text.contains("reload refused, keeping the last good version"),
        "{text}"
    );

    // The run goes on with the old version.
    rt.deliver(run, user_message("first answer")).await.unwrap();
    wait_for("the second question", || async {
        (model.requests().len() == 2).then_some(())
    })
    .await;
    wait_parked(&rt, run).await;
    assert_eq!(model.requests()[1].system.as_deref(), Some("Old prompt."));

    // A file that reads but does not bind (a tool nobody registered) is refused the same way,
    // with the bind error.
    edit(
        dir.path(),
        FILE,
        &agent_file(&name, "tools: [aks]", "Newer prompt."),
    );
    let error = live.reload().unwrap_err();
    let ReloadError::Load(Error::UnknownTool {
        tool, available, ..
    }) = &*error
    else {
        panic!("wrong error: {error}");
    };
    assert_eq!(tool, "aks");
    assert_eq!(available, &["ask"]);
    assert!(error.diagnostics().is_empty());
    assert_eq!(live.generation(), 1);

    // The fix applies, and clears the error.
    edit(
        dir.path(),
        FILE,
        &agent_file(&name, "tools: [ask]", "Fixed prompt."),
    );
    let reloaded = live.reload().unwrap();
    assert_eq!(reloaded.generation, 2);
    assert!(live.last_error().is_none());
    rt.deliver(run, user_message("second answer"))
        .await
        .unwrap();
    wait_done(&rt, run).await;
    worker.stop().await;
    assert_eq!(model.requests()[2].system.as_deref(), Some("Fixed prompt."));
}

#[tokio::test]
async fn a_bad_first_load_is_a_startup_error() {
    let dir = tempfile::tempdir().unwrap();
    edit(dir.path(), FILE, "---\nname: broken\nno closing line\n");
    let error = builder(dir.path(), &Arc::new(MockModel::new()))
        .load()
        .unwrap_err();
    assert!(matches!(error, Error::Manifest(_)), "{error}");
    // A directory that is not there is one too.
    let error = builder(&dir.path().join("nowhere"), &Arc::new(MockModel::new()))
        .load()
        .unwrap_err();
    assert!(matches!(error, Error::Manifest(_)), "{error}");
}

// --- the tool set ---------------------------------------------------------------------------

#[tokio::test]
async fn a_changed_tool_set_applies_to_runs_that_start_later() {
    for (backend, store) in stores().await {
        let name = uniq("helper");
        let dir = tempfile::tempdir().unwrap();
        edit(
            dir.path(),
            FILE,
            &agent_file(&name, "tools: [ask, later]", "Old prompt."),
        );
        let later_calls = Arc::new(AtomicUsize::new(0));
        let tools = ToolSet::new()
            .tool(asking("ask"))
            .tool(counting("later", &later_calls));
        let model = Arc::new(MockModel::new());
        // The first run asks, and in the same turn calls `later`, which it is owed after the answer.
        model.push_tool_calls(vec![call("c1", "ask"), call("c2", "later")]);
        let live = builder(dir.path(), &model).tools(tools).load().unwrap();
        let rt = runtime(&live, &store);
        let worker = spawn_worker(&rt);
        let first = rt.start(&name, user_message("go"), None).await.unwrap();
        wait_parked(&rt, first).await;

        // `later` leaves the file, and the prompt changes with it.
        edit(
            dir.path(),
            FILE,
            &agent_file(&name, "tools: [ask]", "New prompt."),
        );
        let reloaded = live.reload().unwrap();
        assert_eq!(
            reloaded.tool_changes,
            [ToolChange {
                agent: name.clone(),
                added: vec![],
                removed: vec!["later".into()],
            }],
            "{backend}"
        );
        assert_eq!(live.runs_on_previous_tools(), 1, "{backend}");

        // A run that starts now gets the new tool set and the new prompt.
        model.push_text("second done");
        let second = rt.start(&name, user_message("hi"), None).await.unwrap();
        wait_done(&rt, second).await;
        let requests = model.requests();
        assert_eq!(requests.len(), 2, "{backend}");
        assert_eq!(tool_names(&requests[1]), ["ask"], "{backend}");
        assert_eq!(
            requests[1].system.as_deref(),
            Some("New prompt."),
            "{backend}"
        );

        // The first run is still on the tool set it started with: the call it is owed is run for
        // real (the journal knows `tool:c2`, the new files do not), and its model turn is offered
        // the same tools.
        model.push_text("first done");
        rt.deliver(first, user_message("the second one"))
            .await
            .unwrap();
        wait_done(&rt, first).await;
        worker.stop().await;
        assert_eq!(later_calls.load(SeqCst), 1, "{backend}");
        let requests = model.requests();
        assert_eq!(requests.len(), 3, "{backend}");
        assert_eq!(tool_names(&requests[2]), ["ask", "later"], "{backend}");
        assert_eq!(
            requests[2].system.as_deref(),
            Some("Old prompt."),
            "{backend}"
        );
        // The pin ended with the run.
        assert_eq!(live.runs_on_previous_tools(), 0, "{backend}");
    }
}

#[tokio::test]
async fn going_back_to_the_old_tool_set_updates_the_runs_on_it() {
    let name = uniq("helper");
    let store: DynStore = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    edit(dir.path(), FILE, &agent_file(&name, "tools: [ask]", "One."));
    let model = Arc::new(MockModel::new());
    model
        .push_tool_calls(vec![call("c1", "ask")])
        .push_text("done");
    let calls = Arc::new(AtomicUsize::new(0));
    let tools = ToolSet::new()
        .tool(asking("ask"))
        .tool(counting("extra", &calls));
    let live = builder(dir.path(), &model).tools(tools).load().unwrap();
    let rt = runtime(&live, &store);
    let worker = spawn_worker(&rt);
    let run = rt.start(&name, user_message("go"), None).await.unwrap();
    wait_parked(&rt, run).await;

    edit(
        dir.path(),
        FILE,
        &agent_file(&name, "tools: [ask, extra]", "Two."),
    );
    live.reload().unwrap();
    assert_eq!(live.runs_on_previous_tools(), 1);
    // The tool set goes back to what the run has, with a new text: the run follows it again.
    edit(
        dir.path(),
        FILE,
        &agent_file(&name, "tools: [ask]", "Three."),
    );
    live.reload().unwrap();
    assert_eq!(live.runs_on_previous_tools(), 0);

    rt.deliver(run, user_message("answer")).await.unwrap();
    wait_done(&rt, run).await;
    worker.stop().await;
    assert_eq!(model.requests()[1].system.as_deref(), Some("Three."));
}

// --- new and removed agents -----------------------------------------------------------------

#[tokio::test]
async fn a_new_subagent_needs_a_restart_and_changes_nothing() {
    let name = uniq("helper");
    let dir = tempfile::tempdir().unwrap();
    edit(dir.path(), FILE, &agent_file(&name, "", "Old prompt."));
    let model = Arc::new(MockModel::new());
    let live = builder(dir.path(), &model).load().unwrap();

    edit(dir.path(), FILE, &agent_file(&name, "", "New prompt."));
    edit(
        dir.path(),
        "agent/subagents/extra.md",
        "---\ndescription: An extra helper.\n---\nHelp.\n",
    );
    let error = live.reload().unwrap_err();
    let ReloadError::NeedsRestart { added } = &*error else {
        panic!("wrong error: {error}");
    };
    assert_eq!(added, &[format!("{name}/extra")]);
    assert!(error.to_string().contains("restart"), "{error}");
    // The refusal is whole: the new prompt did not apply either.
    assert_eq!(live.generation(), 1);
    assert_eq!(live.info()[0].prompt, "Old prompt.");
    assert!(live.last_error().is_some());
}

#[tokio::test]
async fn a_removed_subagent_stays_registered_with_its_last_version() {
    let name = uniq("helper");
    let sub = format!("{name}/sub");
    let store: DynStore = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    edit(dir.path(), FILE, &agent_file(&name, "", "Parent."));
    edit(
        dir.path(),
        "agent/subagents/sub.md",
        "---\ndescription: The sub.\n---\nOld sub prompt.\n",
    );
    let model = Arc::new(MockModel::new());
    let live = builder(dir.path(), &model).load().unwrap();
    assert_eq!(live.info().len(), 2);
    let rt = runtime(&live, &store);
    let worker = spawn_worker(&rt);

    std::fs::remove_file(dir.path().join("agent/subagents/sub.md")).unwrap();
    let reloaded = live.reload().unwrap();
    assert_eq!(reloaded.retired, std::slice::from_ref(&sub));
    assert_eq!(
        reloaded.tool_changes,
        [ToolChange {
            agent: name.clone(),
            added: vec![],
            removed: vec!["sub".into()],
        }]
    );
    assert_eq!(live.retired(), std::slice::from_ref(&sub));
    assert_eq!(live.info().len(), 1);

    // A run of the retired agent (a parent that started before the edit could have started one)
    // is served by its last version, not failed.
    model.push_text("still here");
    let run = rt.start(&sub, user_message("go"), None).await.unwrap();
    wait_done(&rt, run).await;
    worker.stop().await;
    assert_eq!(
        model.requests()[0].system.as_deref(),
        Some("Old sub prompt.")
    );

    // Putting the file back brings it back.
    edit(
        dir.path(),
        "agent/subagents/sub.md",
        "---\ndescription: The sub.\n---\nNew sub prompt.\n",
    );
    live.reload().unwrap();
    assert!(live.retired().is_empty());
    assert_eq!(live.info().len(), 2);
}

// --- what the code supplies is applied again ------------------------------------------------

struct Env;

struct NeedsEnv;

#[async_trait]
impl Tool for NeedsEnv {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "needs_env".into(),
            description: "Needs the environment.".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }
    fn required_state(&self) -> Vec<StateKey> {
        vec![StateKey::of::<Env>()]
    }
    async fn call(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        ctx.require_state::<Env>()?;
        Ok(ToolOutput::text("saw the environment"))
    }
}

#[tokio::test]
async fn vars_and_state_from_code_are_applied_on_every_load() {
    let name = uniq("helper");
    let dir = tempfile::tempdir().unwrap();
    let file = |body: &str| {
        instructions(
            &format!("name: {name}\ntools: [needs_env]\nvars: {{ tone: plain }}"),
            body,
        )
    };
    edit(dir.path(), FILE, &file("Answer in a {{tone}} style."));
    let model = Arc::new(MockModel::new());
    let tools = || ToolSet::new().tool(NeedsEnv);

    // Without the state the first load is the startup error it always was.
    let error = builder(dir.path(), &model)
        .tools(tools())
        .load()
        .unwrap_err();
    assert!(matches!(error, Error::Build { .. }), "{error}");

    let env = Arc::new(Env);
    let tone = Arc::new(Mutex::new("formal".to_owned()));
    let live = builder(dir.path(), &model)
        .tools(tools())
        .configure({
            let (n, tone) = (name.clone(), tone.clone());
            move |def| def.agent_var(&n, "tone", tone.lock().unwrap().clone())
        })
        .configure_bound(move |bound| bound.state(env.clone()))
        .load()
        .unwrap();
    assert_eq!(live.info()[0].prompt, "Answer in a formal style.");

    // The value is read again on a reload, and so is the state (the build would fail without it).
    *tone.lock().unwrap() = "casual".into();
    edit(dir.path(), FILE, &file("Speak in a {{tone}} way."));
    live.reload().unwrap();
    assert_eq!(live.info()[0].prompt, "Speak in a casual way.");
}

// --- remote subagents -----------------------------------------------------------------------

const VAR: &str = "BILLING_AGENT_TOKEN";

type Seen = Arc<Mutex<Vec<Option<String>>>>;

/// An A2A server behind bearer authentication that notes the `Authorization` header of every
/// request.
struct Server {
    addr: SocketAddr,
    authorization: Seen,
}

impl Server {
    async fn start(backend: impl TaskBackend, token: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let card = AgentCardConfig::new(
            "billing",
            "Answers billing questions",
            format!("http://{addr}/").parse().unwrap(),
            "0.1.0",
        );
        let auth = AuthConfig::BearerTokens(vec![token.to_owned().into()]);
        let authorization = Seen::default();
        let app =
            A2aServer::router(card, Arc::new(backend), auth).layer(middleware::from_fn_with_state(
                authorization.clone(),
                |State(seen): State<Seen>, request: Request, next: Next| async move {
                    let header = request
                        .headers()
                        .get(axum::http::header::AUTHORIZATION)
                        .map(|v| v.to_str().unwrap().to_owned());
                    seen.lock().unwrap().push(header);
                    next.run(request).await
                },
            ));
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            addr,
            authorization,
        }
    }

    fn card_url(&self) -> String {
        format!("http://{}/.well-known/agent-card.json", self.addr)
    }
}

/// A backend whose tasks never finish.
struct Never;

fn working() -> Task {
    Task {
        id: a2a::new_task_id(),
        context_id: a2a::new_context_id(),
        status: TaskStatus {
            state: TaskState::Working,
            message: Some(Message::new(Role::Agent, vec![Part::text("working")])),
            timestamp: None,
        },
        artifacts: None,
        history: None,
        metadata: None,
    }
}

#[async_trait]
impl TaskBackend for Never {
    async fn submit(
        &self,
        _caller: Caller,
        _message: Message,
        _task_id: Option<String>,
        _context_id: Option<String>,
    ) -> Result<Task, BackendError> {
        Ok(working())
    }

    async fn get(&self, _caller: &Caller, task_id: &str) -> Result<Option<Task>, BackendError> {
        Ok(Some(Task {
            id: task_id.to_owned(),
            ..working()
        }))
    }

    async fn cancel(&self, _caller: &Caller, task_id: &str) -> Result<Task, BackendError> {
        Err(BackendError::TaskNotFound(task_id.to_owned()))
    }

    fn subscribe(
        &self,
        _caller: &Caller,
        task_id: &str,
    ) -> BoxStream<'static, Result<TaskEvent, BackendError>> {
        let error = BackendError::TaskNotFound(task_id.to_owned());
        Box::pin(futures::stream::once(async move { Err(error) }))
    }
}

fn remote_root(name: &str, prompt: &str, card_url: &str) -> Vec<(String, String)> {
    vec![
        (
            FILE.into(),
            instructions(&format!("name: {name}\ntools: []"), prompt),
        ),
        (
            "agent/subagents/billing.md".into(),
            format!(
                "---\ndescription: Handles billing questions for a customer account.\n\
                 a2a: {card_url}\nauth: bearer:{VAR}\n---\n"
            ),
        ),
    ]
}

fn write_all(root: &Path, files: &[(String, String)]) {
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, t)| (p.as_str(), t.as_str()))
        .collect();
    write(root, &refs);
}

/// `remote_timeout` (a definition setting) and `wait_poll` (a bound one) are given by hooks, and
/// a version made by a reload has them: without them the default wait is an hour and the poll a
/// minute, and this run would never finish.
#[tokio::test]
async fn remote_subagent_settings_are_applied_again_on_reload() {
    let server = Server::start(Never, "tok-7f3a9c2e51d84b06").await;
    let name = uniq("root");
    let store: DynStore = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    write_all(
        dir.path(),
        &remote_root(&name, "Before.", &server.card_url()),
    );
    let model = Arc::new(MockModel::new());
    let live = builder(dir.path(), &model)
        .configure(|def| {
            def.env(VAR, "tok-7f3a9c2e51d84b06")
                .remote_timeout(Duration::from_millis(150))
        })
        .configure_bound(|bound| bound.wait_poll(Duration::from_millis(30)))
        .load()
        .unwrap();
    let rt = runtime(&live, &store);
    let worker = spawn_worker(&rt);

    write_all(
        dir.path(),
        &remote_root(&name, "After.", &server.card_url()),
    );
    live.reload().unwrap();
    assert_eq!(live.info()[0].prompt, "After.");

    model
        .push_tool_calls(vec![ToolCall {
            id: "c1".into(),
            name: "billing".into(),
            arguments: json!({"message": "invoice 7"}),
        }])
        .push_text("gave up");
    let run = rt.start(&name, user_message("go"), None).await.unwrap();
    wait_done(&rt, run).await;
    worker.stop().await;
    let requests = model.requests();
    assert_eq!(requests[0].system.as_deref(), Some("After."));
    let Some(adam_model::Message::Tool {
        content, is_error, ..
    }) = requests[1].messages.last()
    else {
        panic!("no tool result: {:?}", requests[1].messages);
    };
    assert!(is_error);
    assert!(
        content.contains("did not finish in the time allowed"),
        "{content}"
    );
}

/// A reload binds the remote subagents again, so the tool is new and so is its client: a token
/// that was rotated (here, by the hook reading a value the test changes) is the one sent next.
#[tokio::test]
async fn a_reload_gives_the_remote_tool_a_fresh_client_and_token() {
    let server = Server::start(InMemoryBackend::new(), "new-token-1111").await;
    let name = uniq("root");
    let store: DynStore = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    write_all(dir.path(), &remote_root(&name, "Root.", &server.card_url()));
    let token = Arc::new(Mutex::new("old-token-0000".to_owned()));
    let model = Arc::new(MockModel::new());
    let live = builder(dir.path(), &model)
        .configure({
            let token = token.clone();
            move |def| def.env(VAR, token.lock().unwrap().clone())
        })
        .configure_bound(|bound| bound.wait_poll(Duration::from_millis(30)))
        .load()
        .unwrap();
    let rt = runtime(&live, &store);
    let worker = spawn_worker(&rt);
    let ask = |id: &str| {
        vec![ToolCall {
            id: id.into(),
            name: "billing".into(),
            arguments: json!({"message": "invoice 7"}),
        }]
    };

    // With the old token the server says no: an error result, and the run goes on.
    model.push_tool_calls(ask("c1")).push_text("first");
    let run = rt.start(&name, user_message("go"), None).await.unwrap();
    wait_done(&rt, run).await;
    let requests = model.requests();
    let Some(adam_model::Message::Tool { is_error, .. }) = requests[1].messages.last() else {
        panic!("no tool result");
    };
    assert!(is_error, "the old token is refused");

    *token.lock().unwrap() = "new-token-1111".into();
    live.reload().unwrap();
    model.push_tool_calls(ask("c2")).push_text("second");
    let run = rt.start(&name, user_message("again"), None).await.unwrap();
    wait_done(&rt, run).await;
    worker.stop().await;
    let requests = model.requests();
    let Some(adam_model::Message::Tool {
        is_error, content, ..
    }) = requests[3].messages.last()
    else {
        panic!("no tool result");
    };
    assert!(!is_error, "{content}");
    let seen = server.authorization.lock().unwrap().clone();
    assert!(
        seen.iter()
            .any(|h| h.as_deref() == Some("Bearer new-token-1111")),
        "{seen:?}"
    );
}

// --- the real watcher -----------------------------------------------------------------------

#[tokio::test]
async fn a_real_watcher_loads_an_edit_after_it_settles_and_keeps_the_good_version_on_a_bad_one() {
    let name = uniq("helper");
    let dir = tempfile::tempdir().unwrap();
    edit(dir.path(), FILE, &agent_file(&name, "", "First."));
    let model = Arc::new(MockModel::new());
    let live = builder(dir.path(), &model)
        .debounce(Duration::from_millis(30))
        .load()
        .unwrap();
    let watch = live.watch().unwrap();

    // A burst of writes, the way an editor saves: the last text is the one that ends up loaded.
    for n in 0..4 {
        edit(
            dir.path(),
            FILE,
            &agent_file(&name, "", &format!("Draft {n}.")),
        );
    }
    edit(dir.path(), FILE, &agent_file(&name, "", "Second."));
    wait_for("the edit to be loaded", || async {
        (live.info()[0].prompt == "Second.").then_some(())
    })
    .await;
    assert!(live.generation() >= 2);
    assert!(live.last_error().is_none());

    // A broken file is refused by the watcher too, and the error is kept.
    edit(dir.path(), FILE, "---\nname: broken\nno closing line\n");
    wait_for("the refusal", || async { live.last_error() }).await;
    assert_eq!(live.info()[0].prompt, "Second.");

    edit(dir.path(), FILE, &agent_file(&name, "", "Third."));
    wait_for("the fix to be loaded", || async {
        (live.info()[0].prompt == "Third." && live.last_error().is_none()).then_some(())
    })
    .await;

    // Dropping the watch joins its thread; a reload by hand still works.
    drop(watch);
    edit(dir.path(), FILE, &agent_file(&name, "", "Fourth."));
    live.reload().unwrap();
    assert_eq!(live.info()[0].prompt, "Fourth.");
}

#[tokio::test]
async fn a_watch_needs_something_to_watch() {
    let name = uniq("helper");
    let dir = tempfile::tempdir().unwrap();
    edit(dir.path(), FILE, &agent_file(&name, "", "Hi."));
    let live = builder(dir.path(), &Arc::new(MockModel::new()))
        .load()
        .unwrap();
    std::fs::remove_dir_all(dir.path().join("agent")).unwrap();
    let error = live.watch().unwrap_err();
    assert!(error.to_string().starts_with("nothing to watch"), "{error}");
    // ... and the reload says what is wrong with the files.
    let error = live.reload().unwrap_err();
    assert!(
        matches!(&*error, ReloadError::Load(Error::Manifest(_))),
        "{error}"
    );
    assert_eq!(live.info()[0].prompt, "Hi.");
}
