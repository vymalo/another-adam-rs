//! Deterministic ids: the same input always gives the same id, so a status or
//! a retried submission is recognised by whoever keys on the id.

use adam_core::RunId;
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// A UUID derived from `domain` and `parts` (RFC 9562 version 8, "custom": the
/// first 16 bytes of a SHA-256 over the length-prefixed fields).
///
/// The fields are length-prefixed so `("ab", "c")` and `("a", "bc")` differ,
/// and the domain keeps ids of different meanings apart.
fn derived(domain: &str, parts: &[&[u8]]) -> Uuid {
    let mut hash = Sha256::new();
    for part in std::iter::once(domain.as_bytes()).chain(parts.iter().copied()) {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part);
    }
    let digest = hash.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    uuid::Builder::from_custom_bytes(bytes).into_uuid()
}

/// Id of the status message of task `task_id` in `state` saying `text`
/// (`None`: nothing to say). The stream events and `tasks/get` snapshots of the
/// same status carry the same id, and different statuses different ones.
pub(crate) fn status_message_id(task_id: &str, state: &str, text: &str) -> String {
    derived(
        "adam-a2a-runtime/status-message/v1",
        &[task_id.as_bytes(), state.as_bytes(), text.as_bytes()],
    )
    .to_string()
}

/// The task (run) id that a new task started by message `message_id` gets.
///
/// `RuntimeTaskBackend` starts a task from a message with this id, so a client
/// that repeats the request (same `messageId`, same `contextId`, same caller)
/// reaches the task the first attempt made instead of starting another or
/// feeding the input in twice. Use it to look up "the task this message
/// started" without a side table.
///
/// The inputs are the `agent` the task is for (one runtime can serve several
/// agents, and a run belongs to exactly one), the caller's `subject` (two
/// callers never share a task), the `context_id` the request carried (`None`
/// when it carried none; that is not the same as an empty one), and the
/// `message_id`. Different agents or contexts give different tasks even for the
/// same message id.
pub fn task_id_for(
    agent: &str,
    subject: &str,
    context_id: Option<&str>,
    message_id: &str,
) -> RunId {
    let (present, context) = match context_id {
        Some(c) => (b"1".as_slice(), c.as_bytes()),
        None => (b"0".as_slice(), b"".as_slice()),
    };
    RunId(derived(
        "adam-a2a-runtime/task/v1",
        &[
            agent.as_bytes(),
            subject.as_bytes(),
            present,
            context,
            message_id.as_bytes(),
        ],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_stable_and_distinguish_every_input() {
        let a = task_id_for("x", "s", Some("c"), "m");
        assert_eq!(a, task_id_for("x", "s", Some("c"), "m"));
        // Two agents of one runtime never share a task for the same message.
        assert_ne!(a, task_id_for("y", "s", Some("c"), "m"));
        assert_ne!(a, task_id_for("x", "t", Some("c"), "m"));
        assert_ne!(a, task_id_for("x", "s", Some("d"), "m"));
        assert_ne!(a, task_id_for("x", "s", Some("c"), "n"));
        assert_ne!(
            task_id_for("x", "s", None, "m"),
            task_id_for("x", "s", Some(""), "m")
        );
        // Field boundaries matter.
        assert_ne!(
            task_id_for("x", "ab", Some("c"), "m"),
            task_id_for("x", "a", Some("bc"), "m")
        );
        assert_ne!(
            task_id_for("xs", "", Some("c"), "m"),
            task_id_for("x", "s", Some("c"), "m")
        );
        // Status ids and task ids never coincide for the same fields.
        assert_ne!(
            status_message_id("t", "s", "x"),
            status_message_id("t", "s", "y")
        );
    }

    #[test]
    fn derived_ids_are_valid_uuids() {
        let id = task_id_for("x", "s", None, "m").0;
        assert_eq!(id.get_version_num(), 8);
        assert_eq!(Uuid::parse_str(&id.to_string()).unwrap(), id);
    }
}
