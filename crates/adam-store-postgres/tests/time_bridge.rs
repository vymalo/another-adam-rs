//! The `TIMESTAMPTZ` bridge: a `Timestamp` is encoded to the bytes chrono wrote, and decoded back.
//!
//! `PgTime` is private, so the proof goes through the public store and the wire: the column is
//! read as PostgreSQL's own `i64` (microseconds since 2000-01-01), and compared with what binding
//! a `chrono::DateTime<Utc>` to the same instant produces.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use adam_core::time::Timestamp;
use adam_core::{NewRun, RunId, RunStatus, Store};
use adam_error::Classify;
use adam_store_postgres::PgStore;
use serde_json::json;
use sqlx::error::BoxDynError;
use sqlx::postgres::{PgPoolOptions, PgTypeInfo, PgValueRef};
use sqlx::{AssertSqlSafe, Decode, PgPool, Postgres, Type};

/// PostgreSQL's binary `TIMESTAMPTZ`: microseconds since 2000-01-01T00:00:00Z, big-endian.
struct RawMicros(i64);

impl Type<Postgres> for RawMicros {
    fn type_info() -> PgTypeInfo {
        <chrono::DateTime<chrono::Utc> as Type<Postgres>>::type_info()
    }
}

impl<'r> Decode<'r, Postgres> for RawMicros {
    fn decode(value: PgValueRef<'r>) -> Result<Self, BoxDynError> {
        let bytes = value.as_bytes()?;
        let raw: [u8; 8] = bytes
            .try_into()
            .map_err(|_| "a TIMESTAMPTZ is eight bytes")?;
        Ok(Self(i64::from_be_bytes(raw)))
    }
}

/// The offset between the Unix epoch and PostgreSQL's 2000-01-01 epoch, in microseconds.
const PG_EPOCH_MICROS: i64 = 946_684_800_000_000;

fn chrono_of(at: Timestamp) -> chrono::DateTime<chrono::Utc> {
    let (second, nanosecond) = (at.as_second(), at.subsec_nanosecond());
    let (second, nanosecond) = if nanosecond < 0 {
        (second - 1, (nanosecond + 1_000_000_000) as u32)
    } else {
        (second, nanosecond as u32)
    };
    chrono::DateTime::from_timestamp(second, nanosecond).expect("in range")
}

async fn store(pool: PgPool) -> PgStore {
    let store = PgStore::from_pool(pool)
        .with_table_prefix("adam_pgtime_")
        .expect("prefix");
    store.migrate().await.expect("migrate");
    store
}

async fn pool() -> PgPool {
    let url = adam_core::testing::test_env("ADAM_TEST_POSTGRES_URL")
        .expect("the database suite needs ADAM_TEST_POSTGRES_URL");
    PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect")
}

/// What binding `at` as chrono's `DateTime<Utc>` writes on the wire: the bytes the old store wrote.
async fn chrono_wire_micros(pool: &PgPool, at: Timestamp) -> i64 {
    sqlx::query_scalar::<_, RawMicros>("SELECT $1::timestamptz")
        .bind(chrono_of(at))
        .fetch_one(pool)
        .await
        .expect("encode with chrono")
        .0
}

#[tokio::test]
async fn the_wire_bytes_are_the_ones_chrono_wrote() {
    let pool = pool().await;
    let store = store(pool.clone()).await;
    let agent = format!("pgtime-{}", uuid::Uuid::now_v7());

    // A whole millisecond, microseconds (truncated to the millisecond by the store, as before),
    // before 1970, the epoch, and the top of jiff's range (which sits inside 9999-12-31 by the
    // largest offset jiff allows). jiff's MIN (year -9999) is outside PostgreSQL's own range
    // (4713 BC), so neither the old nor the new store could write it.
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

        let raw = sqlx::query_scalar::<_, Option<RawMicros>>(AssertSqlSafe(
            "SELECT wake_at FROM adam_pgtime_runs WHERE id = $1".to_owned(),
        ))
        .bind(run.id.0)
        .fetch_one(&pool)
        .await
        .expect("read the raw column")
        .expect("a wake_at")
        .0;
        assert_eq!(
            raw,
            chrono_wire_micros(&pool, want).await,
            "{written}: the bridge wrote different bytes than chrono"
        );
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
    let pool = pool().await;
    let store = store(pool.clone()).await;
    let agent = format!("pgtime-{}", uuid::Uuid::now_v7());
    let run = store
        .create_run(NewRun::new(&agent, json!({})))
        .await
        .expect("create");
    let raw: Option<RawMicros> = sqlx::query_scalar(AssertSqlSafe(
        "SELECT wake_at FROM adam_pgtime_runs WHERE id = $1".to_owned(),
    ))
    .bind(run.id.0)
    .fetch_one(&pool)
    .await
    .expect("read the raw column");
    assert!(raw.is_none(), "no timer is SQL NULL");
    assert_eq!(store.load_run(run.id).await.unwrap().unwrap().wake_at, None);
    assert_eq!(
        store.run_header(run.id).await.unwrap().unwrap().wake_at,
        None
    );
}

/// A `TIMESTAMPTZ` outside jiff's range is a `Corrupt` store error, not a panic and not a wrap.
#[tokio::test]
async fn a_timestamp_outside_jiffs_range_is_corrupt() {
    let pool = pool().await;
    let store = store(pool.clone()).await;
    let agent = format!("pgtime-{}", uuid::Uuid::now_v7());
    let id = RunId(uuid::Uuid::now_v7());
    // Year 10000: inside PostgreSQL's and chrono's range, outside jiff's.
    let beyond = chrono::DateTime::from_timestamp(253_402_300_800, 0).expect("in range");
    sqlx::query(AssertSqlSafe(
        "INSERT INTO adam_pgtime_runs
             (id, agent, status, state, wake_at, sched_at, version, created_at, updated_at)
         VALUES ($1, $2, 'parked', '{}', $3, $3, 1, now(), now())"
            .to_owned(),
    ))
    .bind(id.0)
    .bind(&agent)
    .bind(beyond)
    .execute(&pool)
    .await
    .expect("insert a row adam could not have written");
    let error = store.load_run(id).await.expect_err("out of range");
    assert_eq!(error.class(), adam_error::ErrorClass::Corrupt, "{error:?}");
    // The same row's header path reads the same column and must fail the same way.
    let error = store.run_header(id).await.expect_err("out of range");
    assert_eq!(error.class(), adam_error::ErrorClass::Corrupt, "{error:?}");
}

/// A sub-millisecond cutoff is encoded with its microseconds, so `purge_finished` selects the
/// run exactly below it: the bridge does not truncate the comparison operand.
#[tokio::test]
async fn a_sub_millisecond_cutoff_keeps_its_microseconds() {
    let pool = pool().await;
    let store = store(pool.clone()).await;
    let agent = format!("pgtime-{}", uuid::Uuid::now_v7());
    let t: Timestamp = "2026-01-02T03:04:05.120Z".parse().unwrap();
    let later = t + jiff::SignedDuration::from_millis(1);
    for at in [t, later] {
        let id = RunId(uuid::Uuid::now_v7());
        sqlx::query(AssertSqlSafe(
            "INSERT INTO adam_pgtime_runs
                 (id, agent, status, state, wake_at, sched_at, version, created_at, updated_at)
             VALUES ($1, $2, 'done', '{}', NULL, NULL, 1, $3, $3)"
                .to_owned(),
        ))
        .bind(id.0)
        .bind(&agent)
        .bind(chrono_of(at))
        .execute(&pool)
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
