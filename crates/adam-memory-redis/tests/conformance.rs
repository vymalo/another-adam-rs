//! The `adam-memory-testkit` L2 suite against a real Redis: two `RedisMemory` with two
//! configured scopes over one server stand for two scopes on one database.
//!
//! ```sh
//! ADAM_TEST_REDIS_URL=redis://127.0.0.1:6379 \
//!     cargo test -p adam-memory-redis --test conformance
//! ```
//!
//! Skipped when the variable is unset, unless `ADAM_TEST_REQUIRE_DB=1` (then it fails).
//! Every test process gets its own key prefix, so the suite's fixed scopes and audiences never
//! read a previous run's lessons.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use adam_memory::{DynMemory, MemoryScope};
use adam_memory_testkit::LessonPlan;

/// The key prefix of this test process: fresh keys per run, so the suite never reads a previous
/// run's lessons.
fn prefix() -> &'static str {
    static PREFIX: OnceLock<String> = OnceLock::new();
    PREFIX.get_or_init(|| {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or(0);
        format!("adam-memory-redis:{}:{}", std::process::id(), nanos)
    })
}

/// Whether a Redis is configured: `ADAM_TEST_REDIS_URL`, honoured by the CI switch.
fn available() -> bool {
    adam_core::testing::test_env("ADAM_TEST_REDIS_URL").is_some()
}

fn make(plan: &LessonPlan, scope: &MemoryScope) -> DynMemory {
    let url = adam_core::testing::test_env("ADAM_TEST_REDIS_URL")
        .expect("the suite checked the variable");
    Arc::new(
        adam_memory_redis::RedisMemory::open(url, plan.settings.clone(), scope.clone())
            .expect("the URL parses")
            .with_key_prefix(prefix()),
    )
}

adam_memory_testkit::memory_conformance!(crate::make, crate::available);
