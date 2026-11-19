//! Where the secrets that agent files name come from (ADR 0047, decision 5).
//!
//! An agent file names a secret by a variable: `auth: bearer:BILLING_TOKEN` on a remote subagent,
//! `${LINEAR_API_TOKEN}` in an `mcp.json`. A server reads it from its process environment
//! ([`ProcessEnv`], the default); an application that embeds adam has secrets of its own, and its
//! environment is the person's, so the engine's default ([`NoSecrets`]) refuses every name and the
//! host gives the ones it means to.

use std::fmt;
use std::sync::Arc;

use secrecy::SecretString;

/// Gives the value of a secret an agent file names. Asked each time a value is needed, never
/// cached by the caller; the value is never logged or put in an error.
pub trait SecretSource: Send + Sync + fmt::Debug {
    /// The value of `name`, or why there is none.
    ///
    /// # Errors
    ///
    /// [`SecretError`]: not set, not valid Unicode, or refused by this source.
    fn secret(&self, name: &str) -> Result<SecretString, SecretError>;
}

/// A shared [`SecretSource`].
pub type DynSecretSource = Arc<dyn SecretSource>;

/// Why a [`SecretSource`] has no value for a name. Carries no value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SecretError {
    /// The source would give it, and it is not set.
    #[error("it is not set")]
    Missing,
    /// It is set, and not valid Unicode.
    #[error("it is not valid Unicode")]
    NotUnicode,
    /// The source gives no value for this name, set or not: an embedded engine whose host did not
    /// give it.
    #[error("this host gives no value for it")]
    Refused,
}

/// The process environment: what a server reads its secrets from.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessEnv;

impl SecretSource for ProcessEnv {
    fn secret(&self, name: &str) -> Result<SecretString, SecretError> {
        match std::env::var(name) {
            Ok(value) => Ok(SecretString::from(value)),
            Err(std::env::VarError::NotPresent) => Err(SecretError::Missing),
            Err(std::env::VarError::NotUnicode(_)) => Err(SecretError::NotUnicode),
        }
    }
}

/// Refuses every name, whether the process environment has it or not: the default of an
/// embedded engine, whose environment is the person's and not a place for the agent's secrets.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoSecrets;

impl SecretSource for NoSecrets {
    fn secret(&self, _name: &str) -> Result<SecretString, SecretError> {
        Err(SecretError::Refused)
    }
}

#[cfg(test)]
mod tests {
    use secrecy::ExposeSecret;

    use super::*;

    /// A variable every test process has: cargo sets it for the test binary.
    const SET: &str = "CARGO_PKG_NAME";
    /// A name no environment has.
    const UNSET: &str = "ADAM_CORE_TEST_SURELY_UNSET_5C1E";

    #[test]
    fn the_process_environment_gives_what_it_has_and_says_what_it_lacks() {
        let value = ProcessEnv.secret(SET).unwrap();
        assert_eq!(value.expose_secret(), "adam-core");
        assert_eq!(ProcessEnv.secret(UNSET).unwrap_err(), SecretError::Missing);
    }

    #[test]
    fn no_secrets_refuses_a_name_the_environment_has() {
        assert!(std::env::var(SET).is_ok());
        assert_eq!(NoSecrets.secret(SET).unwrap_err(), SecretError::Refused);
        assert_eq!(NoSecrets.secret(UNSET).unwrap_err(), SecretError::Refused);
    }

    #[test]
    fn a_shared_source_is_asked_like_its_implementation() {
        let source: DynSecretSource = Arc::new(NoSecrets);
        assert_eq!(source.secret(SET).unwrap_err(), SecretError::Refused);
        assert_eq!(format!("{source:?}"), "NoSecrets");
    }

    #[test]
    fn the_errors_say_why_and_never_a_value() {
        assert_eq!(SecretError::Missing.to_string(), "it is not set");
        assert_eq!(
            SecretError::NotUnicode.to_string(),
            "it is not valid Unicode"
        );
        assert_eq!(
            SecretError::Refused.to_string(),
            "this host gives no value for it"
        );
    }
}
