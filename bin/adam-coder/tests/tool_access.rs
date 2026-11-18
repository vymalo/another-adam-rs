//! What each tool of the coder shares with the other calls of its model turn (ADR 0049), and what that
//! makes the loop do.
//!
//! `every_tool_has_the_class_the_table_says` pins the class of every tool, so a tool added without a
//! decision here fails the test: the table below is `docs/reference/coder-agent.md`
//! "Calls at the same time". The rest runs the real classes through the real loop: each tool of a
//! turn is a probe that takes its class from the coder's own tool of that name, so what overlaps
//! and what waits is decided by the declarations the coder ships.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use adam_coder::coder_tools;
use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{
    DynTool, LlmAgent, Tool, ToolAccess, ToolCtx, ToolError, ToolOutput, Use, user_message,
};
use adam_model::{MockModel, ToolCall, ToolSpec};
use adam_runtime::{RetryPolicy, Runtime};
use async_trait::async_trait;
use common::Fixture;
use serde_json::{Value, json};

fn reads(scope: &str) -> ToolAccess {
    ToolAccess::reads(scope)
}

fn writes(scope: &str) -> ToolAccess {
    ToolAccess::writes(scope)
}

fn uses(uses: Vec<Use>) -> ToolAccess {
    ToolAccess::Uses(uses)
}

/// A tool's declared class for each of the argument shapes worth telling apart.
type Cases = Vec<(Value, ToolAccess)>;

/// The pinned table: the class of every tool the coder offers.
fn table() -> BTreeMap<&'static str, Cases> {
    let by_path = |read: bool| -> Cases {
        let scope = |s: &str| if read { reads(s) } else { writes(s) };
        vec![
            (json!({"path": "src/a.rs"}), scope("files/src/a.rs")),
            (json!({"path": "./src/./a.rs"}), scope("files/src/a.rs")),
            (json!({"path": "../a.rs"}), scope("files")),
            (json!({"path": "/etc/a"}), scope("files")),
            // The slot is not part of the scope: two spellings can name one slot.
            (
                json!({"path": "src/a.rs", "repo": "other"}),
                scope("files/src/a.rs"),
            ),
        ]
    };
    let optional_path = |mut cases: Cases| -> Cases {
        cases.push((json!({"pattern": "x"}), reads("files")));
        cases
    };
    let mut t: BTreeMap<&'static str, Cases> = BTreeMap::new();

    // Files: a path names the scope; reads share it, a write is alone with whatever overlaps it.
    t.insert("read_file", by_path(true));
    t.insert("share_file", by_path(true));
    t.insert("list_files", optional_path(by_path(true)));
    t.insert("find_files", optional_path(by_path(true)));
    t.insert("search_files", optional_path(by_path(true)));
    t.insert("write_file", by_path(false));
    t.insert("edit_file", by_path(false));
    t.insert(
        "apply_patch",
        vec![
            (json!({"patch": "x"}), writes("files")),
            (json!({"patch": "x", "repo": "main"}), writes("files")),
        ],
    );

    // Commands: one at a time, beside reads. A command that makes files also writes them.
    let command = json!({"command": "ls"});
    t.insert(
        "run_command",
        vec![(
            command.clone(),
            uses(vec![Use::read("files"), Use::write("commands")]),
        )],
    );
    for name in ["run", "start_command"] {
        t.insert(
            name,
            vec![(
                command.clone(),
                uses(vec![Use::write("files"), Use::write("commands")]),
            )],
        );
    }
    t.insert(
        "stop_command",
        vec![(json!({"id": "o-0123456789abcdef"}), writes("commands"))],
    );
    t.insert("list_commands", vec![(json!({}), reads("commands"))]);

    // The language servers read the slot (they sync what changed) and serialise on their own scope.
    for name in ["file_diagnostics", "find_definition", "find_references"] {
        t.insert(
            name,
            vec![(
                json!({"path": "src/a.rs"}),
                uses(vec![Use::read("files"), Use::write("lsp")]),
            )],
        );
    }

    // The screen.
    t.insert("show", vec![(json!({}), writes("screen"))]);
    t.insert("ui_catalog", vec![(json!({}), ToolAccess::Independent)]);
    t.insert("read_output", vec![(json!({}), ToolAccess::Independent)]);

    // Alone: the gates, the state the loop must apply first, the order of what they do, and the
    // tools that ask the person.
    for name in [
        "prepare_workspace",
        "start_scratch",
        "publish_scratch",
        "request_repository",
        "create_repository",
        "run_checks",
        "rebuild_environment",
        "commit_and_push",
        "open_pull_request",
        "merge_pull_request",
        "rename_repository",
        "delete_repository",
        "ask_user",
        "plan_work",
        "plan_check",
        "plan_add",
    ] {
        t.insert(name, vec![(json!({}), ToolAccess::Alone)]);
    }
    t
}

#[tokio::test]
async fn every_tool_has_the_class_the_table_says() {
    let fx = Fixture::new().await.offering_creation();
    let tools: Vec<DynTool> = coder_tools(&fx.env).into_iter().collect();
    let table = table();
    let offered: Vec<String> = tools.iter().map(|t| t.spec().name).collect();
    let mut pinned: Vec<String> = table.keys().map(|n| (*n).to_owned()).collect();
    let mut have = offered.clone();
    have.sort();
    pinned.sort();
    assert_eq!(
        have, pinned,
        "a tool without a row in the table (or a row without a tool): decide its class and pin it"
    );
    for tool in &tools {
        let name = tool.spec().name;
        for (args, expected) in &table[name.as_str()] {
            assert_eq!(&tool.access(args), expected, "{name} with {args}");
        }
    }
}

/// The coder's GitHub server is an MCP server of the agent folder, and an MCP tool runs beside the
/// other calls unless its server says `"parallel": false`: the folder must not say it.
#[test]
fn the_github_server_of_the_agent_folder_runs_beside_other_calls() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("agent/mcp.json");
    let text = std::fs::read_to_string(path).unwrap();
    let json: Value = serde_json::from_str(&text).unwrap();
    let servers = json.get("mcpServers").unwrap_or(&json);
    let github = &servers["github"];
    assert!(github.is_object(), "no github server in mcp.json");
    assert_ne!(github.get("parallel"), Some(&json!(false)));
}

// ---------------------------------------------------------------------------
// The real classes through the real loop
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Trace {
    events: Mutex<Vec<String>>,
    running: AtomicUsize,
    max_running: AtomicUsize,
}

impl Trace {
    fn at(&self, event: &str) -> usize {
        let events = self.events.lock().unwrap();
        events
            .iter()
            .position(|e| e == event)
            .unwrap_or_else(|| panic!("no {event}: {events:?}"))
    }

    /// Whether the two calls ran at the same time: each began before the other ended.
    fn overlapped(&self, a: &str, b: &str) -> bool {
        self.at(&format!("start:{a}")) < self.at(&format!("end:{b}"))
            && self.at(&format!("start:{b}")) < self.at(&format!("end:{a}"))
    }
}

/// A stand-in for one of the coder's tools: the same name and the same class, and a call that only
/// takes its time.
struct Shadow {
    real: DynTool,
    trace: Arc<Trace>,
}

#[async_trait]
impl Tool for Shadow {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.real.spec().name,
            description: "shadow".into(),
            parameters: json!({"type": "object"}),
        }
    }

    fn access(&self, args: &Value) -> ToolAccess {
        self.real.access(args)
    }

    async fn call(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        let id = ctx.call_id().to_owned();
        self.trace
            .events
            .lock()
            .unwrap()
            .push(format!("start:{id}"));
        let now = self.trace.running.fetch_add(1, SeqCst) + 1;
        self.trace.max_running.fetch_max(now, SeqCst);
        tokio::time::sleep(Duration::from_millis(150)).await;
        self.trace.running.fetch_sub(1, SeqCst);
        self.trace.events.lock().unwrap().push(format!("end:{id}"));
        Ok(ToolOutput::text(format!("ok:{id}")))
    }
}

fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
        ..Default::default()
    }
}

/// One turn of `calls` through an agent whose tools are the shadows of the coder's own, at `limit`.
async fn run_turn(calls: Vec<ToolCall>, limit: usize) -> Arc<Trace> {
    let fx = Fixture::new().await.offering_creation();
    let trace = Arc::new(Trace::default());
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(calls);
    mock.push_text("done");
    let mut builder = LlmAgent::builder("llm", mock, "test-model").max_parallel(limit);
    for real in coder_tools(&fx.env) {
        builder = builder.tool(Shadow {
            real,
            trace: trace.clone(),
        });
    }
    let store: DynStore = Arc::new(MemoryStore::new());
    let rt = Runtime::builder(store)
        .agent(builder.build())
        .worker_id(format!("w-{}", RunId::new()))
        .poll_interval(Duration::from_millis(20))
        .retry(RetryPolicy::default())
        .build();
    let run = rt.start("llm", user_message("go"), None).await.unwrap();
    let (stop, rx) = tokio::sync::oneshot::channel::<()>();
    let worker = {
        let rt = rt.clone();
        tokio::spawn(async move {
            rt.run_worker(async {
                let _ = rx.await;
            })
            .await
        })
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let view = rt.view(run).await.unwrap().unwrap();
        if matches!(view.status, RunStatus::Done | RunStatus::Failed) {
            assert_eq!(view.status, RunStatus::Done, "{view:#?}");
            break;
        }
        assert!(Instant::now() < deadline, "timed out: {view:#?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let _ = stop.send(());
    worker.await.unwrap().unwrap();
    trace
}

#[tokio::test]
async fn two_read_file_calls_overlap() {
    let t = run_turn(
        vec![
            call("c1", "read_file", json!({"path": "src/a.rs"})),
            call("c2", "read_file", json!({"path": "src/b.rs"})),
            call("c3", "search_files", json!({"pattern": "x"})),
            call("c4", "list_files", json!({})),
        ],
        10,
    )
    .await;
    assert_eq!(t.max_running.load(SeqCst), 4, "{:?}", t.events.lock());
}

#[tokio::test]
async fn a_read_and_a_write_of_the_same_path_do_not_overlap_and_other_paths_do() {
    let t = run_turn(
        vec![
            call("c1", "read_file", json!({"path": "src/a.rs"})),
            call(
                "c2",
                "edit_file",
                json!({"path": "src/a.rs", "old": "a", "new": "b"}),
            ),
            call(
                "c3",
                "write_file",
                json!({"path": "src/other.rs", "content": "x"}),
            ),
        ],
        10,
    )
    .await;
    assert!(!t.overlapped("c1", "c2"), "{:?}", t.events.lock());
    assert!(
        t.at("end:c1") < t.at("start:c2"),
        "the model's order is kept"
    );
    assert!(
        t.overlapped("c1", "c3"),
        "a write of another file starts beside the read: {:?}",
        t.events.lock()
    );
}

#[tokio::test]
async fn run_command_waits_for_a_write_and_runs_beside_reads() {
    let t = run_turn(
        vec![
            call("c1", "read_file", json!({"path": "src/a.rs"})),
            call("c2", "run_command", json!({"command": "ls"})),
            call(
                "c3",
                "write_file",
                json!({"path": "src/b.rs", "content": "x"}),
            ),
        ],
        10,
    )
    .await;
    assert!(
        t.overlapped("c1", "c2"),
        "a command runs beside a read: {:?}",
        t.events.lock()
    );
    assert!(
        t.at("end:c2") < t.at("start:c3"),
        "the write waits for the command that came before it: {:?}",
        t.events.lock()
    );
    // And the other way round: a command after a write waits for it.
    let t = run_turn(
        vec![
            call(
                "c1",
                "write_file",
                json!({"path": "src/b.rs", "content": "x"}),
            ),
            call("c2", "run_command", json!({"command": "ls"})),
        ],
        10,
    )
    .await;
    assert!(t.at("end:c1") < t.at("start:c2"), "{:?}", t.events.lock());
}

#[tokio::test]
async fn two_commands_never_overlap_but_run_beside_a_read() {
    let t = run_turn(
        vec![
            call("c1", "run_command", json!({"command": "a"})),
            call("c2", "run_command", json!({"command": "b"})),
            call("c3", "list_files", json!({})),
        ],
        10,
    )
    .await;
    assert!(!t.overlapped("c1", "c2"), "{:?}", t.events.lock());
    assert!(t.overlapped("c1", "c3"), "{:?}", t.events.lock());
    assert!(t.at("end:c1") < t.at("start:c2"));
}

#[tokio::test]
async fn a_gate_runs_alone() {
    let t = run_turn(
        vec![
            call("c1", "read_file", json!({"path": "a"})),
            call("c2", "run_checks", json!({"command": "x"})),
            call("c3", "read_file", json!({"path": "b"})),
        ],
        10,
    )
    .await;
    assert_eq!(t.max_running.load(SeqCst), 1, "{:?}", t.events.lock());
}

/// A tool that runs beside the other calls of its turn changes the run notes through
/// `NotesStore::update`, one step under the run's lock: a `load` and a `save` of its own could drop
/// the change of a call that ran between them. Only what never runs beside a call of its turn saves
/// the notes itself: the agent before the turn, and the gates that run alone (`run_checks` and its run
/// on the base, `commit_and_push`, `open_pull_request`). A new direct save is a decision: pin it here.
#[test]
fn only_what_runs_alone_saves_the_notes_itself() {
    fn walk(dir: &std::path::Path, root: &std::path::Path, found: &mut BTreeMap<String, usize>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, root, found);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let text: String = std::fs::read_to_string(&path)
                    .unwrap()
                    .split_whitespace()
                    .collect();
                let saves = text.matches(".notes.save(").count();
                if saves > 0 {
                    let rel = path
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned();
                    found.insert(rel, saves);
                }
            }
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found = BTreeMap::new();
    walk(&root, &root, &mut found);
    let expected: BTreeMap<String, usize> = [
        ("agent.rs", 1),
        ("tools/checks.rs", 1),
        ("tools/preexisting.rs", 1),
        ("tools/publish.rs", 4),
    ]
    .into_iter()
    .map(|(f, n)| (f.to_owned(), n))
    .collect();
    assert_eq!(
        found, expected,
        "a save of the notes outside `NotesStore::update`"
    );
}
