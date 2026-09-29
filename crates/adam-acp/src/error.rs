//! The crate's error type.

use std::time::Duration;

/// Everything that can go wrong while driving an ACP agent.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AcpError {
    /// The agent program could not be found or started.
    #[error("cannot start ACP agent `{program}`: {source}")]
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

impl AcpError {
    /// Whether retrying the operation on a **fresh** agent process may
    /// succeed. A crashed agent or a stalled turn is worth another attempt;
    /// a missing binary, bad configuration or protocol violation is not.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Exited { .. } | Self::Timeout { .. })
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
