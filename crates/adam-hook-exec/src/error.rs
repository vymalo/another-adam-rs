//! Why the adapter could not start, or could not get a verdict.
//!
//! Every variant carries an [`adam_error::ErrorClass`] ([`Classify`], Rule 6 of the repository): a
//! start that will never work is `Invalid` (a binary reports it as exit 78), a program that died,
//! missed its deadline or is restarting is `Transient` (the transition is tried again), and an
//! answer the protocol does not allow is `Invalid` (the call is refused, never let through).

use adam_error::{Classify, ErrorClass};
use thiserror::Error;

/// Why the adapter failed.
#[derive(Debug, Error)]
pub enum Error {
    /// The program could not be started at all: a bad path, a missing file, a refused permission.
    #[error("the program {program} could not be started: {message}")]
    Spawn {
        /// The program, as the configuration named it.
        program: String,
        /// What the operating system said.
        message: String,
    },

    /// The program did not answer the hello within the start's deadline.
    #[error("the program {program} did not answer the hello within {millis} ms")]
    Hello {
        /// The program, as the configuration named it.
        program: String,
        /// How long it was given.
        millis: u64,
    },

    /// The program answered the hello with another protocol.
    #[error("the program {program} speaks {found:?}, not {expected:?}")]
    Protocol {
        /// The program, as the configuration named it.
        program: String,
        /// The protocol the adapter speaks.
        expected: String,
        /// What the program named.
        found: String,
    },

    /// The program wrote a line that is not one of the protocol's messages.
    #[error("the program {program} wrote {what} where a {expected} line was expected")]
    Malformed {
        /// The program, as the configuration named it.
        program: String,
        /// What was expected, e.g. "a JSON object".
        expected: String,
        /// What arrived, cut for the message.
        what: String,
    },

    /// The program died or is restarting; its deciding points are transient until it is back.
    #[error("the program {program} is not running: {message}")]
    Down {
        /// The program, as the configuration named it.
        program: String,
        /// What happened.
        message: String,
    },

    /// A request passed its deadline.
    #[error("the request to {program} passed its {millis} ms deadline")]
    Deadline {
        /// The program, as the configuration named it.
        program: String,
        /// The deadline the request was given.
        millis: u64,
    },

    /// The program answered something the point does not allow.
    #[error("the program {program} answered {what}")]
    Reply {
        /// The program, as the configuration named it.
        program: String,
        /// What it answered, as a person reads it.
        what: String,
    },

    /// The program asked for an action this build cannot perform.
    #[error(
        "the program {program} asked for an action, and no action is built yet (ADR 0050, slice H1d)"
    )]
    Action {
        /// The program, as the configuration named it.
        program: String,
    },

    /// The configuration is not usable: a deadline out of bounds, a dead subscription pattern.
    #[error("the configuration is not usable: {message}")]
    Config {
        /// What is wrong.
        message: String,
    },
}

impl Classify for Error {
    fn class(&self) -> ErrorClass {
        match self {
            // A program that cannot start, speaks another protocol, or answers what the point does
            // not allow will never work with this configuration: the binary reports it as exit 78.
            Error::Spawn { .. }
            | Error::Hello { .. }
            | Error::Protocol { .. }
            | Error::Malformed { .. }
            | Error::Reply { .. }
            | Error::Config { .. } => ErrorClass::Invalid,
            // A dead program stops calls until it is back, and a missed deadline is retried.
            Error::Down { .. } | Error::Deadline { .. } => ErrorClass::Transient,
            Error::Action { .. } => ErrorClass::Unsupported,
        }
    }
}
