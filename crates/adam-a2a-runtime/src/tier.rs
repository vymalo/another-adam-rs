//! `access-tier/v1`: the tier a message names reaches the run as its inbound context, and the
//! tier a task works under is said on the task.
//!
//! * A message that **activated** the extension (the card declares it, and the request named it:
//!   `Caller::extensions`) must carry `metadata[<URI>] = {"tier": "plan" | "edits" | "auto" |
//!   "bypass"}`, a tier the agent honours. Anything else is refused before the message is written,
//!   as an A2A `invalid params` error. The tier becomes the context key [`CONTEXT_ACCESS_TIER`],
//!   `{"tier": ..}`, which the run merges like any other key, so the newest message wins from the
//!   agent's next step on.
//! * A message that did not activate it is read as it always was: no tier is read from it, and a
//!   message that names none leaves the run's context as it is.
//! * A task never inherits a tier from another task: that is the agent's rule at `init_continuing`
//!   (it drops the key the earlier task left), not the backend's.
//! * To a caller that activated the extension, a task says `{"tier": ..}` in its `metadata` under
//!   the URI: on the task of `SendMessage`, `GetTask` and the first frame of a stream, and in a
//!   status update when the tier of a streamed task changes. The tier said is the one in the run's
//!   state, or the agent's default when the state has none.

use std::collections::HashMap;

use a2a::{Message, Task};
use adam_a2a::{ACCESS_TIER_EXTENSION, AccessTier, AccessTiers, Caller};
use adam_runtime::{Inbound, RunView};
use serde_json::{Value, json};

use crate::backend::RuntimeTaskBackend;

/// The context key under which the tier of the **latest message that named one** is kept:
/// `{"tier": "plan" | "edits" | "auto" | "bypass"}`. Set from the message's metadata under
/// [`ACCESS_TIER_EXTENSION`] when the request activated the extension. The run keeps it even when
/// the rest of a message's context is too large and dropped (`adam_llm_agent::PROTECTED_CONTEXT_KEYS`
/// names this key; a test keeps the two equal).
pub const CONTEXT_ACCESS_TIER: &str = "vymalo.accessTier";

/// The tier `message` names in its metadata under the extension's URI, checked against `tiers`.
///
/// # Errors
///
/// The sentence for the A2A error: no metadata entry, an entry that is not an object, a `tier`
/// that is missing or not one of the four words (lower case, exactly), or a tier the agent does
/// not honour. It never rounds a tier up or down.
pub(crate) fn requested(message: &Message, tiers: &AccessTiers) -> Result<AccessTier, String> {
    let entry = message
        .metadata
        .as_ref()
        .and_then(|m| m.get(ACCESS_TIER_EXTENSION))
        .ok_or_else(|| {
            format!(
                "the message activates {ACCESS_TIER_EXTENSION} and names no tier: put \
                 {{\"tier\": ..}} in its metadata under that URI"
            )
        })?;
    let word = entry.get("tier").and_then(Value::as_str).ok_or_else(|| {
        "the access tier of the message is missing: `tier` is a string".to_owned()
    })?;
    let tier: AccessTier = word
        .parse()
        .map_err(|e: adam_a2a::NotATier| e.to_string())?;
    if !tiers.honours(tier) {
        return Err(format!(
            "this agent does not take the access tier {tier}; it takes: {}",
            tiers
                .honoured()
                .iter()
                .map(|t| t.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(tier)
}

/// Put `tier` into the context `inbound` carries to the run.
///
/// # Errors
///
/// The sentence for the A2A error when the payload cannot carry a context (it is neither text nor an
/// object) or its `context` is not an object: the tier is never dropped silently.
pub(crate) fn put(inbound: &mut Inbound, tier: AccessTier) -> Result<(), String> {
    if let Value::String(text) = &inbound.payload {
        inbound.payload = json!({ "text": text });
    }
    let payload = inbound.payload.as_object_mut().ok_or_else(|| {
        "the message cannot carry an access tier: its payload is not text".to_owned()
    })?;
    let context = payload
        .entry("context")
        .or_insert_with(|| Value::Object(Default::default()))
        .as_object_mut()
        .ok_or_else(|| {
            "the message cannot carry an access tier: its context is not an object".to_owned()
        })?;
    context.insert(CONTEXT_ACCESS_TIER.to_owned(), json!({ "tier": tier }));
    Ok(())
}

/// The tier a run works under: the one in its state's context, else `tiers`' default.
pub(crate) fn in_force(view: &RunView, tiers: &AccessTiers) -> AccessTier {
    view.state
        .pointer("/context")
        .and_then(|context| context.get(CONTEXT_ACCESS_TIER))
        .and_then(|entry| entry.get("tier"))
        .and_then(Value::as_str)
        .and_then(|word| word.parse::<AccessTier>().ok())
        .filter(|tier| tiers.honours(*tier))
        .unwrap_or_else(|| tiers.default_tier())
}

/// `metadata` that says `tier`.
pub(crate) fn echo(tier: AccessTier) -> HashMap<String, Value> {
    HashMap::from([(
        ACCESS_TIER_EXTENSION.to_owned(),
        json!({ "tier": tier.as_str() }),
    )])
}

impl RuntimeTaskBackend {
    /// The tier to say to `caller` about the run `view` shows: `None` unless the backend declares
    /// tiers and the request activated the extension.
    pub(crate) fn tier_said_to(&self, caller: &Caller, view: &RunView) -> Option<AccessTier> {
        caller
            .has_extension(ACCESS_TIER_EXTENSION)
            .then(|| self.tier_of(view))
            .flatten()
    }

    /// The tier the run `view` shows works under, when the backend declares tiers.
    pub(crate) fn tier_of(&self, view: &RunView) -> Option<AccessTier> {
        self.access_tiers
            .as_ref()
            .map(|tiers| in_force(view, tiers))
    }

    /// `task` with the tier it works under in its metadata, when `caller` activated the extension.
    pub(crate) fn said(&self, caller: &Caller, view: &RunView, mut task: Task) -> Task {
        if let Some(tier) = self.tier_said_to(caller, view) {
            task.metadata
                .get_or_insert_with(HashMap::new)
                .extend(echo(tier));
        }
        task
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2a::{Part, Role};

    fn tiers() -> AccessTiers {
        AccessTiers::new(
            [AccessTier::Plan, AccessTier::Edits, AccessTier::Auto],
            AccessTier::Auto,
        )
        .unwrap()
    }

    fn message(entry: Option<Value>) -> Message {
        let mut message = Message::new(Role::User, vec![Part::text("hi")]);
        message.metadata = entry.map(|e| HashMap::from([(ACCESS_TIER_EXTENSION.to_owned(), e)]));
        message
    }

    #[test]
    fn a_valid_tier_is_read_and_anything_else_is_a_sentence_for_the_error() {
        assert_eq!(
            requested(&message(Some(json!({"tier": "plan"}))), &tiers()),
            Ok(AccessTier::Plan)
        );
        // Unknown members are ignored.
        assert_eq!(
            requested(
                &message(Some(json!({"tier": "edits", "why": "x"}))),
                &tiers()
            ),
            Ok(AccessTier::Edits)
        );
        for (entry, wanted) in [
            (None, "names no tier"),
            (Some(json!("plan")), "`tier` is a string"),
            (Some(json!({})), "`tier` is a string"),
            (Some(json!({"tier": 3})), "`tier` is a string"),
            (Some(json!({"tier": "Plan"})), "is not an access tier"),
            (Some(json!({"tier": "root"})), "is not an access tier"),
            (Some(json!({"tier": ""})), "is not an access tier"),
            (
                Some(json!({"tier": "bypass"})),
                "does not take the access tier bypass",
            ),
        ] {
            let err = requested(&message(entry.clone()), &tiers()).unwrap_err();
            assert!(err.contains(wanted), "{entry:?}: {err}");
        }
    }

    #[test]
    fn the_tier_goes_into_the_context_of_the_inbound_whatever_its_payload() {
        let mut inbound = Inbound::new("message", json!({"text": "hi"}));
        put(&mut inbound, AccessTier::Edits).unwrap();
        assert_eq!(
            inbound.payload,
            json!({"text": "hi", "context": {"vymalo.accessTier": {"tier": "edits"}}})
        );
        // Beside what the context already holds.
        let mut inbound = Inbound::new("message", json!({"text": "hi", "context": {"a": 1}}));
        put(&mut inbound, AccessTier::Plan).unwrap();
        assert_eq!(
            inbound.payload["context"],
            json!({"a": 1, "vymalo.accessTier": {"tier": "plan"}})
        );
        // A bare string payload becomes an object.
        let mut inbound = Inbound::new("message", json!("hi"));
        put(&mut inbound, AccessTier::Auto).unwrap();
        assert_eq!(inbound.payload["text"], "hi");
        assert_eq!(
            inbound.payload["context"]["vymalo.accessTier"]["tier"],
            "auto"
        );
    }

    #[test]
    fn the_key_is_one_the_run_never_loses_to_a_large_context() {
        assert_eq!(CONTEXT_ACCESS_TIER, adam_llm_agent::ACCESS_TIER_CONTEXT_KEY);
        assert!(adam_llm_agent::PROTECTED_CONTEXT_KEYS.contains(&CONTEXT_ACCESS_TIER));
    }

    #[test]
    fn a_payload_that_cannot_carry_a_tier_is_an_error_and_never_a_silent_drop() {
        for payload in [
            json!(3),
            json!([1]),
            json!(null),
            json!({"text": "hi", "context": "no"}),
            json!({"text": "hi", "context": [1]}),
        ] {
            let mut inbound = Inbound::new("message", payload.clone());
            let err = put(&mut inbound, AccessTier::Plan).unwrap_err();
            assert!(
                err.contains("cannot carry an access tier"),
                "{payload}: {err}"
            );
        }
    }
}
