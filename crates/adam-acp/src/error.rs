//! The crate's error type.

use std::time::Duration;

use adam_error::{Classify, ErrorClass};

/// Everything that can go wrong while driving an ACP agent.
///
/// Decide from [`Classify::class`]: `Exited` and `Timeout` are `Transient` (worth another
/// attempt on a fresh agent process), `AuthRequired` is `Unauthenticated`, `Config` and an RPC
/// error with code -32602 are `Invalid`, `Protocol` is `Corrupt`, `TurnInProgress` and `Closed`
/// are `Rejected`, and `Spawn` and any other RPC error are `Internal`. A message describes this
/// layer only; [`adam_error::report`] prints the chain.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AcpError {
    /// The agent program could not be found or started.
    #[error("cannot start ACP agent `{program}`")]
    Spawn {
        /// The program as configured (before path resolution).
        program: String,
        /// The underlying OS error.
        #[source]
        source: std::io::Error,
    },
    /// The agent process exited (or crashed) while it was still needed.
    #[error("ACP agent exited ({}){}", exit_desc(*.code), tail_desc(.stderr_tail))]
    Exited {
        /// The exit code, or `None` when the process was killed by a signal.
        code: Option<i32>,
        /// The last part of the agent's stderr (bounded), for diagnosis.
        stderr_tail: String,
    },
    /// An operation did not finish in time.
    #[error("ACP {operation} timed out after {after:?}")]
    Timeout {
        /// What timed out (`initialize`, `turn (no activity)`, ...).
        operation: &'static str,
        /// The limit that was exceeded.
        after: Duration,
    },
    /// The agent answered a request with a JSON-RPC error.
    #[error("ACP agent returned error {code}: {message}")]
    Rpc {
        /// JSON-RPC error code.
        code: i32,
        /// The agent's message.
        message: String,
    },
    /// The agent needs credentials before it can work (JSON-RPC code -32000),
    /// e.g. no provider is configured.
    #[error("ACP agent requires authentication: {0}")]
    AuthRequired(String),
    /// The agent violated the protocol or negotiated something unsupported.
    #[error("ACP protocol error: {0}")]
    Protocol(String),
    /// The configuration cannot be honoured (bad `fs_root`, unsupported option).
    #[error("invalid ACP configuration: {0}")]
    Config(String),
    /// A prompt was started while another turn is still running in the same
    /// session. Wait for `TurnEnded` (or cancel) first.
    #[error("a turn is already running in this ACP session")]
    TurnInProgress,
    /// The client was shut down; the connection is gone.
    #[error("the ACP connection is closed")]
    Closed,
}

/// JSON-RPC "invalid params".
const RPC_INVALID_PARAMS: i32 = -32602;

impl Classify for AcpError {
    /// [`is_retryable`](Classify::is_retryable) means: retrying the operation on a **fresh**
    /// agent process may succeed. A crashed agent or a stalled turn is worth another attempt;
    /// a missing binary, bad configuration or protocol violation is not.
    fn class(&self) -> ErrorClass {
        match self {
            Self::Exited { .. } | Self::Timeout { .. } => ErrorClass::Transient,
            Self::AuthRequired(_) => ErrorClass::Unauthenticated,
            Self::Config(_) => ErrorClass::Invalid,
            Self::Rpc { code, .. } if *code == RPC_INVALID_PARAMS => ErrorClass::Invalid,
            Self::Rpc { .. } | Self::Spawn { .. } => ErrorClass::Internal,
            Self::Protocol(_) => ErrorClass::Corrupt,
            Self::TurnInProgress | Self::Closed => ErrorClass::Rejected,
        }
    }
}

fn exit_desc(code: Option<i32>) -> String {
    match code {
        Some(c) => format!("code {c}"),
        None => "killed by signal".to_owned(),
    }
}

fn tail_desc(tail: &str) -> String {
    let tail = tail.trim_end();
    if tail.is_empty() {
        String::new()
    } else {
        format!("; stderr tail:\n{tail}")
    }
}

/// Result alias for this crate.
pub type AcpResult<T> = Result<T, AcpError>;

#[cfg(test)]
mod tests {
    use super::*;

    /// Exhaustive: a new variant forces a class decision here.
    fn expected(e: &AcpError) -> ErrorClass {
        match e {
            AcpError::Spawn { .. } => ErrorClass::Internal,
            AcpError::Exited { .. } => ErrorClass::Transient,
            AcpError::Timeout { .. } => ErrorClass::Transient,
            AcpError::Rpc { code: -32602, .. } => ErrorClass::Invalid,
            AcpError::Rpc { .. } => ErrorClass::Internal,
            AcpError::AuthRequired(_) => ErrorClass::Unauthenticated,
            AcpError::Protocol(_) => ErrorClass::Corrupt,
            AcpError::Config(_) => ErrorClass::Invalid,
            AcpError::TurnInProgress => ErrorClass::Rejected,
            AcpError::Closed => ErrorClass::Rejected,
        }
    }

    fn samples() -> Vec<AcpError> {
        vec![
            AcpError::Spawn {
                program: "opencode".into(),
                source: std::io::Error::from(std::io::ErrorKind::NotFound),
            },
            AcpError::Exited {
                code: Some(1),
                stderr_tail: String::new(),
            },
            AcpError::Timeout {
                operation: "initialize",
                after: Duration::from_secs(1),
            },
            AcpError::Rpc {
                code: -32602,
                message: "bad params".into(),
            },
            AcpError::Rpc {
                code: -32603,
                message: "boom".into(),
            },
            AcpError::AuthRequired("no provider".into()),
            AcpError::Protocol("x".into()),
            AcpError::Config("x".into()),
            AcpError::TurnInProgress,
            AcpError::Closed,
        ]
    }

    #[test]
    fn class_table() {
        for e in samples() {
            assert_eq!(e.class(), expected(&e), "{e}");
        }
        // Retry behaviour is unchanged: only a crashed agent or a stalled turn.
        let retryable: Vec<bool> = samples().iter().map(Classify::is_retryable).collect();
        assert_eq!(
            retryable,
            [
                false, true, true, false, false, false, false, false, false, false
            ]
        );
    }

    /// Regression for A4: `{:#}` printed a spawn failure's OS error twice.
    #[test]
    fn spawn_display_does_not_repeat_its_source() {
        let e = &samples()[0];
        let source = std::error::Error::source(e)
            .expect("source kept")
            .to_string();
        assert!(!e.to_string().contains(&source), "{e}");
        assert_eq!(
            adam_error::report(e),
            format!("cannot start ACP agent `opencode`: {source}")
        );
    }
}
