//! The in-memory store is the reference implementation of the suite.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

async fn make_store() -> Option<adam_core::DynStore> {
    Some(std::sync::Arc::new(adam_core::MemoryStore::new()))
}

adam_store_testkit::store_conformance!(make_store);

/// Prints the transcript measurement for the reference store. It is a measurement helper, not a
/// conformance case: it asserts only that the workload ran. S3 adds the `< 50 × 20 KB` and
/// `max < 20 KB` assertions here, when the contract release makes them true.
#[tokio::test]
async fn measure_the_transcript_write() {
    let store = make_store()
        .await
        .expect("the memory store is always configured");
    store.migrate().await.expect("migrate");
    let measured = adam_store_testkit::counting::measure_transcript_write(store).await;
    println!("{measured}");
    assert_eq!(measured.commits, 50, "the workload ran to the end");
    assert!(measured.total_written > 0, "the commits wrote state");
}
