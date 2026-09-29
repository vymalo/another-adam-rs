//! Shared helpers of the integration tests: agent directories written to a temp dir, stub tools.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use adam_agent_fs::{AgentManifest, Dir, ManifestSource, Strictness};
use adam_assembly::AgentDef;
use adam_llm_agent::{FnTool, ToolError, ToolOutput, ToolSet};
use serde_json::json;

/// A tool called `name` that answers `<name>-out`.
pub fn stub(name: &str) -> FnTool {
    let out = format!("{name}-out");
    FnTool::raw(
        name,
        format!("the {name} tool"),
        json!({"type": "object", "properties": {}}),
        move |_ctx, _args| {
            let out = out.clone();
            async move { Ok::<_, ToolError>(ToolOutput::text(out)) }
        },
    )
}

/// A set of stub tools, in the order given.
pub fn tools(names: &[&str]) -> ToolSet {
    names
        .iter()
        .fold(ToolSet::new(), |set, name| set.tool(stub(name)))
}

/// Write `files` (path relative to the root, content) under `root`.
pub fn write(root: &Path, files: &[(&str, &str)]) {
    for (path, content) in files {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
}

/// Load the agents of a directory written from `files`, as a run-time load does. Warnings are
/// fine, errors are not.
pub fn manifests(files: &[(&str, &str)]) -> (tempfile::TempDir, Vec<AgentManifest>) {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), files);
    let package = Dir::new(dir.path())
        .default_name("coder")
        .load()
        .unwrap()
        .into_package(Strictness::Lenient)
        .unwrap();
    (dir, package.agents)
}

/// The single agent of a directory written from `files`.
pub fn def(files: &[(&str, &str)]) -> AgentDef {
    let (_dir, mut agents) = manifests(files);
    AgentDef::from_manifest(agents.remove(0)).unwrap()
}

/// `agent/instructions.md` with this frontmatter and body.
pub fn instructions(frontmatter: &str, body: &str) -> String {
    format!("---\n{frontmatter}\n---\n{body}\n")
}

use std::sync::Arc;
use std::time::{Duration, Instant};

use adam_core::{RunId, RunStatus};
use adam_runtime::{RunView, Runtime};
use tokio::sync::oneshot;

/// A worker task on a runtime, stopped with [`Worker::stop`].
pub struct Worker {
    stop: oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<Result<(), adam_runtime::RuntimeError>>,
}

pub fn spawn_worker(rt: &Runtime) -> Worker {
    let (stop, rx) = oneshot::channel::<()>();
    let rt = rt.clone();
    let handle = tokio::spawn(async move {
        rt.run_worker(async {
            let _ = rx.await;
        })
        .await
    });
    Worker { stop, handle }
}

impl Worker {
    pub async fn stop(self) {
        let _ = self.stop.send(());
        tokio::time::timeout(Duration::from_secs(10), self.handle)
            .await
            .expect("worker stops in time")
            .expect("worker task")
            .expect("worker result");
    }
}

/// Wait until the run is `Done` (or fail the test when it is `Failed` or takes too long).
pub async fn wait_done(rt: &Runtime, run: RunId) -> RunView {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let view = rt.view(run).await.unwrap().expect("run exists");
        match view.status {
            RunStatus::Done => return view,
            RunStatus::Failed => panic!("the run failed: {view:#?}"),
            _ => {}
        }
        assert!(Instant::now() < deadline, "timed out; last view: {view:#?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// A runtime over a fresh in-memory store with the assembly's agents registered.
pub fn runtime(assembly: &adam_assembly::Assembly) -> Runtime {
    let store: adam_core::DynStore = Arc::new(adam_core::MemoryStore::new());
    assembly
        .register(Runtime::builder(store))
        .poll_interval(Duration::from_millis(20))
        .build()
}

/// A runtime over `store` with the assembly's agents registered.
pub fn runtime_on(assembly: &adam_assembly::Assembly, store: adam_core::DynStore) -> Runtime {
    assembly
        .register(Runtime::builder(store))
        .poll_interval(Duration::from_millis(20))
        .build()
}

/// Wait until `check` returns `Some` (or fail the test when it takes too long).
pub async fn wait_for<T, F, Fut>(what: &str, mut check: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(found) = check().await {
            return found;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}
