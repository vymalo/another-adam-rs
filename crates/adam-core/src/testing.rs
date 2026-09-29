//! Helpers for tests that need an external service (Postgres, MongoDB).
//!
//! Such tests are gated on an environment variable such as
//! `ADAM_TEST_POSTGRES_URL` and skip (return early, green) when it is unset, so
//! `cargo test --workspace` works on a bare machine. That is the wrong default
//! in CI, where a missing variable would silently turn a whole suite into a
//! no-op. Setting `ADAM_TEST_REQUIRE_DB=1` flips it: an unset variable then
//! **panics**, failing the test with a message naming the variable. CI sets the
//! flag in every job that provides the databases.
//!
//! ```no_run
//! use adam_core::testing::test_env;
//!
//! // `None` (and a note on stderr) when the variable is unset and the flag is
//! // off; a panic when the flag is on.
//! let Some(_url) = test_env("ADAM_TEST_POSTGRES_URL") else { return };
//! ```
//!
//! Use [`test_env`] in every harness that gates on a database variable, and
//! [`skipped`] where a gate is decided some other way. Do not read the
//! variables directly in gated tests.

/// The environment variable that makes skipped database tests fail instead.
pub const REQUIRE_DB_VAR: &str = "ADAM_TEST_REQUIRE_DB";

/// Whether skipping a database-gated test is forbidden
/// (`ADAM_TEST_REQUIRE_DB` is `1` or `true`).
pub fn require_db() -> bool {
    matches!(
        std::env::var(REQUIRE_DB_VAR).as_deref(),
        Ok("1") | Ok("true")
    )
}

/// The value of the gate variable `var`, or `None` to skip the test.
///
/// A set but empty variable counts as unset. When it is unset and
/// [`require_db`] is on, this panics instead of returning `None`.
pub fn test_env(var: &str) -> Option<String> {
    match std::env::var(var) {
        Ok(value) if !value.is_empty() => Some(value),
        _ => {
            skipped(&format!("{var} is not set"));
            None
        }
    }
}

/// Report a skipped test: a note on stderr, or a panic when [`require_db`] is on.
///
/// # Panics
///
/// When `ADAM_TEST_REQUIRE_DB` is `1` or `true`.
#[track_caller]
pub fn skipped(reason: &str) {
    assert!(
        !require_db(),
        "test would be skipped ({reason}) but {REQUIRE_DB_VAR}=1 forbids skipping"
    );
    eprintln!("skipped: {reason}");
}

#[cfg(test)]
mod tests {
    use super::*;

    // Environment variables are process-global and tests run in parallel, so
    // these tests never set anything: they read PATH and a name nobody sets,
    // and follow whatever the flag is in the surrounding process.
    #[test]
    fn a_set_variable_is_returned() {
        assert!(test_env("PATH").is_some());
    }

    #[test]
    fn an_unset_variable_skips_unless_required() {
        let unset = "ADAM_TEST_DEFINITELY_UNSET_VARIABLE";
        if require_db() {
            let caught = std::panic::catch_unwind(|| test_env(unset));
            assert!(caught.is_err(), "must panic when the flag is on");
        } else {
            assert_eq!(test_env(unset), None);
        }
    }
}
