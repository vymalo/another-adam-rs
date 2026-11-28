//! The closed label sets the instruments may use ([ADR 0075] decision 3: a metric says nothing about a
//! person — no id, no token, no amount, and no value outside the set its instrument declares).
//!
//! A label is a value an instrument attaches to a measurement. The sets are **types**, not strings, so
//! a label outside its set cannot be constructed: a caller that tries gets an error at the boundary
//! instead of a new series in the backend that nobody declared, and the backend's cardinality stays
//! what the deployment planned for.
//!
//! [ADR 0075]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0075-metrics-and-traces-are-off-by-default-and-say-nothing-about-a-person.md

use std::fmt;

/// The lifecycle status of a run, as a label (`adam.run` instruments).
///
/// Closed on purpose: the four states of [`adam_core::store::RunStatus`] and nothing else. The test
/// below holds the two to each other, so a state added there without a line here fails a test rather
/// than shipping an instrument that can never report it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RunState {
    /// Ready to be advanced by a worker.
    Runnable,
    /// Waiting on a timer or an external event.
    Parked,
    /// Finished successfully.
    Done,
    /// Finished with an error.
    Failed,
}

impl RunState {
    /// Every state, in the order the instruments should list them.
    pub const ALL: [RunState; 4] = [Self::Runnable, Self::Parked, Self::Done, Self::Failed];

    /// The label value: `snake_case`, the spelling the store uses.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Runnable => "runnable",
            Self::Parked => "parked",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }

    /// The state a label names, or `None` for a value outside the set.
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|state| state.as_str() == value)
    }
}

impl fmt::Display for RunState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl TryFrom<&str> for RunState {
    type Error = LabelError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value).ok_or(LabelError::OutsideTheSet {
            value: value.to_owned(),
        })
    }
}

/// Why a label was refused. The class is [`adam_error::ErrorClass::Invalid`]: the same value never
/// becomes a label.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LabelError {
    /// A value the set does not hold.
    #[error("'{value}' is not a label of this set")]
    OutsideTheSet {
        /// The refused value, which is never a secret: a label is a name the caller chose.
        value: String,
    },
    /// A name a label could carry, but not one that is safe to export: empty, too long, or holding a
    /// character outside the label alphabet.
    #[error("'{value}' is not a label: {reason}")]
    NotAName {
        /// The refused value.
        value: String,
        /// Why it was refused.
        reason: &'static str,
    },
}

impl adam_error::Classify for LabelError {
    fn class(&self) -> adam_error::ErrorClass {
        adam_error::ErrorClass::Invalid
    }
}

/// The longest label value the sets accept. A tool name or a model alias is a name, not a sentence.
pub const MAX_LABEL_LEN: usize = 64;

/// A name an instrument may carry: a tool's name, a model alias, a store's pool.
///
/// The set of a name is the caller's (the tools an agent declares, the aliases a deployment allows).
/// What this type adds is the shape that is the same for all of them — non-empty, at most
/// [`MAX_LABEL_LEN`] bytes, and only `[a-z0-9._-]` — which keeps whitespace, paths, email addresses,
/// upper-case ids and long values out of a label by accident.
///
/// **It is not a secret filter.** A token can have that shape (`ghp_0123…` is lower-case, digits and
/// an underscore), so what keeps a secret out of a metric is the closed set: a label is one of
/// [`RunState`]'s values or a name the deployment declares, never a value at hand.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Name(String);

impl Name {
    /// The name, or why it cannot be a label.
    pub fn new(value: impl AsRef<str>) -> Result<Self, LabelError> {
        let value = value.as_ref();
        if value.is_empty() {
            return Err(LabelError::NotAName {
                value: value.to_owned(),
                reason: "empty",
            });
        }
        if value.len() > MAX_LABEL_LEN {
            return Err(LabelError::NotAName {
                value: value.to_owned(),
                reason: "longer than MAX_LABEL_LEN",
            });
        }
        if !value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
        {
            return Err(LabelError::NotAName {
                value: value.to_owned(),
                reason: "a character outside [a-z0-9._-]",
            });
        }
        Ok(Self(value.to_owned()))
    }

    /// The name as a label value.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use adam_error::Classify as _;

    /// The label set mirrors the store's: a state added to `RunStatus` without a line in `RunState`
    /// fails here, instead of shipping an instrument that can never report it.
    #[test]
    fn the_run_states_are_the_stores_states() {
        let store: Vec<&str> = adam_core::store::RunStatus::ALL
            .iter()
            .map(|status| status.as_str())
            .collect();
        let labels: Vec<&str> = RunState::ALL.iter().map(|state| state.as_str()).collect();
        assert_eq!(store, labels);
    }

    #[test]
    fn a_state_outside_the_set_cannot_be_built() {
        for value in ["running", "Runnable", "done ", "", "42"] {
            let error = RunState::try_from(value).expect_err(value);
            assert_eq!(error.class(), adam_error::ErrorClass::Invalid);
            assert!(matches!(error, LabelError::OutsideTheSet { .. }), "{value}");
        }
        for state in RunState::ALL {
            assert!(
                matches!(RunState::try_from(state.as_str()), Ok(parsed) if parsed == state),
                "{state} did not parse back"
            );
        }
    }

    #[test]
    fn a_name_outside_the_alphabet_cannot_be_built() {
        for value in [
            "",
            "a b",
            "Ada Lovelace",
            "/tmp/x",
            "a@b.example",
            "ünïcode",
            "UPPER",
            &"x".repeat(MAX_LABEL_LEN + 1),
        ] {
            let error = Name::new(value).expect_err(value);
            assert_eq!(error.class(), adam_error::ErrorClass::Invalid);
        }
        for value in [
            "bash",
            "rust-analyzer",
            "claude-sonnet-5",
            "store.pool",
            "a1",
        ] {
            assert_eq!(Name::new(value).expect(value).as_str(), value);
        }
    }

    /// The limit, asserted rather than glossed over: a token can have a name's shape, so the alphabet
    /// is not what keeps a secret out of a metric — the closed set is. This test exists so that no
    /// later reader takes the shape check for a secret filter.
    #[test]
    fn the_alphabet_is_not_a_secret_filter() {
        for value in [
            // A token, and an id: both are lower-case, digits and separators.
            "ghp_1234567890abcdef",
            "0123456789abcdef0123456789abcdef",
        ] {
            assert!(
                Name::new(value).is_ok(),
                "{value}: the shape check started claiming to be a secret filter, re-read the type's docs"
            );
        }
        // What the shape does catch: the usual accidental label.
        for value in [
            "Bearer abc",
            "/home/someone/x",
            "someone@example.com",
            "12.50 EUR",
        ] {
            assert!(Name::new(value).is_err(), "{value} passed the shape check");
        }
    }
}
