//! The janitor against real workspaces over a local bare remote and the in-memory store: which
//! runs lose their workspace, what stays, and how it runs as a component. (`tests/binary.rs` runs
//! it inside the process with Postgres.)
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use adam_coder::Janitor;
use adam_coder::tools::RunNotes;
use adam_core::{DynStore, MemoryStore, NewRun, RunId, RunStatus, RunUpdate};
use adam_workspace::RepoRef;
use common::{FakeEnvironment, Fixture};
use serde_json::json;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// A run in `store` with `status`.
async fn run_in(store: &DynStore, status: RunStatus) -> RunId {
    let id = RunId::new();
    store
        .create_run(NewRun {
            id,
            agent: "coder".to_owned(),
            conversation_id: None,
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

#[tokio::test]
async fn a_sweep_removes_what_is_over_and_keeps_what_is_open() {
    let fx = Fixture::new("hello\n").await;
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

/// A sweep that is told to stop between two runs stops.
#[tokio::test]
async fn a_cancelled_sweep_stops_before_it_removes_anything() {
    let fx = Fixture::new("hello\n").await;
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
    let fx = Fixture::new("hello\n").await;
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
    let fx = Fixture::new("hello\n").await;
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
    let fx = Fixture::new("hello\n").await;
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
    let fx = Fixture::new("hello\n").await;
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
    let fx = Fixture::new("hello\n").await;
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
