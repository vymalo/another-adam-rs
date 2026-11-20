//! The conversations and run ids of schedules and their firings (ADR 0060, decisions 1 and 2).

use adam_core::RunId;
use adam_core::time::Timestamp;
use adam_runtime::SCHEDULE_CONVERSATION_PREFIX;
use sha2::{Digest, Sha256};

/// The conversation of a schedule's scheduler run: `adam.schedule:<agent>/<name>`. The store
/// keeps one open run per conversation, so at most one scheduler per schedule.
pub fn schedule_conversation_id(agent: &str, name: &str) -> String {
    format!("{SCHEDULE_CONVERSATION_PREFIX}{agent}/{name}")
}

/// The conversation of the run a schedule fires at `due`: `adam.schedule:<agent>/<name>@<due>`,
/// the due time in UTC RFC 3339. [`trigger_of`] reads it back.
pub fn fired_conversation_id(agent: &str, name: &str, due: Timestamp) -> String {
    format!(
        "{}@{}",
        schedule_conversation_id(agent, name),
        canonical(due)
    )
}

/// The id of the run a schedule fires at `due`, the same on every replica and for every scheduler
/// run of the schedule: a UUID v8 of SHA-256 over a tag of its own and the length-prefixed agent,
/// name and due time, built like `adam_runtime::child_run_id`.
pub fn fired_run_id(agent: &str, name: &str, due: Timestamp) -> RunId {
    let due = canonical(due);
    let mut hash = Sha256::new();
    for part in ["adam-schedule/fired-run/v1", agent, name, due.as_str()] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    let digest = hash.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    RunId(uuid::Builder::from_custom_bytes(bytes).into_uuid())
}

/// What started a fired run, read from its conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trigger {
    /// The agent the schedule belongs to.
    pub agent: String,
    /// The schedule's name.
    pub name: String,
    /// The due time it fired for.
    pub due: Timestamp,
}

/// The trigger of a run in `conversation_id`, when it is a firing's ([`fired_conversation_id`]);
/// `None` for any other conversation, a scheduler's included.
pub fn trigger_of(conversation_id: &str) -> Option<Trigger> {
    let rest = conversation_id.strip_prefix(SCHEDULE_CONVERSATION_PREFIX)?;
    let (schedule, due_text) = rest.rsplit_once('@')?;
    let (agent, name) = schedule.split_once('/')?;
    if agent.is_empty() || name.is_empty() {
        return None;
    }
    let due = Timestamp(due_text.parse().ok()?);
    // Only the form this crate writes: a second spelling of one due time would be a second firing.
    (canonical(due) == due_text).then(|| Trigger {
        agent: agent.to_owned(),
        name: name.to_owned(),
        due,
    })
}

/// A due time as UTC RFC 3339, with a fraction only when it has one (jiff's form).
fn canonical(due: Timestamp) -> String {
    due.0.to_string()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn at(text: &str) -> Timestamp {
        Timestamp(text.parse().unwrap())
    }

    #[test]
    fn conversations_name_the_schedule_and_the_due_time() {
        let due = at("2026-12-24T17:00:00Z");
        assert_eq!(
            schedule_conversation_id("coder", "team/weekly"),
            "adam.schedule:coder/team/weekly"
        );
        assert_eq!(
            fired_conversation_id("coder", "team/weekly", due),
            "adam.schedule:coder/team/weekly@2026-12-24T17:00:00Z"
        );
        assert_eq!(
            fired_conversation_id("a", "soon", at("2026-12-24T17:00:00.25Z")),
            "adam.schedule:a/soon@2026-12-24T17:00:00.25Z"
        );
    }

    #[test]
    fn a_trigger_is_read_back() {
        let due = at("2026-12-24T17:00:00Z");
        let conv = fired_conversation_id("coder", "team/weekly", due);
        assert_eq!(
            trigger_of(&conv),
            Some(Trigger {
                agent: "coder".into(),
                name: "team/weekly".into(),
                due
            })
        );
        for other in [
            "adam.schedule:coder/team/weekly",
            "adam.schedule:coder@2026-12-24T17:00:00Z",
            "adam.schedule:/x@2026-12-24T17:00:00Z",
            "adam.schedule:coder/@2026-12-24T17:00:00Z",
            "adam.schedule:coder/x@tomorrow",
            "adam.schedule:coder/x@2026-12-24T18:00:00+01:00",
            "adam.schedule:coder/x@2026-12-24T17:00Z",
            "token-1:coder/x@2026-12-24T17:00:00Z",
        ] {
            assert_eq!(trigger_of(other), None, "{other}");
        }
    }

    #[test]
    fn fired_ids_are_stable_and_distinct() {
        let due = at("2026-12-24T17:00:00Z");
        let id = fired_run_id("coder", "daily", due);
        assert_eq!(id, fired_run_id("coder", "daily", due));
        assert_eq!(id.0.get_version_num(), 8);
        // Pinned: a change of the derivation would fire every due time again after an upgrade.
        assert_eq!(id.to_string(), FIRED_ID_PIN);
        assert_ne!(
            id,
            fired_run_id("coder", "daily", at("2026-12-24T17:01:00Z"))
        );
        assert_ne!(id, fired_run_id("coder", "nightly", due));
        assert_ne!(id, fired_run_id("chat", "daily", due));
        // Length prefixes: moving a character between agent and name is another id.
        assert_ne!(fired_run_id("ab", "c", due), fired_run_id("a", "bc", due));
        // Not a child id of anything: the tag is the crate's own.
        assert_ne!(
            id,
            adam_runtime::child_run_id(RunId(uuid::Uuid::nil()), "daily")
        );
    }

    const FIRED_ID_PIN: &str = "811c8bcc-f6d5-8d97-ae72-72b18a8ebcaf";
}
