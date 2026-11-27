//! The store's database functions (ADR 0082): a fresh database gets exactly the functions this
//! list names, each answers a call, and a server below PostgreSQL 18 is refused before any DDL.
//!
//! A slice that adds a function adds it to [`FUNCTIONS`] with a call that exercises it, or
//! `every_function_is_created_and_answers` fails. Gated on `ADAM_TEST_POSTGRES_URL` like the
//! conformance suite (and failing instead of skipping under `ADAM_TEST_REQUIRE_DB=1`).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod support;

use std::collections::BTreeSet;

use adam_core::Store;
use adam_error::{Classify, ErrorClass};
use adam_store_postgres::{MIN_SERVER_VERSION_NUM, PgStore, SCHEMA_VERSION};
use deadpool_postgres::Pool;

/// Every function of the store's schema: its name without the table prefix, its identity
/// arguments as `pg_get_function_identity_arguments` prints them, and a statement that calls it,
/// `{p}` standing for the prefix. Empty at slice P0; P1 to P7 add theirs.
const FUNCTIONS: &[(&str, &str, &str)] = &[];

/// A fresh prefix of the test's own, so no other store's functions match it.
fn prefix(label: &str) -> String {
    format!(
        "adam_{label}_{}_",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    )
}

/// The functions of the current schema whose name starts with `p`, as `name(arguments)` with the
/// prefix taken off.
async fn functions_in_schema(pool: &Pool, p: &str) -> BTreeSet<String> {
    let client = pool.get().await.unwrap();
    client
        .query(
            "SELECT substr(p.proname::text, length($1) + 1)
                    || '(' || pg_get_function_identity_arguments(p.oid) || ')'
               FROM pg_proc p
              WHERE p.pronamespace = to_regnamespace(current_schema())
                AND starts_with(p.proname::text, $1)",
            &[&p],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect()
}

/// [`FUNCTIONS`] as `name(arguments)`.
fn listed(list: &[(&str, &str, &str)]) -> BTreeSet<String> {
    list.iter()
        .map(|(name, args, _)| format!("{name}({args})"))
        .collect()
}

/// What the schema has and the list does not, and what the list names and the schema lacks.
fn differences(schema: &BTreeSet<String>, list: &BTreeSet<String>) -> (Vec<String>, Vec<String>) {
    (
        schema.difference(list).cloned().collect(),
        list.difference(schema).cloned().collect(),
    )
}

async fn drop_store(pool: &Pool, p: &str) {
    let client = pool.get().await.unwrap();
    for (name, args, _) in FUNCTIONS {
        let _ = client
            .batch_execute(&format!("DROP FUNCTION IF EXISTS {p}{name}({args})"))
            .await;
    }
    for table in [
        "body", "run_blob", "blob", "push", "journal", "runs", "meta",
    ] {
        let _ = client
            .batch_execute(&format!("DROP TABLE IF EXISTS {p}{table} CASCADE"))
            .await;
    }
}

/// The decisive test of slice P0: a fresh database, migrated, holds exactly the listed functions,
/// and each one answers its call.
#[tokio::test]
async fn every_function_is_created_and_answers() {
    let Some(url) = support::test_url() else {
        return;
    };
    let pool = support::pool(&url, 2);
    let p = prefix("fn");
    let store = PgStore::from_pool(pool.clone())
        .with_table_prefix(&p)
        .unwrap();
    store.migrate().await.unwrap();

    let (unlisted, missing) =
        differences(&functions_in_schema(&pool, &p).await, &listed(FUNCTIONS));
    assert!(
        unlisted.is_empty(),
        "functions the schema has and FUNCTIONS does not list: {unlisted:?}"
    );
    assert!(
        missing.is_empty(),
        "functions FUNCTIONS lists and the schema lacks: {missing:?}"
    );
    let client = pool.get().await.unwrap();
    for (name, _, call) in FUNCTIONS {
        let call = call.replace("{p}", &p);
        client
            .batch_execute(&call)
            .await
            .unwrap_or_else(|e| panic!("{p}{name}: {call}: {e}"));
    }
    drop(client);
    drop_store(&pool, &p).await;
}

/// The comparison of the test above, broken on purpose both ways: a function the schema has that
/// the list does not name, and one the list names that the schema lacks, are each reported.
#[tokio::test]
async fn the_function_check_reports_an_unlisted_and_a_missing_function() {
    let Some(url) = support::test_url() else {
        return;
    };
    let pool = support::pool(&url, 2);
    let p = prefix("fncheck");
    let store = PgStore::from_pool(pool.clone())
        .with_table_prefix(&p)
        .unwrap();
    store.migrate().await.unwrap();
    support::exec(
        &pool,
        &format!("CREATE FUNCTION {p}stray(n integer) RETURNS integer LANGUAGE sql RETURN n"),
    )
    .await;

    let mut list: Vec<(&str, &str, &str)> = FUNCTIONS.to_vec();
    list.push(("ghost", "", "SELECT {p}ghost()"));
    let (unlisted, missing) = differences(&functions_in_schema(&pool, &p).await, &listed(&list));
    assert_eq!(unlisted, ["stray(n integer)"]);
    assert_eq!(missing, ["ghost()"]);

    support::exec(&pool, &format!("DROP FUNCTION {p}stray(integer)")).await;
    drop_store(&pool, &p).await;
}

/// A database of schema version 6 (the release before the functions) gets them on its next
/// start, and is stamped with the current version.
#[tokio::test]
async fn a_version_6_database_gets_the_functions() {
    let Some(url) = support::test_url() else {
        return;
    };
    let pool = support::pool(&url, 2);
    let p = prefix("fnup");
    let store = PgStore::from_pool(pool.clone())
        .with_table_prefix(&p)
        .unwrap();
    store.migrate().await.unwrap();
    // Back to version 6: no function, the version row at 6.
    let client = pool.get().await.unwrap();
    for (name, args, _) in FUNCTIONS {
        client
            .batch_execute(&format!("DROP FUNCTION {p}{name}({args})"))
            .await
            .unwrap();
    }
    drop(client);
    support::exec(
        &pool,
        &format!("UPDATE {p}meta SET value = '6' WHERE key = 'schema_version'"),
    )
    .await;
    assert!(functions_in_schema(&pool, &p).await.is_empty());

    store.migrate().await.unwrap();
    let version: String = support::scalar(
        &pool,
        &format!("SELECT value FROM {p}meta WHERE key = 'schema_version'"),
        &[],
    )
    .await;
    assert_eq!(version, SCHEMA_VERSION.to_string());
    assert_eq!(
        functions_in_schema(&pool, &p).await,
        listed(FUNCTIONS),
        "the upgrade made exactly the listed functions"
    );
    drop_store(&pool, &p).await;
}

/// A real server older than 18 is refused with `Invalid` before any DDL, and holds no object of
/// the prefix afterwards. CI runs no such server (every job runs 18), so this runs only where
/// `ADAM_TEST_POSTGRES_BELOW_FLOOR_URL` names one, and skips otherwise, even under
/// `ADAM_TEST_REQUIRE_DB=1`; the floor-raised test in the crate's unit tests is the one CI runs.
#[tokio::test]
async fn a_server_below_18_is_refused_before_any_ddl() {
    let Ok(url) = std::env::var("ADAM_TEST_POSTGRES_BELOW_FLOOR_URL") else {
        return;
    };
    let pool = support::pool(&url, 2);
    let version: i32 = support::scalar(
        &pool,
        "SELECT current_setting('server_version_num')::integer",
        &[],
    )
    .await;
    assert!(
        version < MIN_SERVER_VERSION_NUM,
        "ADAM_TEST_POSTGRES_BELOW_FLOOR_URL names a {version} server, not one below the floor"
    );
    let p = prefix("old");
    let store = PgStore::from_pool(pool.clone())
        .with_table_prefix(&p)
        .unwrap();
    let err = store.migrate().await.unwrap_err();
    assert_eq!(err.class(), ErrorClass::Invalid, "{err:?}");
    assert!(err.to_string().contains("is older than 18"), "{err}");
    let objects: i64 = support::scalar(
        &pool,
        "SELECT (SELECT count(*) FROM pg_class WHERE starts_with(relname::text, $1))
              + (SELECT count(*) FROM pg_proc WHERE starts_with(proname::text, $1))",
        &[&p],
    )
    .await;
    assert_eq!(objects, 0, "the refused migration created something");
}
