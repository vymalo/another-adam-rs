//! A private database per test, on the server `ADAM_TEST_POSTGRES_URL` names.
//!
//! The runs are claimed *by agent name*, so two tests sharing tables would
//! steal each other's runs (and run them with the wrong mock). Each test
//! therefore gets a database of its own, created here and dropped by
//! [`TestDb::finish`]. A test that panics leaks its database; the next
//! [`TestDb::create`] anywhere drops the leaked ones older than an hour.
//!
//! Needs a role with `CREATEDB` (the local and CI `postgres` superuser has it).
#![allow(dead_code)]

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use adam_core::DynStore;
use adam_postgres::PgConfig;
use adam_store_postgres::PgStore;
use tokio_postgres::Client;
use url::Url;

/// Prefix of every database made here; the rest is `<unix seconds>_<random>`.
const PREFIX: &str = "coder_test_";
const STALE_AFTER_SECS: u64 = 3600;

/// The server URL from the environment, if the Postgres tests are enabled.
/// Unset with `ADAM_TEST_REQUIRE_DB=1` panics (see `adam_core::testing`).
pub fn server_url() -> Option<String> {
    adam_core::testing::test_env("ADAM_TEST_POSTGRES_URL")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn with_database(server: &str, database: &str) -> String {
    let mut url = Url::parse(server).expect("ADAM_TEST_POSTGRES_URL is a URL");
    url.set_path(&format!("/{database}"));
    url.to_string()
}

async fn admin(server: &str) -> Client {
    PgConfig::parse(server)
        .expect("ADAM_TEST_POSTGRES_URL is a URL")
        .connect()
        .await
        .expect("connect to the test Postgres")
}

/// One throwaway database.
pub struct TestDb {
    server: String,
    name: String,
    store: Option<PgStore>,
}

impl TestDb {
    /// Create a migrated database, or `None` when Postgres tests are not
    /// enabled (`ADAM_TEST_POSTGRES_URL` unset). With
    /// `ADAM_TEST_REQUIRE_DB=1` a missing URL is a failure instead.
    pub async fn create() -> Option<Self> {
        let server = server_url()?;
        let mut admin = admin(&server).await;
        drop_stale(&mut admin).await;
        let name = format!(
            "{PREFIX}{}_{}",
            now_secs(),
            uuid::Uuid::new_v4().simple().to_string()[..12].to_owned()
        );
        admin
            .batch_execute(&format!("CREATE DATABASE {name}"))
            .await
            .expect("create the test database (does the role have CREATEDB?)");
        drop(admin);
        let store = PgStore::connect(&with_database(&server, &name))
            .await
            .expect("connect to the test database");
        adam_core::Store::migrate(&store)
            .await
            .expect("migrate the test database");
        Some(Self {
            server,
            name,
            store: Some(store),
        })
    }

    /// `DATABASE_URL` for a process that should use this database.
    pub fn url(&self) -> String {
        with_database(&self.server, &self.name)
    }

    /// The store over it.
    pub fn store(&self) -> DynStore {
        Arc::new(self.store.clone().expect("not finished"))
    }

    /// The raw store (pool access for assertions).
    pub fn pg(&self) -> &PgStore {
        self.store.as_ref().expect("not finished")
    }

    /// Drop the database. Call it at the end of a passing test.
    pub async fn finish(mut self) {
        if let Some(store) = self.store.take() {
            store.pool().close();
        }
        let admin = admin(&self.server).await;
        admin
            .batch_execute(&format!("DROP DATABASE IF EXISTS {}", self.name))
            .await
            .expect("drop the test database");
    }
}

/// Drop databases of earlier, panicked tests.
async fn drop_stale(admin: &mut Client) {
    let names: Vec<String> = admin
        .query(
            "SELECT datname FROM pg_database WHERE datname LIKE 'coder\\_test\\_%'",
            &[],
        )
        .await
        .unwrap_or_default()
        .iter()
        .map(|row| row.get(0))
        .collect();
    for name in names {
        let created: u64 = name
            .strip_prefix(PREFIX)
            .and_then(|rest| rest.split('_').next())
            .and_then(|secs| secs.parse().ok())
            .unwrap_or(0);
        if now_secs().saturating_sub(created) > STALE_AFTER_SECS {
            let _ = admin
                .batch_execute(&format!("DROP DATABASE IF EXISTS {name}"))
                .await;
        }
    }
}
