//! Runs the shared conformance suite against a real MongoDB.
//!
//! ```sh
//! ADAM_TEST_MONGODB_URI=mongodb://localhost:27017 cargo test -p adam-store-mongodb
//! ```
//! Skipped when the variable is unset, unless `ADAM_TEST_REQUIRE_DB=1` (then it fails).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;

use adam_store_mongodb::MongoStore;

async fn make_store() -> Option<adam_core::DynStore> {
    let uri = adam_core::testing::test_env("ADAM_TEST_MONGODB_URI")?;
    let db = std::env::var("ADAM_TEST_MONGODB_DB").unwrap_or_else(|_| "adam_test".into());
    let store = MongoStore::connect(&uri, &db).await.expect("connect");
    Some(Arc::new(store))
}

adam_store_testkit::store_conformance!(make_store);
