//! A file stamped with a schema this binary does not know is refused, and left as it is.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::path::{Path, PathBuf};

use adam_core::{Store, StoreError};
use adam_error::{Classify, ErrorClass};
use adam_store_sqlite::{SCHEMA_VERSION, SqliteStore};
use uuid::Uuid;

fn unique(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(dir).expect("the test target directory");
    dir.join(format!("{name}-{}.sqlite", Uuid::new_v4()))
}

fn read_version(path: &Path) -> String {
    let conn = rusqlite::Connection::open(path).expect("a raw connection");
    conn.query_row(
        "SELECT value FROM adam_meta WHERE key = 'schema_version'",
        [],
        |row| row.get(0),
    )
    .expect("the schema version")
}

#[test]
fn a_newer_schema_is_refused_and_the_file_is_left_alone() {
    let database = unique("schema");
    let runtime = tokio::runtime::Runtime::new().expect("a runtime");

    runtime.block_on(async {
        let store = SqliteStore::open(&database).expect("open");
        store.migrate().await.expect("migrate");
        assert_eq!(read_version(&database), SCHEMA_VERSION.to_string());
    });

    // A newer release migrated the file: stamp a version this binary does not know.
    let newer = SCHEMA_VERSION + 1;
    let conn = rusqlite::Connection::open(&database).expect("a raw connection");
    conn.execute(
        "UPDATE adam_meta SET value = ?1 WHERE key = 'schema_version'",
        [newer.to_string()],
    )
    .expect("stamp the newer version");
    drop(conn);

    let store = SqliteStore::open(&database).expect("open the newer file");
    let error = runtime
        .block_on(store.migrate())
        .expect_err("a newer schema must be refused");
    assert!(
        matches!(error, StoreError::InvalidInput(_)),
        "a newer schema is a configuration error: {error:?}"
    );
    assert_eq!(error.class(), ErrorClass::Invalid, "{error:?}");
    let message = error.to_string();
    assert!(
        message.contains(&format!("schema version {newer}")),
        "{message}"
    );
    assert!(message.contains("newer than this binary"), "{message}");
    assert_eq!(
        read_version(&database),
        newer.to_string(),
        "a refused migration does not lower or rewrite the stamp"
    );

    let _ = std::fs::remove_file(&database);
}
