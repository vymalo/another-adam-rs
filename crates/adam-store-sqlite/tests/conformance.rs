//! The shared conformance suites, each case on its own file.
//!
//! SQLite needs no server and no environment variable, so `make_store` always answers `Some` and
//! the suite never skips. A file per case keeps the cases independent (the blob sweeps are global
//! to a database) and avoids the one-process lock refusing a second test.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::path::Path;
use std::sync::Arc;

use adam_core::DynStore;
use adam_store_sqlite::SqliteStore;
use uuid::Uuid;

async fn make_store() -> Option<DynStore> {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(dir).expect("the test target directory");
    let path = dir.join(format!("conformance-{}.sqlite", Uuid::new_v4()));
    let store = SqliteStore::open(&path).expect("open the SQLite store");
    Some(Arc::new(store))
}

adam_store_testkit::store_conformance!(make_store);
adam_store_testkit::blob_conformance!(make_store);
adam_store_testkit::body_conformance!(make_store);
