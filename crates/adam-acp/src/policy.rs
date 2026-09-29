//! What the agent may do through the client: file access and permission
//! decisions.

use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::guard::FsGuard;

/// Client-side capabilities, and what the agent may do through them.
#[derive(Debug, Clone)]
pub struct ClientPolicy {
    /// `fs/read_text_file` and `fs/write_text_file` are allowed only under
    /// this directory. It is canonicalised at spawn time; symlink and `..`
    /// escapes are rejected. Must exist.
    pub fs_root: PathBuf,
    /// How `session/request_permission` is answered.
    pub permission: PermissionMode,
    /// Advertise `terminal/*`. Not implemented yet: `true` makes
    /// [`AcpClient::spawn`](crate::AcpClient::spawn) fail with
    /// [`AcpError::Config`](crate::AcpError::Config) rather than advertising
    /// a capability the client cannot serve.
    pub terminal: bool,
}

impl ClientPolicy {
    /// The default policy for `fs_root`: [`PermissionMode::AllowWithinRoot`],
    /// no terminal.
    pub fn new(fs_root: impl Into<PathBuf>) -> Self {
        Self {
            fs_root: fs_root.into(),
            permission: PermissionMode::default(),
            terminal: false,
        }
    }

    /// Replace the permission mode.
    #[must_use]
    pub fn with_permission(mut self, permission: PermissionMode) -> Self {
        self.permission = permission;
        self
    }
}

/// How permission requests from the agent are answered.
#[derive(Clone, Default)]
pub enum PermissionMode {
    /// Approve (once) when every location of the tool call is under
    /// `fs_root` or the call names no location; reject otherwise.
    #[default]
    AllowWithinRoot,
    /// Reject everything.
    DenyAll,
    /// Ask this prompt. While it is pending the turn's idle timeout is paused.
    Ask(DynPermissionPrompt),
}

impl fmt::Debug for PermissionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AllowWithinRoot => f.write_str("AllowWithinRoot"),
            Self::DenyAll => f.write_str("DenyAll"),
            Self::Ask(_) => f.write_str("Ask(..)"),
        }
    }
}

/// Kind of a permission option, as offered by the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionKind {
    /// Allow this call only.
    AllowOnce,
    /// Allow this and future similar calls.
    AllowAlways,
    /// Reject this call only.
    RejectOnce,
    /// Reject this and future similar calls.
    RejectAlways,
}

/// One option the agent offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionChoice {
    /// Opaque id to answer with.
    pub id: String,
    /// Label to show a human.
    pub name: String,
    /// What choosing it means.
    pub kind: PermissionKind,
}

/// A permission request from the agent, stripped of protocol types.
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionRequest {
    /// The session asking.
    pub session_id: String,
    /// The tool call it concerns.
    pub tool_call_id: String,
    /// Human-readable description of the tool call, if given.
    pub title: Option<String>,
    /// Tool category (`edit`, `execute`, ...), if given.
    pub kind: Option<String>,
    /// Files the call touches (may be empty).
    pub locations: Vec<PathBuf>,
    /// The tool's raw input, if given.
    pub raw_input: Option<serde_json::Value>,
    /// The options to choose from.
    pub options: Vec<PermissionChoice>,
}

impl PermissionRequest {
    /// The first option of `kind`, if the agent offered one.
    pub fn option_of_kind(&self, kind: PermissionKind) -> Option<&PermissionChoice> {
        self.options.iter().find(|o| o.kind == kind)
    }
}

/// The answer to a [`PermissionRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    /// Pick the option with this id (one of `PermissionRequest::options`).
    Select(String),
    /// Decline to answer; the agent treats this as a rejection.
    Cancel,
}

/// Asked for a decision when the policy is [`PermissionMode::Ask`].
///
/// The agent's tool call waits for the answer, but nothing else does: file
/// requests and updates keep flowing.
#[async_trait]
pub trait PermissionPrompt: Send + Sync {
    /// Decide on `request`.
    async fn ask(&self, request: PermissionRequest) -> PermissionDecision;
}

/// Shared handle to a [`PermissionPrompt`].
pub type DynPermissionPrompt = Arc<dyn PermissionPrompt>;

/// Test double: answers every request with the same kind of option and
/// records what it was asked.
#[derive(Debug)]
pub struct StaticPrompt {
    answer: Option<PermissionKind>,
    seen: Mutex<Vec<PermissionRequest>>,
}

impl StaticPrompt {
    /// Always picks the first option of `kind` (cancels if there is none).
    pub fn choosing(kind: PermissionKind) -> Arc<Self> {
        Arc::new(Self {
            answer: Some(kind),
            seen: Mutex::default(),
        })
    }

    /// Always cancels.
    pub fn cancelling() -> Arc<Self> {
        Arc::new(Self {
            answer: None,
            seen: Mutex::default(),
        })
    }

    /// The requests seen so far, oldest first.
    pub fn requests(&self) -> Vec<PermissionRequest> {
        self.seen.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

#[async_trait]
impl PermissionPrompt for StaticPrompt {
    async fn ask(&self, request: PermissionRequest) -> PermissionDecision {
        let decision = self
            .answer
            .and_then(|k| request.option_of_kind(k))
            .map_or(PermissionDecision::Cancel, |o| {
                PermissionDecision::Select(o.id.clone())
            });
        if let Ok(mut seen) = self.seen.lock() {
            seen.push(request);
        }
        decision
    }
}

/// Decide `request` under `mode`.
pub(crate) async fn decide(
    mode: &PermissionMode,
    guard: &FsGuard,
    request: PermissionRequest,
) -> PermissionDecision {
    let reject = |why: &str, request: &PermissionRequest| {
        tracing::warn!(
            tool_call = %request.tool_call_id,
            title = request.title.as_deref().unwrap_or(""),
            "permission rejected: {why}"
        );
        request
            .option_of_kind(PermissionKind::RejectOnce)
            .map_or(PermissionDecision::Cancel, |o| {
                PermissionDecision::Select(o.id.clone())
            })
    };
    match mode {
        PermissionMode::DenyAll => reject("policy denies all permissions", &request),
        PermissionMode::AllowWithinRoot => {
            if !guard.contains_all(&request.locations).await {
                return reject("tool call touches paths outside fs_root", &request);
            }
            match request.option_of_kind(PermissionKind::AllowOnce) {
                Some(o) => {
                    tracing::debug!(tool_call = %request.tool_call_id, "permission allowed once");
                    PermissionDecision::Select(o.id.clone())
                }
                None => reject("agent offered no allow-once option", &request),
            }
        }
        PermissionMode::Ask(prompt) => {
            let decision = prompt.ask(request.clone()).await;
            match decision {
                PermissionDecision::Select(ref id)
                    if request.options.iter().any(|o| &o.id == id) =>
                {
                    decision
                }
                PermissionDecision::Select(id) => {
                    reject(&format!("prompt chose unknown option `{id}`"), &request)
                }
                PermissionDecision::Cancel => PermissionDecision::Cancel,
            }
        }
    }
}
