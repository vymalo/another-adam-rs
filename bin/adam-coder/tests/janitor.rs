//! The janitor against real workspaces over a local bare remote and the in-memory store: which
//! runs lose their workspace, what stays, and how it runs as a component. (`tests/binary.rs` runs
//! it inside the process with Postgres.)
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use adam_coder::Janitor;
use adam_coder::tools::RunNotes;
use adam_core::{DynStore, MemoryStore, NewRun, RunId, RunStatus, RunUpdate};
use adam_llm_agent::KeptOutputs as _;
use adam_workspace::RepoRef;
use common::{FakeEnvironment, Fixture};
use serde_json::json;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// A run in `store` with `status`.
async fn run_in(store: &DynStore, status: RunStatus) -> RunId {
    run_of(store, status, None).await
}

/// A run in `store` with `status`, a task of `conversation`.
async fn run_of(store: &DynStore, status: RunStatus, conversation: Option<&str>) -> RunId {
    let id = RunId::new();
    store
        .create_run(NewRun {
            id,
            agent: "coder".to_owned(),
            conversation_id: conversation.map(str::to_owned),
            parent_id: None,
            status,
            state: json!({}),
            wake_at: None,
        })
        .await
        .unwrap();
    id
}

/// A workspace of one slot for `run`.
async fn workspace_of(fx: &Fixture, run: &str) {
    let repo = RepoRef::new(fx.remote_url(), "main");
    fx.env
        .workspaces
        .run(run)
        .unwrap()
        .add_repository(&repo)
        .await
        .unwrap();
}

fn workspace_dir(fx: &Fixture, run: &str) -> std::path::PathBuf {
    fx.root.join("workspaces").join(run)
}

/// A scratch project made without a conversation belongs to its run (nothing could take it over): a
/// cancelled (failed) run's project is deleted, a parked run's is kept with its files, and what was
/// published stays on the remote. (A project of a conversation lives for the thread: see below.)
#[tokio::test]
async fn a_scratch_project_goes_with_its_run_unless_the_run_is_waiting() {
    let fx = Fixture::new().await;
    let store: DynStore = Arc::new(MemoryStore::new());
    let cancelled = run_in(&store, RunStatus::Failed).await.to_string();
    let parked = run_in(&store, RunStatus::Parked).await.to_string();
    for run in [&cancelled, &parked] {
        let workspace = fx.env.workspaces.run(run).unwrap();
        let slot = workspace
            .add_scratch("fib", &fx.env.settings.identity)
            .await
            .unwrap();
        std::fs::write(slot.path().join("fib.sh"), "echo 0\n").unwrap();
    }
    // The cancelled run also has a repository in its workspace, beside the project.
    workspace_of(&fx, &cancelled).await;

    let janitor = Janitor::new(fx.env.workspaces.clone(), Some(Duration::from_secs(300)));
    let report = janitor
        .sweep(store.as_ref(), &CancellationToken::new())
        .await;
    assert_eq!(
        report.removed,
        std::slice::from_ref(&cancelled),
        "{report:?}"
    );
    assert!(report.failed.is_empty(), "{report:?}");
    assert!(!workspace_dir(&fx, &cancelled).exists());
    assert!(
        !fx.root.join("meta").join(&cancelled).exists(),
        "the metadata of its slots is gone too"
    );
    assert!(
        workspace_dir(&fx, &parked).join("fib/fib.sh").is_file(),
        "a run that waits for the person keeps its project"
    );
    assert!(workspace_dir(&fx, &parked).join("fib/.git").is_dir());
}

/// A scratch project lives for the thread: when its run is over, its worktrees go and the project
/// stays for as long as the conversation has a task within the idle time, whatever sweeps happen.
#[tokio::test]
async fn a_scratch_project_outlives_its_run_while_the_conversation_is_alive() {
    let fx = Fixture::new().await;
    let store: DynStore = Arc::new(MemoryStore::new());
    let done = run_of(&store, RunStatus::Done, Some("caller:thread-1"))
        .await
        .to_string();
    let workspace = fx.env.workspaces.run(&done).unwrap();
    let slot = workspace
        .add_scratch_for("site", &fx.env.settings.identity, Some("caller:thread-1"))
        .await
        .unwrap();
    std::fs::write(slot.path().join("index.html"), "<p>hi</p>\n").unwrap();
    workspace_of(&fx, &done).await;
    let environment = FakeEnvironment::new(Some(&fx.root));

    let janitor = Janitor::new(fx.env.workspaces.clone(), Some(Duration::from_secs(300)))
        .with_environment(environment.clone())
        .with_scratch_idle(Some(Duration::from_secs(7 * 86_400)));
    let report = janitor
        .sweep(store.as_ref(), &CancellationToken::new())
        .await;
    assert!(
        report.removed.is_empty() && report.failed.is_empty(),
        "{report:?}"
    );
    assert_eq!(
        report.kept_scratch,
        std::slice::from_ref(&done),
        "{report:?}"
    );
    assert!(
        !workspace_dir(&fx, &done).join("remote").exists(),
        "the repository's worktree went with the run"
    );
    assert!(
        workspace_dir(&fx, &done).join("site/index.html").is_file(),
        "the project stayed"
    );
    assert_eq!(
        environment.released.lock().unwrap().len(),
        1,
        "the environment was released once"
    );

    // The next sweeps leave it, and do not ask the environment again.
    for _ in 0..2 {
        let again = janitor
            .sweep(store.as_ref(), &CancellationToken::new())
            .await;
        assert_eq!(again.kept_scratch, std::slice::from_ref(&done), "{again:?}");
        assert!(again.removed.is_empty(), "{again:?}");
    }
    assert_eq!(environment.released.lock().unwrap().len(), 1);

    // A task of the conversation takes the project over: nothing is left of the finished run.
    let next = run_of(&store, RunStatus::Parked, Some("caller:thread-1"))
        .await
        .to_string();
    let taken = fx
        .env
        .workspaces
        .run(&next)
        .unwrap()
        .adopt_scratch("caller:thread-1")
        .await
        .unwrap();
    assert_eq!(taken, ["site"]);
    let last = janitor
        .sweep(store.as_ref(), &CancellationToken::new())
        .await;
    assert_eq!(last.removed, std::slice::from_ref(&done), "{last:?}");
    assert!(!workspace_dir(&fx, &done).exists());
    assert!(
        workspace_dir(&fx, &next).join("site/index.html").is_file(),
        "the open run has the project, and the sweep leaves an open run alone"
    );
}

/// The project goes once the conversation has had no task for the idle time, measured from its
/// newest task, and an open task keeps it however old the finished one is.
#[tokio::test]
async fn a_scratch_project_goes_when_its_conversation_has_been_idle() {
    let fx = Fixture::new().await;
    let store: DynStore = Arc::new(MemoryStore::new());
    let idle = run_of(&store, RunStatus::Done, Some("caller:idle"))
        .await
        .to_string();
    let busy = run_of(&store, RunStatus::Done, Some("caller:busy"))
        .await
        .to_string();
    for (run, conversation) in [(&idle, "caller:idle"), (&busy, "caller:busy")] {
        fx.env
            .workspaces
            .run(run)
            .unwrap()
            .add_scratch_for("site", &fx.env.settings.identity, Some(conversation))
            .await
            .unwrap();
    }
    // The busy conversation has a task open.
    run_of(&store, RunStatus::Parked, Some("caller:busy")).await;
    tokio::time::sleep(Duration::from_millis(60)).await;

    let janitor = Janitor::new(fx.env.workspaces.clone(), None)
        .with_scratch_idle(Some(Duration::from_millis(30)));
    let report = janitor
        .sweep(store.as_ref(), &CancellationToken::new())
        .await;
    assert_eq!(report.removed, std::slice::from_ref(&idle), "{report:?}");
    assert_eq!(
        report.kept_scratch,
        std::slice::from_ref(&busy),
        "{report:?}"
    );
    assert!(!workspace_dir(&fx, &idle).exists());
    assert!(workspace_dir(&fx, &busy).join("site/.git").is_dir());
}

/// `SCRATCH_IDLE_DAYS=0` is the old rule: the project goes with its run, conversation or not.
#[tokio::test]
async fn without_an_idle_time_a_scratch_project_goes_with_its_run() {
    let fx = Fixture::new().await;
    let store: DynStore = Arc::new(MemoryStore::new());
    let done = run_of(&store, RunStatus::Done, Some("caller:thread-1"))
        .await
        .to_string();
    fx.env
        .workspaces
        .run(&done)
        .unwrap()
        .add_scratch_for("site", &fx.env.settings.identity, Some("caller:thread-1"))
        .await
        .unwrap();
    let report = Janitor::new(fx.env.workspaces.clone(), None)
        .with_scratch_idle(None)
        .sweep(store.as_ref(), &CancellationToken::new())
        .await;
    assert_eq!(report.removed, std::slice::from_ref(&done), "{report:?}");
    assert!(report.kept_scratch.is_empty());
    assert!(!workspace_dir(&fx, &done).exists());
}

#[tokio::test]
async fn a_sweep_removes_what_is_over_and_keeps_what_is_open() {
    let fx = Fixture::new().await;
    let store: DynStore = Arc::new(MemoryStore::new());
    let done = run_in(&store, RunStatus::Done).await.to_string();
    let failed = run_in(&store, RunStatus::Failed).await.to_string();
    let parked = run_in(&store, RunStatus::Parked).await.to_string();
    let runnable = run_in(&store, RunStatus::Runnable).await.to_string();
    let unknown = RunId::new().to_string();
    for run in [&done, &failed, &parked, &runnable, &unknown] {
        workspace_of(&fx, run).await;
    }
    // A legacy worktree (one per run, made by `prepare`) of a finished run goes too.
    let legacy = run_in(&store, RunStatus::Done).await.to_string();
    fx.env
        .workspaces
        .prepare(&RepoRef::new(fx.remote_url(), "main"), &legacy)
        .await
        .unwrap();
    // Notes, a directory that is not a run, and a mirror.
    fx.env
        .notes
        .save(&done, &RunNotes::default())
        .await
        .unwrap();
    std::fs::create_dir_all(fx.root.join("workspaces/scratch-notes")).unwrap();

    let janitor = Janitor::new(fx.env.workspaces.clone(), Some(Duration::from_secs(300)));
    let report = janitor
        .sweep(store.as_ref(), &CancellationToken::new())
        .await;
    let mut removed = report.removed.clone();
    removed.sort();
    let mut want = vec![
        done.clone(),
        failed.clone(),
        unknown.clone(),
        legacy.clone(),
    ];
    want.sort();
    assert_eq!(removed, want, "{report:?}");
    assert_eq!(
        report.kept, 3,
        "two open runs, and a directory that is not a run: {report:?}"
    );
    assert!(report.failed.is_empty(), "{report:?}");
    for run in [&done, &failed, &unknown] {
        assert!(!workspace_dir(&fx, run).exists(), "{run}");
    }
    assert!(
        !fx.root.join("worktrees").join(&legacy).exists(),
        "the legacy worktree"
    );
    assert!(!fx.root.join("meta").join(format!("{legacy}.json")).exists());
    for run in [&parked, &runnable] {
        assert!(
            workspace_dir(&fx, run).join("remote/README.md").is_file(),
            "{run} keeps its files"
        );
    }
    assert!(fx.root.join("workspaces/scratch-notes").is_dir());
    assert!(
        fx.root.join("coder").join(format!("{done}.json")).is_file(),
        "the notes stay"
    );
    assert!(fx.root.join("git").is_dir(), "the mirrors stay");

    // Nothing is left to do now, and a run that finishes is swept at the next one.
    let again = janitor
        .sweep(store.as_ref(), &CancellationToken::new())
        .await;
    assert!(
        again.removed.is_empty() && again.failed.is_empty(),
        "{again:?}"
    );
    let record = store
        .load_run(parked.parse().map(RunId).unwrap())
        .await
        .unwrap()
        .unwrap();
    store
        .commit_run(
            record.id,
            record.version,
            RunUpdate::new(RunStatus::Done, record.state),
        )
        .await
        .unwrap();
    let last = janitor
        .sweep(store.as_ref(), &CancellationToken::new())
        .await;
    assert_eq!(last.removed, std::slice::from_ref(&parked));
    assert!(!workspace_dir(&fx, &parked).exists());
    // The run's branch is still in the mirror: a workspace is files, the branch is the work.
    let mirror = fx.root.join(
        RepoRef::new(fx.remote_url(), "main")
            .locate()
            .unwrap()
            .mirror_relative(),
    );
    let branches = common::git(
        &mirror,
        &["for-each-ref", "--format=%(refname)", "refs/heads/agent"],
    );
    assert!(
        branches
            .lines()
            .any(|b| b.starts_with(&format!("refs/heads/agent/{}", &done[..8]))),
        "{branches}"
    );
}

/// One sweep asks the store about each run once, whatever the later passes look at it for: the
/// loop over workspaces, the kept outputs, and what the environment still holds all reuse the
/// verdict of that one read.
#[tokio::test]
async fn a_sweep_reads_each_run_once() {
    use adam_llm_agent::KeptOutputs as _;
    use adam_store_testkit::fault::{FaultyStore, Method};

    let fx = Fixture::new().await;
    let counting = Arc::new(FaultyStore::new(Arc::new(MemoryStore::new())));
    let store: DynStore = counting.clone();
    let fake = FakeEnvironment::new(Some(&fx.root));
    let mut runs = Vec::new();
    for status in [
        RunStatus::Runnable,
        RunStatus::Parked,
        RunStatus::Done,
        RunStatus::Failed,
    ] {
        let run = run_in(&store, status).await.to_string();
        workspace_of(&fx, &run).await;
        let id = adam_llm_agent::output_id(&run, "c1");
        fx.env.kept.keep(&run, &id, "kept\n", 0).await.unwrap();
        runs.push(run);
    }
    // Every one of them is also something the environment still holds: the orphan pass would ask
    // again about the open ones, and the kept-output pass about the open ones (a finished run's
    // output went with its workspace).
    *fake.held.lock().unwrap() = runs.clone();

    let report = janitor_with(&fx, &fake)
        .sweep(store.as_ref(), &CancellationToken::new())
        .await;
    assert!(report.failed.is_empty(), "{report:?}");
    assert_eq!(
        counting.calls(Method::LoadRun),
        4,
        "one read per run, not one per pass: {report:?}"
    );
}

/// An environment that, when the sweep asks what it holds, does what a retry after a purge does:
/// creates the run again under the same id, with kept output under the same id.
struct Recreated {
    inner: Arc<FakeEnvironment>,
    store: DynStore,
    kept: Arc<adam_coder::kept::FileKept>,
    run: String,
    once: AtomicBool,
}

#[async_trait::async_trait]
impl adam_workspace::Environment for Recreated {
    async fn ensure(
        &self,
        workspace: &adam_workspace::RunWorkspace,
        progress: &dyn adam_workspace::EnvProgress,
    ) -> Result<Arc<dyn adam_workspace::EnvSession>, adam_workspace::EnvError> {
        self.inner.ensure(workspace, progress).await
    }

    async fn release(&self, run: &str) -> Result<(), adam_workspace::EnvError> {
        self.inner.release(run).await
    }

    async fn held_runs(&self) -> Result<Vec<String>, adam_workspace::EnvError> {
        if !self.once.swap(true, Ordering::SeqCst) {
            self.store
                .create_run(NewRun {
                    id: RunId(self.run.parse().unwrap()),
                    agent: "coder".to_owned(),
                    conversation_id: None,
                    parent_id: None,
                    status: RunStatus::Runnable,
                    state: json!({}),
                    wake_at: None,
                })
                .await
                .unwrap();
            let id = adam_llm_agent::output_id(&self.run, "c1");
            self.kept
                .keep(&self.run, &id, "kept again\n", 0)
                .await
                .unwrap();
        }
        self.inner.held_runs().await
    }
}

/// A purged id is not remembered as finished: `task_id_for` derives it from the caller, the context
/// and the message id, so a retry can create the same run during a sweep. The kept-output pass
/// re-reads an unknown id, sees the live run, and leaves its output alone.
#[tokio::test]
async fn a_purged_run_recreated_during_a_sweep_keeps_its_output() {
    use adam_llm_agent::KeptOutputs as _;

    let fx = Fixture::new().await;
    let store: DynStore = Arc::new(MemoryStore::new());
    let unknown = RunId::new().to_string();
    workspace_of(&fx, &unknown).await;
    let id = adam_llm_agent::output_id(&unknown, "c1");
    fx.env.kept.keep(&unknown, &id, "kept\n", 0).await.unwrap();
    let fake = FakeEnvironment::new(Some(&fx.root));
    *fake.held.lock().unwrap() = vec![unknown.clone()];
    let recreated = Arc::new(Recreated {
        inner: fake,
        store: store.clone(),
        kept: fx.env.kept.clone(),
        run: unknown.clone(),
        once: AtomicBool::new(false),
    });
    let janitor = Janitor::new(fx.env.workspaces.clone(), Some(Duration::from_secs(300)))
        .with_environment(recreated);
    let report = janitor
        .sweep(store.as_ref(), &CancellationToken::new())
        .await;
    assert!(report.failed.is_empty(), "{report:?}");
    assert_eq!(report.removed, std::slice::from_ref(&unknown), "{report:?}");
    assert!(
        fx.root.join("outputs").join(&unknown).is_dir(),
        "the retry recreated the run during the sweep: its output stays"
    );
}

/// A sweep that is told to stop between two runs stops.
#[tokio::test]
async fn a_cancelled_sweep_stops_before_it_removes_anything() {
    let fx = Fixture::new().await;
    let store: DynStore = Arc::new(MemoryStore::new());
    let done = run_in(&store, RunStatus::Done).await.to_string();
    workspace_of(&fx, &done).await;
    let stop = CancellationToken::new();
    stop.cancel();
    let report = Janitor::new(fx.env.workspaces.clone(), None)
        .sweep(store.as_ref(), &stop)
        .await;
    assert!(report.removed.is_empty(), "{report:?}");
    assert!(workspace_dir(&fx, &done).is_dir());
}

/// As a component: a sweep at start, then one every interval, and it ends when it is stopped.
#[tokio::test]
async fn the_component_sweeps_at_start_and_again_and_stops_with_the_workers() {
    let fx = Fixture::new().await;
    let store: DynStore = Arc::new(MemoryStore::new());
    let first = run_in(&store, RunStatus::Done).await.to_string();
    workspace_of(&fx, &first).await;
    let janitor = Janitor::new(fx.env.workspaces.clone(), Some(Duration::from_millis(100)));
    let stop = CancellationToken::new();
    let component = tokio::spawn(janitor.run(store.clone(), stop.clone()));
    let until_gone = |run: String| {
        let dir = workspace_dir(&fx, &run);
        async move {
            for _ in 0..200 {
                if !dir.exists() {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            false
        }
    };
    assert!(until_gone(first).await, "the first sweep");
    // A run that ends later is swept by a later one.
    let later = run_in(&store, RunStatus::Done).await.to_string();
    workspace_of(&fx, &later).await;
    assert!(until_gone(later).await, "a later sweep");
    stop.cancel();
    let ended = tokio::time::timeout(Duration::from_secs(10), component)
        .await
        .expect("it stops when told to")
        .unwrap();
    assert!(ended.is_ok());
}

/// Without an interval the component only waits for the stop: it must not return early (a
/// component that ends before shutdown stops the process).
#[tokio::test]
async fn a_janitor_that_is_off_waits_for_the_stop_and_removes_nothing() {
    let fx = Fixture::new().await;
    let store: DynStore = Arc::new(MemoryStore::new());
    let done = run_in(&store, RunStatus::Done).await.to_string();
    workspace_of(&fx, &done).await;
    let stop = CancellationToken::new();
    let mut component =
        tokio::spawn(Janitor::new(fx.env.workspaces.clone(), None).run(store, stop.clone()));
    assert!(
        tokio::time::timeout(Duration::from_millis(500), &mut component)
            .await
            .is_err(),
        "it is still running"
    );
    assert!(workspace_dir(&fx, &done).is_dir());
    stop.cancel();
    assert!(
        tokio::time::timeout(Duration::from_secs(5), component)
            .await
            .unwrap()
            .unwrap()
            .is_ok()
    );
}

/// A fixture whose janitor holds `fake` as its environment.
fn janitor_with(fx: &Fixture, fake: &Arc<FakeEnvironment>) -> Janitor {
    Janitor::new(fx.env.workspaces.clone(), Some(Duration::from_secs(300)))
        .with_environment(fake.clone())
}

/// The environment of a run that is over is released **before** its workspace is removed, and an
/// open run's is not touched.
#[tokio::test]
async fn the_environment_of_a_finished_run_is_released_before_its_workspace_is_removed() {
    let fx = Fixture::new().await;
    let fake = FakeEnvironment::new(Some(&fx.root));
    let store: DynStore = Arc::new(MemoryStore::new());
    let done = run_in(&store, RunStatus::Done).await.to_string();
    let unknown = RunId::new().to_string();
    let parked = run_in(&store, RunStatus::Parked).await.to_string();
    for run in [&done, &unknown, &parked] {
        workspace_of(&fx, run).await;
    }

    let report = janitor_with(&fx, &fake)
        .sweep(store.as_ref(), &CancellationToken::new())
        .await;
    assert!(report.failed.is_empty(), "{report:?}");
    let mut released = fake.released.lock().unwrap().clone();
    released.sort();
    let mut want = vec![(done.clone(), true), (unknown.clone(), true)];
    want.sort();
    assert_eq!(
        released, want,
        "released while the workspace was still there, and never for the open run"
    );
    assert!(!workspace_dir(&fx, &done).exists());
    assert!(!workspace_dir(&fx, &unknown).exists());
    assert!(workspace_dir(&fx, &parked).is_dir());
}

/// A workspace that an environment could not let go of stays, and goes at the next sweep.
#[tokio::test]
async fn a_release_that_fails_keeps_the_workspace_for_the_next_sweep() {
    let fx = Fixture::new().await;
    let fake = FakeEnvironment::new(Some(&fx.root));
    let store: DynStore = Arc::new(MemoryStore::new());
    let done = run_in(&store, RunStatus::Done).await.to_string();
    workspace_of(&fx, &done).await;
    let janitor = janitor_with(&fx, &fake);

    fake.release_fails.store(true, Ordering::SeqCst);
    let report = janitor
        .sweep(store.as_ref(), &CancellationToken::new())
        .await;
    assert_eq!(report.failed, std::slice::from_ref(&done), "{report:?}");
    assert!(report.removed.is_empty(), "{report:?}");
    assert!(
        workspace_dir(&fx, &done).join("remote/README.md").is_file(),
        "the files an environment may still hold stay"
    );

    fake.release_fails.store(false, Ordering::SeqCst);
    let report = janitor
        .sweep(store.as_ref(), &CancellationToken::new())
        .await;
    assert_eq!(report.removed, std::slice::from_ref(&done), "{report:?}");
    assert!(report.failed.is_empty(), "{report:?}");
    assert!(!workspace_dir(&fx, &done).exists());
}

/// What an environment still holds for a run that is over is released even when the run has no
/// workspace here (a crash left it), and what it holds for an open run, or for something that is not
/// a run, is not.
#[tokio::test]
async fn what_an_environment_holds_for_runs_that_are_over_is_released_without_a_workspace() {
    let fx = Fixture::new().await;
    let fake = FakeEnvironment::new(Some(&fx.root));
    let store: DynStore = Arc::new(MemoryStore::new());
    let finished = run_in(&store, RunStatus::Failed).await.to_string();
    let unknown = RunId::new().to_string();
    let open = run_in(&store, RunStatus::Runnable).await.to_string();
    // This one has a workspace as well: it is released once, by the loop over workspaces.
    let with_files = run_in(&store, RunStatus::Done).await.to_string();
    workspace_of(&fx, &with_files).await;
    *fake.held.lock().unwrap() = vec![
        finished.clone(),
        unknown.clone(),
        open.clone(),
        with_files.clone(),
        "not-a-run".to_owned(),
    ];

    let report = janitor_with(&fx, &fake)
        .sweep(store.as_ref(), &CancellationToken::new())
        .await;
    let mut orphans = report.orphans.clone();
    orphans.sort();
    let mut want = vec![finished.clone(), unknown.clone()];
    want.sort();
    assert_eq!(orphans, want, "{report:?}");
    assert_eq!(
        report.removed,
        std::slice::from_ref(&with_files),
        "{report:?}"
    );
    assert!(report.failed.is_empty(), "{report:?}");
    let released = fake.released.lock().unwrap().clone();
    assert_eq!(
        released
            .iter()
            .filter(|(run, _)| *run == with_files)
            .count(),
        1,
        "released once: {released:?}"
    );
    assert_eq!(
        *fake.held.lock().unwrap(),
        [open, "not-a-run".to_owned()],
        "what is left held is the open run's and what is not a run's"
    );
}

/// The kept output of a run (`<root>/outputs/<run>`) goes with the run's workspace, also when the
/// workspace is already gone; an open run's stays.
#[tokio::test]
async fn the_janitor_removes_kept_output_with_the_run() {
    use adam_llm_agent::KeptOutputs as _;

    let fx = Fixture::new().await;
    let store: DynStore = Arc::new(MemoryStore::new());
    let done = run_in(&store, RunStatus::Done).await.to_string();
    let orphan = run_in(&store, RunStatus::Failed).await.to_string();
    let open = run_in(&store, RunStatus::Runnable).await.to_string();
    workspace_of(&fx, &done).await;
    for run in [&done, &orphan, &open] {
        let id = adam_llm_agent::output_id(run, "c1");
        fx.env.kept.keep(run, &id, "kept\n", 0).await.unwrap();
    }
    let janitor = Janitor::new(fx.env.workspaces.clone(), Some(Duration::from_secs(300)));
    let report = janitor
        .sweep(store.as_ref(), &CancellationToken::new())
        .await;
    assert!(report.failed.is_empty(), "{report:?}");
    let outputs = fx.root.join("outputs");
    assert!(!outputs.join(&done).exists(), "removed with its workspace");
    assert!(
        !outputs.join(&orphan).exists(),
        "removed with no workspace left"
    );
    assert!(outputs.join(&open).is_dir(), "an open run keeps its output");
}

/// An environment that says how many commands of the run still ran when it was asked to release it.
struct Watching {
    inner: Arc<FakeEnvironment>,
    commands: adam_coder::Commands,
    running_at_release: std::sync::Mutex<Vec<(String, usize)>>,
}

#[async_trait::async_trait]
impl adam_workspace::Environment for Watching {
    async fn ensure(
        &self,
        workspace: &adam_workspace::RunWorkspace,
        progress: &dyn adam_workspace::EnvProgress,
    ) -> Result<Arc<dyn adam_workspace::EnvSession>, adam_workspace::EnvError> {
        self.inner.ensure(workspace, progress).await
    }

    async fn release(&self, run: &str) -> Result<(), adam_workspace::EnvError> {
        self.running_at_release
            .lock()
            .unwrap()
            .push((run.to_owned(), self.commands.running_count(run)));
        self.inner.release(run).await
    }

    async fn held_runs(&self) -> Result<Vec<String>, adam_workspace::EnvError> {
        self.inner.held_runs().await
    }
}

/// A command that keeps running is stopped before the environment that holds it is released: the
/// backstop for a run whose end nobody saw (a worker that was down).
#[tokio::test]
async fn the_sweep_stops_background_commands_before_release() {
    use adam_coder::tools::background::{CommandState, StartCommand};
    use adam_llm_agent::{Tool as _, ToolCtx};
    use adam_runtime::CollectingSink;

    let fx = Fixture::new().await;
    let store: DynStore = Arc::new(MemoryStore::new());
    let run = run_in(&store, RunStatus::Failed).await;
    workspace_of(&fx, &run.to_string()).await;
    let ctx = ToolCtx::detached("tool", "c1", Arc::new(CollectingSink::new()))
        .with_state(fx.env.clone())
        .with_root_run(run);
    let started = StartCommand
        .call(&ctx, json!({"command": "sleep 600", "wait": 1}))
        .await
        .unwrap();
    assert!(!started.is_error, "{}", started.content);
    assert_eq!(fx.env.commands.running_count(&run.to_string()), 1);

    let watching = Arc::new(Watching {
        inner: FakeEnvironment::new(Some(&fx.root)),
        commands: fx.env.commands.clone(),
        running_at_release: std::sync::Mutex::default(),
    });
    let janitor = Janitor::new(fx.env.workspaces.clone(), Some(Duration::from_secs(300)))
        .with_environment(watching.clone())
        .with_commands(fx.env.commands.clone());
    let report = janitor
        .sweep(store.as_ref(), &CancellationToken::new())
        .await;
    assert!(report.failed.is_empty(), "{report:?}");
    assert_eq!(
        *watching.running_at_release.lock().unwrap(),
        [(run.to_string(), 0)],
        "none ran any more when the environment was released"
    );
    let states = fx.env.commands.states(&run.to_string());
    assert!(
        states.is_empty()
            || states.iter().all(|(_, state)| matches!(
                state,
                CommandState::Stopped { why } if why == "the run ended"
            )),
        "{states:?}"
    );
}

/// An environment that holds a run no workspace of this volume has any more: the janitor stops its
/// commands before it releases what holds them.
#[tokio::test]
async fn the_orphan_sweep_stops_background_commands_before_release() {
    use adam_coder::tools::background::{CommandState, StartCommand};
    use adam_llm_agent::{Tool as _, ToolCtx};
    use adam_runtime::CollectingSink;

    let fx = Fixture::new().await;
    let store: DynStore = Arc::new(MemoryStore::new());
    let run = run_in(&store, RunStatus::Failed).await;
    let run_id = run.to_string();
    workspace_of(&fx, &run_id).await;
    let ctx = ToolCtx::detached("tool", "c1", Arc::new(CollectingSink::new()))
        .with_state(fx.env.clone())
        .with_root_run(run);
    let started = StartCommand
        .call(&ctx, json!({"command": "sleep 661", "wait": 1}))
        .await
        .unwrap();
    assert!(!started.is_error, "{}", started.content);
    // The workspace is gone (a crash between two removals); the environment still holds the run.
    std::fs::remove_dir_all(workspace_dir(&fx, &run_id)).unwrap();
    let _ = std::fs::remove_dir_all(fx.root.join("worktrees").join(&run_id));
    let _ = std::fs::remove_dir_all(fx.root.join("meta").join(&run_id));
    let _ = std::fs::remove_file(fx.root.join("workspaces").join(format!("{run_id}.lock")));
    let watching = Arc::new(Watching {
        inner: FakeEnvironment::new(Some(&fx.root)),
        commands: fx.env.commands.clone(),
        running_at_release: std::sync::Mutex::default(),
    });
    *watching.inner.held.lock().unwrap() = vec![run_id.clone()];
    let janitor = Janitor::new(fx.env.workspaces.clone(), Some(Duration::from_secs(300)))
        .with_environment(watching.clone())
        .with_commands(fx.env.commands.clone());
    let report = janitor
        .sweep(store.as_ref(), &CancellationToken::new())
        .await;
    assert_eq!(report.orphans, std::slice::from_ref(&run_id), "{report:?}");
    assert_eq!(
        *watching.running_at_release.lock().unwrap(),
        [(run_id.clone(), 0)],
        "none ran any more when the environment was released"
    );
    assert!(
        fx.env
            .commands
            .states(&run_id)
            .iter()
            .all(|(_, state)| matches!(
                state,
                CommandState::Stopped { why } if why == "the run ended"
            )),
        "{:?}",
        fx.env.commands.states(&run_id)
    );
}

/// What a worker that is gone left running for a finished run is killed by pid before the janitor
/// removes the records that name it, and so is what is left of its group.
#[tokio::test]
async fn the_janitor_kills_what_the_records_of_a_finished_run_name_before_it_removes_them() {
    use std::os::unix::process::CommandExt as _;

    let fx = Fixture::new().await;
    let store: DynStore = Arc::new(MemoryStore::new());
    let run = run_in(&store, RunStatus::Done).await.to_string();
    workspace_of(&fx, &run).await;
    let mut leader = std::process::Command::new("sh")
        .arg("-c")
        .arg("sleep 662 & wait")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    let pid = leader.id();
    let _reap = common::GroupGuard(pid);
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let start: u64 = stat
        .rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap();
    let id = adam_llm_agent::output_id(&run, "orphan");
    let record = json!({
        "command": "orphan", "slot": "remote", "cwd": "", "exec": "bg-orphan-1",
        "owner": "a-worker-that-is-gone", "pid": pid, "start": start, "started_at": 1,
        "state": "running", "ran_secs": null, "lines": 0, "git": null,
        // A beat that is fresh: the run is over, so it does not matter.
        "host": fx.env.commands.host(),
        "beat": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    });
    fx.env
        .kept
        .put_record(&run, &id, &serde_json::to_vec(&record).unwrap())
        .unwrap();
    let janitor = Janitor::new(fx.env.workspaces.clone(), Some(Duration::from_secs(300)))
        .with_commands(fx.env.commands.clone());
    let report = janitor
        .sweep(store.as_ref(), &CancellationToken::new())
        .await;
    assert!(report.failed.is_empty(), "{report:?}");
    let began = std::time::Instant::now();
    while leader.try_wait().unwrap().is_none() {
        assert!(began.elapsed() < Duration::from_secs(10), "still running");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // Nothing of the group is left either.
    let left = std::fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|p| {
            std::fs::read_to_string(format!("/proc/{p}/stat")).is_ok_and(|s| {
                s.rsplit_once(") ").is_some_and(|(_, rest)| {
                    rest.split_whitespace().nth(2) == Some(pid.to_string().as_str())
                        && !rest.starts_with('Z')
                })
            })
        })
        .count();
    assert_eq!(left, 0, "what the leader left in its group");
}

/// The janitor kills by pid only what the record says this machine started.
#[tokio::test]
async fn the_janitor_signals_no_pid_of_another_machine() {
    use std::os::unix::process::CommandExt as _;

    let fx = Fixture::new().await;
    let store: DynStore = Arc::new(MemoryStore::new());
    let run = run_in(&store, RunStatus::Done).await.to_string();
    workspace_of(&fx, &run).await;
    let mut other = std::process::Command::new("sleep")
        .arg("666")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    let pid = other.id();
    let _reap = common::GroupGuard(pid);
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let start: u64 = stat
        .rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap();
    let id = adam_llm_agent::output_id(&run, "elsewhere");
    let record = json!({
        "command": "elsewhere", "slot": "remote", "cwd": "", "exec": "bg-elsewhere-1",
        "owner": "a-worker-on-another-pod", "pid": pid, "start": start, "started_at": 1,
        "state": "running", "ran_secs": null, "lines": 0, "git": null, "beat": 0,
        "host": "another-pod",
    });
    fx.env
        .kept
        .put_record(&run, &id, &serde_json::to_vec(&record).unwrap())
        .unwrap();
    let janitor = Janitor::new(fx.env.workspaces.clone(), Some(Duration::from_secs(300)))
        .with_commands(fx.env.commands.clone());
    let report = janitor
        .sweep(store.as_ref(), &CancellationToken::new())
        .await;
    assert!(report.failed.is_empty(), "{report:?}");
    // The run is over and its records are gone with it; the process of this machine was not touched.
    assert!(other.try_wait().unwrap().is_none(), "another machine's pid");
    other.kill().unwrap();
    other.wait().unwrap();
}

/// The images of a conversation (`<root>/images/<scope>`) go when its scratch projects would: the
/// conversation has had no task for the idle time. An open task keeps them, and a run with no
/// conversation takes its own with it.
#[tokio::test]
async fn images_go_with_an_idle_conversation() {
    use adam_llm_agent::{KeptImages as _, image_scope, sha256_hex};

    let fx = Fixture::new().await;
    let store: DynStore = Arc::new(MemoryStore::new());
    let idle = run_of(&store, RunStatus::Done, Some("caller:idle"))
        .await
        .to_string();
    let busy = run_of(&store, RunStatus::Done, Some("caller:busy"))
        .await
        .to_string();
    let alone = run_in(&store, RunStatus::Done).await.to_string();
    let png = b"\x89PNG\r\n\x1a\nan image".to_vec();
    let scopes = [
        image_scope(Some("caller:idle"), &idle),
        image_scope(Some("caller:busy"), &busy),
        image_scope(None, &alone),
    ];
    for (run, scope) in [&idle, &busy, &alone].into_iter().zip(&scopes) {
        workspace_of(&fx, run).await;
        fx.env
            .images
            .keep(scope, &sha256_hex(&png), "image/png", &png)
            .await
            .unwrap();
    }
    // The busy conversation has a task open.
    run_of(&store, RunStatus::Parked, Some("caller:busy")).await;
    tokio::time::sleep(Duration::from_millis(60)).await;

    let janitor = Janitor::new(fx.env.workspaces.clone(), None)
        .with_scratch_idle(Some(Duration::from_millis(30)));
    let report = janitor
        .sweep(store.as_ref(), &CancellationToken::new())
        .await;
    assert!(report.failed.is_empty(), "{report:?}");
    let images = fx.root.join("images");
    assert!(!images.join(&scopes[0]).exists(), "an idle conversation's");
    assert!(
        images.join(&scopes[1]).is_dir(),
        "a busy conversation's stays"
    );
    assert!(
        !images.join(&scopes[2]).exists(),
        "a run with no conversation's"
    );
}

// ---------------------------------------------------------------------------------------------
// The sweep of the scopes of images
// ---------------------------------------------------------------------------------------------

const AN_IMAGE: &[u8] = b"\x89PNG\r\n\x1a\nan image";

/// Keep an image under `scope` and say that `run` keeps it there, as the agent does.
async fn kept_for(fx: &Fixture, scope: &str, run: &RunId) {
    use adam_llm_agent::{KeptImages as _, sha256_hex};
    fx.env
        .images
        .note_run(scope, &run.to_string())
        .await
        .unwrap();
    fx.env
        .images
        .keep(scope, &sha256_hex(AN_IMAGE), "image/png", AN_IMAGE)
        .await
        .unwrap();
}

/// Make everything in a scope `days` old, as the janitor sees it.
fn age_scope(fx: &Fixture, scope: &str, days: u64) {
    let dir = fx.root.join("images").join(scope);
    let then = std::time::SystemTime::now() - Duration::from_secs(days * 24 * 60 * 60);
    for entry in std::fs::read_dir(&dir).unwrap() {
        let file = std::fs::File::options()
            .write(true)
            .open(entry.unwrap().path())
            .unwrap();
        file.set_modified(then).unwrap();
    }
    std::fs::File::open(&dir)
        .unwrap()
        .set_modified(then)
        .unwrap();
}

async fn sweep(
    fx: &Fixture,
    store: &DynStore,
    idle: Option<Duration>,
) -> adam_coder::janitor::Sweep {
    Janitor::new(fx.env.workspaces.clone(), None)
        .with_scratch_idle(idle)
        .sweep(store.as_ref(), &CancellationToken::new())
        .await
}

/// A scope a crash left with no workspace behind it (the workspace was removed and the sweep stopped
/// before the images) is removed by the sweep of scopes alone, once its conversation is idle.
#[tokio::test]
async fn a_scope_left_with_no_workspace_goes() {
    use adam_llm_agent::image_scope;
    let fx = Fixture::new().await;
    let store: DynStore = Arc::new(MemoryStore::new());
    let run = run_of(&store, RunStatus::Done, Some("caller:crashed")).await;
    let scope = image_scope(Some("caller:crashed"), &run.to_string());
    kept_for(&fx, &scope, &run).await;
    tokio::time::sleep(Duration::from_millis(60)).await;
    // Not idle long enough yet: the conversation may go on.
    let report = sweep(&fx, &store, Some(Duration::from_secs(3600))).await;
    assert!(report.images.is_empty(), "{report:?}");
    assert!(fx.root.join("images").join(&scope).is_dir());
    // Idle: the scope goes, with no workspace to sweep.
    let report = sweep(&fx, &store, Some(Duration::from_millis(30))).await;
    assert_eq!(report.images, std::slice::from_ref(&scope), "{report:?}");
    assert!(!fx.root.join("images").join(&scope).exists());
}

/// A run with no conversation keeps its scope while it is open and loses it when it is over.
#[tokio::test]
async fn the_scope_of_a_run_with_no_conversation_goes_when_the_run_is_over() {
    use adam_llm_agent::image_scope;
    let fx = Fixture::new().await;
    let store: DynStore = Arc::new(MemoryStore::new());
    let open = run_in(&store, RunStatus::Parked).await;
    let over = run_in(&store, RunStatus::Done).await;
    let unknown = RunId::new();
    let scopes: Vec<String> = [open, over, unknown]
        .iter()
        .map(|run| image_scope(None, &run.to_string()))
        .collect();
    for (run, scope) in [open, over, unknown].iter().zip(&scopes) {
        kept_for(&fx, scope, run).await;
    }
    let report = sweep(&fx, &store, Some(Duration::from_secs(3600))).await;
    let mut gone = report.images.clone();
    gone.sort();
    let mut want = vec![scopes[1].clone(), scopes[2].clone()];
    want.sort();
    assert_eq!(gone, want, "{report:?}");
    assert!(fx.root.join("images").join(&scopes[0]).is_dir());
}

/// A conversation with an open task keeps its images whatever `SCRATCH_IDLE_DAYS` is, `0` included
/// (the janitor then keeps no scratch project, and still no image of a conversation at work).
#[tokio::test]
async fn a_scope_of_a_conversation_with_an_open_run_stays_at_zero_idle() {
    use adam_llm_agent::image_scope;
    let fx = Fixture::new().await;
    let store: DynStore = Arc::new(MemoryStore::new());
    let older = run_of(&store, RunStatus::Done, Some("caller:busy")).await;
    run_of(&store, RunStatus::Parked, Some("caller:busy")).await;
    let scope = image_scope(Some("caller:busy"), &older.to_string());
    kept_for(&fx, &scope, &older).await;
    // The older run has a workspace, so the sweep of finished runs meets it too.
    workspace_of(&fx, &older.to_string()).await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    for idle in [None, Some(Duration::ZERO)] {
        let report = sweep(&fx, &store, idle).await;
        assert!(report.images.is_empty(), "{idle:?} {report:?}");
        assert!(
            fx.root.join("images").join(&scope).is_dir(),
            "the images of a conversation at work stay ({idle:?})"
        );
    }
}

/// A scope no note leads anywhere is removed when nothing was written in it for 30 days, and not
/// before. So is one whose note names a run of another conversation.
#[tokio::test]
async fn a_scope_nobody_can_place_goes_after_thirty_days() {
    use adam_llm_agent::{KeptImages as _, image_scope, sha256_hex};
    let fx = Fixture::new().await;
    let store: DynStore = Arc::new(MemoryStore::new());
    // No note at all (written by a build that kept none).
    let unnoted = image_scope(Some("caller:unnoted"), "r");
    fx.env
        .images
        .keep(&unnoted, &sha256_hex(AN_IMAGE), "image/png", AN_IMAGE)
        .await
        .unwrap();
    // A note that does not lead to the scope.
    let stranger = run_of(&store, RunStatus::Done, Some("caller:other")).await;
    let misnoted = image_scope(Some("caller:misnoted"), "r");
    kept_for(&fx, &misnoted, &stranger).await;
    let report = sweep(&fx, &store, None).await;
    assert!(report.images.is_empty(), "fresh: {report:?}");
    for days in [29, 31] {
        age_scope(&fx, &unnoted, days);
        age_scope(&fx, &misnoted, days);
        let report = sweep(&fx, &store, None).await;
        if days == 29 {
            assert!(report.images.is_empty(), "29 days: {report:?}");
        } else {
            let mut gone = report.images.clone();
            gone.sort();
            let mut want = vec![unnoted.clone(), misnoted.clone()];
            want.sort();
            assert_eq!(gone, want, "31 days: {report:?}");
        }
    }
    assert!(!fx.root.join("images").join(&unnoted).exists());
}
