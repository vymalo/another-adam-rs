//! Child runs: how a finished run tells its parent, and how a parent reads a child.
//!
//! A run started with [`Runtime::start_child`](crate::Runtime::start_child) records its parent. When
//! it reaches a terminal state, the runtime delivers an [`Inbound`] of kind [`RUN_FINISHED_KIND`] to
//! the parent whose payload is a [`ChildStatus`]. That message is a hint that saves the parent a
//! wait: it is sent after the child's commit, so a crash in between loses it, and the parent
//! therefore keeps a timer and reads the child with [`Ctx::child_status`](crate::Ctx::child_status)
//! when the timer fires. See "Child runs" in `docs/architecture.md`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use adam_core::{RunId, RunRecord, RunStatus};

use crate::agent::Inbound;
use crate::envelope::Envelope;
use crate::runtime::{Runtime, RuntimeError};

/// [`Inbound::kind`] of the message a finished child sends its parent.
///
/// Its [`Inbound::id`] is the child's run id, so a parent that sees the message twice (the notice is
/// at-least-once) recognises the second by the id, and its payload is a [`ChildStatus`].
pub const RUN_FINISHED_KIND: &str = "adam.run.finished";

/// The id of the child a parent starts for `key` (typically a tool call id), the same every time.
///
/// The parent starts its child with [`Runtime::start_child`](crate::Runtime::start_child) under this
/// id, so a step that runs again (a replay after a crash, a lost lease or a transient retry) finds
/// the child it already started instead of starting a second one. The id is a UUID (version 8,
/// "custom") from a SHA-256 over the length-prefixed parent and key, so two parents, or two keys,
/// never share a child.
///
/// ```
/// use adam_core::RunId;
/// use adam_runtime::child_run_id;
///
/// let parent = RunId::new();
/// assert_eq!(child_run_id(parent, "call_1"), child_run_id(parent, "call_1"));
/// assert_ne!(child_run_id(parent, "call_1"), child_run_id(parent, "call_2"));
/// assert_ne!(child_run_id(parent, "call_1"), child_run_id(RunId::new(), "call_1"));
/// ```
pub fn child_run_id(parent: RunId, key: &str) -> RunId {
    let mut hash = Sha256::new();
    for part in [
        b"adam-runtime/child-run/v1".as_slice(),
        parent.0.as_bytes().as_slice(),
        key.as_bytes(),
    ] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part);
    }
    let digest = hash.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    RunId(uuid::Builder::from_custom_bytes(bytes).into_uuid())
}

/// The right of one run to start its own children, as an owned, cloneable handle.
///
/// [`Ctx::child_starter`](crate::Ctx::child_starter) makes one from the runtime that is stepping the
/// run, so code that runs inside a step (a tool of an `LlmAgent`, which cannot hold the `Ctx`) starts
/// children on that very runtime without being given one. It can only start children *of that run*:
/// [`start`](Self::start) is [`Runtime::start_child`] with the parent fixed.
///
/// It is transient: a step gets a new one, and holding on to it past the step keeps the runtime alive.
#[derive(Clone)]
pub struct ChildStarter {
    runtime: Runtime,
    parent: RunId,
}

impl std::fmt::Debug for ChildStarter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildStarter")
            .field("parent", &self.parent)
            .finish_non_exhaustive()
    }
}

impl ChildStarter {
    pub(crate) fn new(runtime: Runtime, parent: RunId) -> Self {
        Self { runtime, parent }
    }

    /// The run whose children this starts.
    pub fn parent(&self) -> RunId {
        self.parent
    }

    /// Start a run of `agent` as a child of [`parent`](Self::parent) under `id`: exactly
    /// [`Runtime::start_child`], so it is idempotent (`false` when the id exists) and the agent must
    /// be registered on this runtime, as an agent or as a starter.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::UnknownAgent`] for an agent this runtime does not know, and the store's
    /// errors.
    pub async fn start(
        &self,
        id: RunId,
        agent: &str,
        input: Inbound,
    ) -> Result<bool, RuntimeError> {
        self.runtime
            .start_child(self.parent, id, agent, input)
            .await
    }
}

/// Where a child run stands, as far as its parent needs to know.
///
/// It is the payload of the [`RUN_FINISHED_KIND`] message (`{"status": "done", "output": ...}` or
/// `{"status": "failed", "error": "..."}`) and what [`Ctx::child_status`](crate::Ctx::child_status)
/// reads back, so both routes to the answer give the same thing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChildStatus {
    /// The child's status. `Runnable` and `Parked` mean it is still working.
    pub status: RunStatus,
    /// The child's result once it is `Done`. A `null` output reads as absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    /// The child's reason once it is `Failed` (a cancel reads `cancelled: <reason>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ChildStatus {
    /// Whether the child is `Done` or `Failed`, so nothing more will change.
    pub fn is_finished(&self) -> bool {
        self.status.is_terminal()
    }

    /// The status of the child that is not there any more (purged after it finished).
    pub fn vanished() -> Self {
        Self {
            status: RunStatus::Failed,
            output: None,
            error: Some("the child run no longer exists".to_owned()),
        }
    }

    /// Read a child's record. Never fails: a finished run whose state cannot be read is reported as
    /// failed, so a parent is not left waiting on something that will never change.
    pub(crate) fn from_record(rec: &RunRecord) -> Self {
        if rec.status.is_open() {
            return Self {
                status: rec.status,
                output: None,
                error: None,
            };
        }
        match Envelope::decode(rec.id, &rec.state) {
            Ok(env) if rec.status == RunStatus::Done => Self {
                status: RunStatus::Done,
                output: (!env.output.is_null()).then_some(env.output),
                error: None,
            },
            Ok(env) => Self {
                status: RunStatus::Failed,
                output: None,
                error: Some(env.error.unwrap_or_else(|| "the run failed".to_owned())),
            },
            Err(e) => Self {
                status: RunStatus::Failed,
                output: None,
                error: Some(format!("the run's state is unreadable: {e}")),
            },
        }
    }

    /// The message that tells the parent of `child` about this status.
    pub(crate) fn notice(&self, child: RunId) -> Inbound {
        let payload = serde_json::to_value(self).unwrap_or(Value::Null);
        Inbound::new(RUN_FINISHED_KIND, payload).with_id(child.to_string())
    }

    /// Read a [`RUN_FINISHED_KIND`] message: the child it is about and its final status. `None`
    /// when `inbound` is of another kind, its id is not a run id, its payload is not a
    /// [`ChildStatus`] or the status it reports is not final.
    ///
    /// ```
    /// use adam_core::RunId;
    /// use adam_runtime::{ChildStatus, Inbound, RUN_FINISHED_KIND};
    /// use serde_json::json;
    ///
    /// let child = RunId::new();
    /// let notice = Inbound::new(RUN_FINISHED_KIND, json!({"status": "done", "output": "42"}))
    ///     .with_id(child.to_string());
    /// let (run, status) = ChildStatus::from_notice(&notice).expect("a finished notice");
    /// assert_eq!(run, child);
    /// assert_eq!(status.output, Some(json!("42")));
    ///
    /// assert!(ChildStatus::from_notice(&Inbound::new("message", json!({}))).is_none());
    /// ```
    pub fn from_notice(inbound: &Inbound) -> Option<(RunId, Self)> {
        if inbound.kind != RUN_FINISHED_KIND {
            return None;
        }
        let run = Uuid::parse_str(&inbound.id).ok().map(RunId)?;
        let status: Self = serde_json::from_value(inbound.payload.clone()).ok()?;
        status.is_finished().then_some((run, status))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_child_id_depends_on_both_inputs_and_nothing_else() {
        let (a, b) = (RunId::new(), RunId::new());
        assert_eq!(child_run_id(a, "x"), child_run_id(a, "x"));
        assert_ne!(child_run_id(a, "x"), child_run_id(a, "y"));
        assert_ne!(child_run_id(a, "x"), child_run_id(b, "x"));
        // A journal relies on the derivation, so it is pinned: this id must never change.
        let fixed = RunId(Uuid::nil());
        assert_eq!(
            child_run_id(fixed, "call_1").to_string(),
            "9a76d7bc-1e62-8362-9521-a05f6a837e5b"
        );
        assert_eq!(child_run_id(fixed, "call_1").0.get_version_num(), 8);
    }

    #[test]
    fn the_payload_has_the_documented_shape() {
        let done = ChildStatus {
            status: RunStatus::Done,
            output: Some(json!({"text": "hi"})),
            error: None,
        };
        assert_eq!(
            serde_json::to_value(&done).unwrap(),
            json!({"status": "done", "output": {"text": "hi"}})
        );
        let failed = ChildStatus {
            status: RunStatus::Failed,
            output: None,
            error: Some("boom".into()),
        };
        assert_eq!(
            serde_json::to_value(&failed).unwrap(),
            json!({"status": "failed", "error": "boom"})
        );
    }

    #[test]
    fn a_notice_round_trips_and_carries_the_child_id() {
        let child = RunId::new();
        let status = ChildStatus {
            status: RunStatus::Failed,
            output: None,
            error: Some("boom".into()),
        };
        let notice = status.notice(child);
        assert_eq!(notice.kind, RUN_FINISHED_KIND);
        assert_eq!(notice.id, child.to_string());
        assert_eq!(ChildStatus::from_notice(&notice), Some((child, status)));
    }

    #[test]
    fn what_is_not_a_finished_notice_is_refused() {
        let child = RunId::new().to_string();
        let make = |kind: &str, id: &str, payload| Inbound::new(kind, payload).with_id(id);
        // Another kind, an id that is not a run, garbage, and a status that is not final.
        assert!(
            ChildStatus::from_notice(&make("message", &child, json!({"status": "done"}))).is_none()
        );
        assert!(
            ChildStatus::from_notice(&make(RUN_FINISHED_KIND, "nope", json!({"status": "done"})))
                .is_none()
        );
        assert!(
            ChildStatus::from_notice(&make(RUN_FINISHED_KIND, &child, json!("done"))).is_none()
        );
        assert!(
            ChildStatus::from_notice(&make(
                RUN_FINISHED_KIND,
                &child,
                json!({"status": "parked"})
            ))
            .is_none()
        );
    }
}
