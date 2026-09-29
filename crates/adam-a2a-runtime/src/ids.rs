//! Deterministic ids: the same input always gives the same id, so a status or
//! a retried submission is recognised by whoever keys on the id.

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_stable_and_distinguish_every_input() {
        let a = status_message_id("t", "s", "x");
        assert_eq!(a, status_message_id("t", "s", "x"));
        assert_ne!(a, status_message_id("u", "s", "x"));
        assert_ne!(a, status_message_id("t", "r", "x"));
        // Field boundaries matter.
        assert_ne!(
            status_message_id("ab", "c", "x"),
            status_message_id("a", "bc", "x")
        );
        assert_ne!(
            status_message_id("t", "s", "x"),
            status_message_id("t", "s", "y")
        );
    }

    #[test]
    fn derived_ids_are_valid_uuids() {
        let id = status_message_id("t", "s", "x");
        let parsed = Uuid::parse_str(&id).unwrap();
        assert_eq!(parsed.get_version_num(), 8);
        assert_eq!(parsed.to_string(), id);
    }
}
