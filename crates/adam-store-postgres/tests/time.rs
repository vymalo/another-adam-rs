//! `TIMESTAMPTZ` is bound through `postgres-types`' `with-jiff-0_2`: microseconds since
//! 2000-01-01T00:00:00Z, the same bytes `sqlx` wrote with chrono. The proof goes through the
//! public store and the wire: the column is read as PostgreSQL's own `i64` and compared with the
//! value the instant implies.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod support;

use std::error::Error;

use adam_core::time::Timestamp;
use adam_core::{NewRun, RunId, RunStatus, Store};
use adam_error::Classify;
use adam_store_postgres::PgStore;
use serde_json::json;
use tokio_postgres::types::{FromSql, Type};

/// PostgreSQL's binary `TIMESTAMPTZ`: microseconds since 2000-01-01T00:00:00Z, big-endian.
struct RawMicros(i64);

impl<'a> FromSql<'a> for RawMicros {
    fn from_sql(_: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn Error + Sync + Send>> {
        let bytes: [u8; 8] = raw.try_into().map_err(|_| "a TIMESTAMPTZ is eight bytes")?;
        Ok(Self(i64::from_be_bytes(bytes)))
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::TIMESTAMPTZ
    }
}

/// The offset between the Unix epoch and PostgreSQL's 2000-01-01 epoch, in microseconds.
const PG_EPOCH_MICROS: i64 = 946_684_800_000_000;

async fn store(url: &str) -> (PgStore, deadpool_postgres::Pool) {
    let pool = support::pool(url, 4);
    let store = PgStore::from_pool(pool.clone())
        .with_table_prefix("adam_pgtime_")
        .expect("prefix");
    store.migrate().await.expect("migrate");
    (store, pool)
}

async fn raw_micros(pool: &deadpool_postgres::Pool, id: RunId) -> Option<i64> {
    let client = pool.get().await.unwrap();
    client
        .query_one(
            "SELECT wake_at FROM adam_pgtime_runs WHERE id = $1",
            &[&id.0],
        )
        .await
        .expect("read the raw column")
        .get::<_, Option<RawMicros>>(0)
        .map(|raw| raw.0)
}

#[tokio::test]
async fn the_wire_bytes_are_microseconds_since_2000() {
    let Some(url) = support::test_url() else {
        return;
    };
    let (store, pool) = store(&url).await;
    let agent = format!("pgtime-{}", uuid::Uuid::now_v7());

    // A whole millisecond, microseconds (truncated to the millisecond by the store, as before),
    // before 1970, the epoch, and the top of jiff's range.
    let cases: [(Timestamp, Timestamp); 7] = [
        (
            "2026-01-02T03:04:05.120Z".parse().unwrap(),
            "2026-01-02T03:04:05.120Z".parse().unwrap(),
        ),
        (
            "2026-01-02T03:04:05.123456Z".parse().unwrap(),
            "2026-01-02T03:04:05.123Z".parse().unwrap(),
        ),
        (
            "1969-12-31T23:59:59.500Z".parse().unwrap(),
            "1969-12-31T23:59:59.500Z".parse().unwrap(),
        ),
        (
            "1969-12-31T23:59:59.999999Z".parse().unwrap(),
            "1969-12-31T23:59:59.999Z".parse().unwrap(),
        ),
        (
            "1970-01-01T00:00:00Z".parse().unwrap(),
            "1970-01-01T00:00:00Z".parse().unwrap(),
        ),
        (
            "2026-01-02T03:04:05Z".parse().unwrap(),
            "2026-01-02T03:04:05Z".parse().unwrap(),
        ),
        (
            Timestamp::MAX,
            adam_core::store::truncate_ms(Timestamp::MAX),
        ),
    ];
    for (at, want) in cases {
        let written = at;
        let run = store
            .create_run(
                NewRun::new(&agent, json!({}))
                    .status(RunStatus::Parked)
                    .wake_at(at),
            )
            .await
            .expect("create");
        assert_eq!(run.wake_at, Some(want), "{written} is truncated");
        assert_eq!(
            store.load_run(run.id).await.expect("load").unwrap().wake_at,
            Some(want),
            "{written} decodes"
        );

        let raw = raw_micros(&pool, run.id).await.expect("a wake_at");
        assert_eq!(
            raw,
            want.as_millisecond() * 1_000 - PG_EPOCH_MICROS,
            "{written}: the wire is not microseconds since 2000"
        );
    }
}

/// A `NULL` stays `NULL`: a run with no timer loads with no `wake_at`.
#[tokio::test]
async fn a_null_wake_at_is_none() {
    let Some(url) = support::test_url() else {
        return;
    };
    let (store, pool) = store(&url).await;
    let agent = format!("pgtime-{}", uuid::Uuid::now_v7());
    let run = store
        .create_run(NewRun::new(&agent, json!({})))
        .await
        .expect("create");
    assert!(
        raw_micros(&pool, run.id).await.is_none(),
        "no timer is SQL NULL"
    );
    assert_eq!(store.load_run(run.id).await.unwrap().unwrap().wake_at, None);
    assert_eq!(
        store.run_header(run.id).await.unwrap().unwrap().wake_at,
        None
    );
}

/// A `TIMESTAMPTZ` outside jiff's range is a `Corrupt` store error, not a panic and not a wrap.
#[tokio::test]
async fn a_timestamp_outside_jiffs_range_is_corrupt() {
    let Some(url) = support::test_url() else {
        return;
    };
    let (store, pool) = store(&url).await;
    let agent = format!("pgtime-{}", uuid::Uuid::now_v7());
    let id = RunId(uuid::Uuid::now_v7());
    // Year 10000: inside PostgreSQL's range, outside jiff's.
    let client = pool.get().await.unwrap();
    client
        .batch_execute(&format!(
            "INSERT INTO adam_pgtime_runs
                 (id, agent, status, state, wake_at, sched_at, version, created_at, updated_at)
             VALUES ('{id}', '{agent}', 'parked', '{{}}',
                     '10000-01-01 00:00:00+00'::timestamptz,
                     '10000-01-01 00:00:00+00'::timestamptz, 1, now(), now())"
        ))
        .await
        .expect("insert a row adam could not have written");
    let error = store.load_run(id).await.expect_err("out of range");
    assert_eq!(error.class(), adam_error::ErrorClass::Corrupt, "{error:?}");
    // The same row's header path reads the same column and must fail the same way.
    let error = store.run_header(id).await.expect_err("out of range");
    assert_eq!(error.class(), adam_error::ErrorClass::Corrupt, "{error:?}");
}

/// A sub-millisecond cutoff is encoded with its microseconds, so `purge_finished` selects the
/// run exactly below it: the binding does not truncate the comparison operand.
#[tokio::test]
async fn a_sub_millisecond_cutoff_keeps_its_microseconds() {
    let Some(url) = support::test_url() else {
        return;
    };
    let (store, pool) = store(&url).await;
    let agent = format!("pgtime-{}", uuid::Uuid::now_v7());
    let t: Timestamp = "2026-01-02T03:04:05.120Z".parse().unwrap();
    let later = t + jiff::SignedDuration::from_millis(1);
    let client = pool.get().await.unwrap();
    for at in [t, later] {
        let id = RunId(uuid::Uuid::now_v7());
        client
            .execute(
                "INSERT INTO adam_pgtime_runs
                     (id, agent, status, state, wake_at, sched_at, version, created_at, updated_at)
                 VALUES ($1, $2, 'done', '{}', NULL, NULL, 1, $3, $3)",
                &[&id.0, &agent, &at.0],
            )
            .await
            .expect("insert a finished run");
    }
    let cutoff = t + jiff::SignedDuration::from_micros(500);
    assert_eq!(
        store.purge_finished(&agent, cutoff).await.unwrap(),
        1,
        "only the run strictly before the cutoff goes"
    );
    assert_eq!(
        store.purge_finished(&agent, cutoff).await.unwrap(),
        0,
        "the run at +1 ms stays"
    );
}
