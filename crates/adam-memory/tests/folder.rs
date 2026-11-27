//! The conformance suite against the in-folder adapter, and what only a folder can prove: a
//! restart on the same folder reads what was written, a fresh folder reads nothing, and the files
//! are plain JSON.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use adam_core::Timestamp;
use adam_memory::{
    AttemptDraft, AttemptId, DynSiMemory, FolderSiMemory, MemoryError, Scrubber, Settings, SiMemory,
};
use adam_memory_testkit::{Plan, draft, scope};
use tempfile::TempDir;

/// The temp directories the suite's stores read from. The generated tests keep no handle, so the
/// test binary holds them until it exits.
fn kept() -> &'static Mutex<Vec<TempDir>> {
    static KEPT: OnceLock<Mutex<Vec<TempDir>>> = OnceLock::new();
    KEPT.get_or_init(Default::default)
}

fn make(plan: &Plan) -> DynSiMemory {
    let dir = TempDir::new().expect("a temp dir");
    let store = FolderSiMemory::open(dir.path(), plan.settings.clone()).expect("open the folder");
    kept()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(dir);
    Arc::new(store)
}

adam_memory_testkit::si_memory_conformance!(crate::make);

fn settings() -> Settings {
    Settings::new(Scrubber::new(|text| text.to_owned()))
}

fn one_draft() -> AttemptDraft {
    draft("the lockfile is stale", "run/tool:1")
        .remedy("regenerate the lockfile")
        .predicted("the build resolves its dependencies")
}

#[test]
fn a_restart_on_the_same_folder_reads_what_was_written() {
    let dir = TempDir::new().unwrap();
    let scope = scope("restart");
    {
        let store = FolderSiMemory::open(dir.path(), settings()).unwrap();
        store.record(&scope, one_draft()).unwrap();
    }
    let reopened = FolderSiMemory::open(dir.path(), settings()).unwrap();
    let recalled = reopened.recall(&scope).unwrap();
    assert_eq!(recalled.len(), 1);
    assert_eq!(recalled[0].root_cause, "the lockfile is stale");
    assert_eq!(recalled[0].attempts[0].id, AttemptId::new("run/tool:1"));
    assert!(recalled[0].created <= Timestamp::now());
}

#[test]
fn a_fresh_folder_reads_nothing() {
    let dir = TempDir::new().unwrap();
    let store = FolderSiMemory::open(dir.path(), settings()).unwrap();
    assert!(store.recall(&scope("fresh")).unwrap().is_empty());
    assert_eq!(store.sweep(&scope("fresh"), Timestamp::now()).unwrap(), 0);
}

#[test]
fn the_folder_holds_plain_json_files() {
    let dir = TempDir::new().unwrap();
    let store = FolderSiMemory::open(dir.path(), settings()).unwrap();
    store.record(&scope("files"), one_draft()).unwrap();
    let files = json_files(dir.path());
    assert_eq!(files.len(), 1, "one problem is one JSON file");
    let text = std::fs::read_to_string(&files[0]).unwrap();
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(value.get("scope").is_some(), "{text}");
    assert_eq!(
        value["problem"]["root_cause"],
        serde_json::json!("the lockfile is stale")
    );
}

#[test]
fn two_scopes_share_a_folder_and_a_restart_keeps_them_apart() {
    let dir = TempDir::new().unwrap();
    let agent_scope = scope("two-scopes");
    let repository_scope = adam_memory_testkit::repository_scope("two-scopes");
    {
        let store = FolderSiMemory::open(dir.path(), settings()).unwrap();
        store.record(&agent_scope, one_draft()).unwrap();
        store
            .record(
                &repository_scope,
                draft("the repository's tests flake", "run/tool:2"),
            )
            .unwrap();
    }
    let reopened = FolderSiMemory::open(dir.path(), settings()).unwrap();
    assert_eq!(reopened.recall(&agent_scope).unwrap().len(), 1);
    assert_eq!(reopened.recall(&repository_scope).unwrap().len(), 1);
    assert_eq!(json_files(dir.path()).len(), 2, "a directory per scope");
}

#[test]
fn a_corrupt_file_is_reported_not_served() {
    let dir = TempDir::new().unwrap();
    let store = FolderSiMemory::open(dir.path(), settings()).unwrap();
    store.record(&scope("corrupt"), one_draft()).unwrap();
    let file = json_files(dir.path()).remove(0);
    std::fs::write(&file, "{ not json").unwrap();
    let error = store.recall(&scope("corrupt")).unwrap_err();
    assert!(matches!(error, MemoryError::Corrupt { .. }), "{error}");
}

fn json_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().and_then(|extension| extension.to_str()) == Some("json") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}
