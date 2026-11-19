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

/// Gives the value of a secret an agent file names. Asked when an agent is bound (a remote
/// subagent's bearer) and when its MCP servers are connected (`${VAR}`); what it gives is kept for
/// the life of that binding or connection, and never logged or put in an error.
///
/// The call is synchronous and made on the caller's async runtime: a source answers from memory. A
/// host whose secrets are in a vault fetches them first, and gives them here or through
/// `AgentDef::env`. `Debug` of a source, and of what holds one, shows [`kind`](Self::kind) only,
/// never a value: a host's source needs no `Debug` of its own.
pub trait SecretSource: Send + Sync {
    /// The value of `name`, or why there is none.
    ///
    /// # Errors
    ///
    /// [`SecretError`]: not set, not valid Unicode, or refused by this source.
    fn secret(&self, name: &str) -> Result<SecretString, SecretError>;

    /// What this source is, for `Debug` and startup logs. Never a value.
    fn kind(&self) -> &'static str {
        "host"
    }
}

impl fmt::Debug for dyn SecretSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SecretSource({})", self.kind())
    }
}

/// A shared [`SecretSource`].
pub type DynSecretSource = Arc<dyn SecretSource>;

/// Why a [`SecretSource`] has no value for a name. Carries no value. Not exhaustive, so a reason
/// can be added without a break: a reader treats one it does not know as [`Refused`](Self::Refused),
/// which fails closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
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
    fn kind(&self) -> &'static str {
        "process environment"
    }

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
    fn kind(&self) -> &'static str {
        "none"
    }

    fn secret(&self, _name: &str) -> Result<SecretString, SecretError> {
        Err(SecretError::Refused)
    }
}

#[cfg(test)]
mod tests {
    use secrecy::ExposeSecret;

    use super::*;

    /// A variable every process has.
    const SET: &str = "PATH";
    /// A name no environment has.
    const UNSET: &str = "ADAM_CORE_TEST_SURELY_UNSET_5C1E";

    #[test]
    fn the_process_environment_gives_what_it_has_and_says_what_it_lacks() {
        let value = ProcessEnv.secret(SET).unwrap();
        assert_eq!(value.expose_secret(), std::env::var(SET).unwrap());
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
        assert_eq!(format!("{source:?}"), "SecretSource(none)");
        let process: DynSecretSource = Arc::new(ProcessEnv);
        assert_eq!(format!("{process:?}"), "SecretSource(process environment)");
    }

    /// A host's source with values, and a derived `Debug` that would show them: what holds it
    /// shows its kind only.
    #[derive(Debug)]
    struct Map(Vec<(&'static str, &'static str)>);

    impl SecretSource for Map {
        fn secret(&self, name: &str) -> Result<SecretString, SecretError> {
            self.0
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| SecretString::from(*v))
                .ok_or(SecretError::Missing)
        }
    }

    #[test]
    fn a_held_source_never_shows_its_values() {
        let source: DynSecretSource = Arc::new(Map(vec![("TOKEN", "s3cret-value")]));
        assert_eq!(
            source.secret("TOKEN").unwrap().expose_secret(),
            "s3cret-value"
        );
        let shown = format!("{source:?}");
        assert_eq!(shown, "SecretSource(host)");
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
