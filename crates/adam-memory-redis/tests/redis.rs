//! The adapter's own three cases: a stored value that is not a lesson is `Corrupt`, a dead port
//! is a `Transient` backend failure, and an empty scope is refused before any connection. The
//! last needs no Redis; the first two need no Redis unless they run.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use adam_core::testing::test_env;
use adam_error::{Classify, ErrorClass};
use adam_memory::{Audience, LessonSettings, Memory, MemoryError, MemoryScope, Scrubber};
use adam_memory_redis::RedisMemory;
use std::net::TcpListener;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// The settings of the adapter under test: a scrubber that changes nothing.
fn settings() -> LessonSettings {
    LessonSettings::new(Scrubber::new(|text| text.to_owned()))
}

/// The key prefix of this test process, so a run never reads a previous run's keys.
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

/// A value under a key the adapter owns that is not a `Lesson` makes `recall` answer `Corrupt`.
#[test]
fn a_value_that_is_not_a_lesson_is_corrupt() {
    let Some(url) = test_env("ADAM_TEST_REDIS_URL") else {
        return;
    };
    let audience = Audience::new("corrupt");
    let memory = RedisMemory::open(&url, settings(), MemoryScope::new("corrupt-scope"))
        .expect("the URL parses")
        .with_key_prefix(prefix());
    // The key the adapter owns for this scope and audience: <prefix>:<scope bytes>:<scope>:<audience>.
    let key = format!(
        "{}:{}:corrupt-scope:corrupt",
        prefix(),
        "corrupt-scope".len()
    );
    let mut conn = redis::Client::open(url.as_str())
        .expect("the URL parses")
        .get_connection()
        .expect("a connection");
    redis::cmd("HSET")
        .arg(&key)
        .arg("a field this adapter never wrote")
        .arg("not a lesson")
        .exec(&mut conn)
        .expect("the junk lands");
    let error = memory
        .recall(&audience)
        .expect_err("a stored value that is not a lesson is corrupt");
    assert!(matches!(error, MemoryError::Corrupt { .. }), "{error}");
}

/// An adapter configured with an empty scope refuses `recall` and `retire` before any
/// connection: the case needs no server and runs with no URL configured.
#[test]
fn an_empty_scope_is_refused_before_any_connection() {
    // The address is never reached: the refusal comes before any connection is opened.
    let memory = RedisMemory::open("redis://127.0.0.1:1/", settings(), MemoryScope::new(""))
        .expect("the URL parses");
    let audience = Audience::new("x");
    let error = memory
        .recall(&audience)
        .expect_err("an empty scope is refused");
    assert!(
        matches!(
            error,
            MemoryError::EmptyField {
                field: "memory scope"
            }
        ),
        "{error}"
    );
    let error = memory
        .retire(&audience, "anything")
        .expect_err("an empty scope is refused");
    assert!(
        matches!(
            error,
            MemoryError::EmptyField {
                field: "memory scope"
            }
        ),
        "{error}"
    );
}

/// A URL pointing at a closed port answers a backend failure whose class is `Transient`.
#[test]
fn a_dead_port_is_a_transient_backend_failure() {
    // Bind a loopback port, read it and drop the listener: the port is closed, no server needed.
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let port = listener.local_addr().expect("the address").port();
    drop(listener);
    let memory = RedisMemory::open(
        format!("redis://127.0.0.1:{port}"),
        settings(),
        MemoryScope::new("transient"),
    )
    .expect("the URL parses");
    let error = memory
        .recall(&Audience::new("nobody"))
        .expect_err("the port is closed");
    assert!(matches!(error, MemoryError::Backend { .. }), "{error}");
    assert_eq!(error.class(), ErrorClass::Transient);
}
