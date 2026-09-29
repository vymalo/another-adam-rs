//! The facade re-exports dev reload under its own feature `dev`, and only then.
#![cfg(feature = "dev")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // tests assert by unwrapping

use std::sync::Arc;

use adam::LiveAssembly;
use adam::model::MockModel;

#[test]
fn the_facade_reaches_dev_reload_with_the_feature() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("agent")).unwrap();
    std::fs::write(
        root.path().join("agent/instructions.md"),
        "---\nname: helper\n---\nBe brief.\n",
    )
    .unwrap();
    let live = LiveAssembly::builder(root.path(), Arc::new(MockModel::new()), "alias")
        .load()
        .unwrap();
    std::fs::write(
        root.path().join("agent/instructions.md"),
        "---\nname: helper\n---\nBe thorough.\n",
    )
    .unwrap();
    live.reload().unwrap();
    // The same type, by either path.
    let same: &adam::assembly::LiveAssembly = &live;
    assert_eq!(same.info()[0].prompt, "Be thorough.");
    assert_eq!(same.generation(), 2);
}
