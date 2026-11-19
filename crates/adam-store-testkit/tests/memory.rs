//! The in-memory store is the reference implementation of the suite.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

async fn make_store() -> Option<adam_core::DynStore> {
    Some(std::sync::Arc::new(adam_core::MemoryStore::new()))
}

adam_store_testkit::store_conformance!(make_store);

adam_store_testkit::blob_conformance!(make_store);

adam_store_testkit::body_conformance!(make_store);

adam_store_testkit::transcript_measurement!(make_store);
