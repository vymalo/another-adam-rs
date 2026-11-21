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

/// A file a previous release stamped with an older version is upgraded in place: the DDL is
/// idempotent and the stamp moves to [`SCHEMA_VERSION`].
#[test]
fn an_older_schema_is_upgraded_in_place() {
    let database = unique("older-schema");
    {
        let conn = rusqlite::Connection::open(&database).expect("a raw connection");
        conn.execute_batch(
            "CREATE TABLE adam_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO adam_meta (key, value) VALUES ('schema_version', '0');",
        )
        .expect("stamp an older version");
    }

    let runtime = tokio::runtime::Runtime::new().expect("a runtime");
    runtime.block_on(async {
        let store = SqliteStore::open(&database).expect("open the older file");
        store.migrate().await.expect("an older schema upgrades");
    });
    assert_eq!(
        read_version(&database),
        SCHEMA_VERSION.to_string(),
        "the upgrade stamps this binary's version"
    );

    let _ = std::fs::remove_file(&database);
}

/// A file that is not a SQLite database is a **corrupt** store (exit 70), not a configuration
/// mistake (exit 78): the bytes are damaged, and no `DATABASE_URL` fixes them.
#[test]
fn a_corrupt_file_is_refused_as_corrupt() {
    let database = unique("corrupt");
    std::fs::write(
        &database,
        b"this is not a SQLite database, and the header says so: ".repeat(8),
    )
    .expect("write a corrupt file");

    let error = SqliteStore::open(&database).expect_err("a corrupt file must be refused");
    assert_eq!(error.class(), ErrorClass::Corrupt, "{error:?}");
    assert!(
        error.to_string().contains("not a database"),
        "the message says what is wrong: {error}"
    );

    let _ = std::fs::remove_file(&database);
    let _ = std::fs::remove_file(lock_of(&database));
}

fn lock_of(path: &Path) -> PathBuf {
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    PathBuf::from(lock)
}
