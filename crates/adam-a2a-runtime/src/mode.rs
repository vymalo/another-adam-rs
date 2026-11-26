//! `agent-mode/v1`: the mode a message picks reaches the run as its inbound context.
//!
//! * A message that **activated** the extension (the card declares it, and the request named it:
//!   `Caller::extensions`) must carry `metadata[<URI>] = {"mode": "<name>"}`, a mode the agent
//!   declares ([`AgentModes`](adam_a2a::AgentModes)). Anything else is refused before the message
//!   is written, as an A2A `invalid params` error whose sentence begins `mode_not_offered`. The
//!   mode becomes the context key [`CONTEXT_MODE`], `{"mode": ..}`, which the mode source of the
//!   assembly reads on every model turn and every call; it holds until a later message names
//!   another, and it is a protected key, so a child run starts in its parent's mode.
//! * A message that did not activate it is read as it always was: no mode is read from it, and a
//!   run that names none works in the agent's default mode.
//! * A mode only narrows. The access tier still caps every tool the mode allows, and a mode can
//!   neither raise the tier nor grant a tool the agent does not have.

use adam_a2a::{AGENT_MODE_EXTENSION, AgentModes, Caller};
use adam_runtime::Inbound;
use a2a::Message;
use serde_json::{Value, json};

/// The context key under which the mode the **latest message that named one** picked is kept:
/// `{"mode": "<name>"}`. It is `adam_llm_agent::MODE_CONTEXT_KEY` (a test keeps the two equal),
/// read on every model turn and every call by the assembly's mode source (ADR 0039, decision 3).
pub const CONTEXT_MODE: &str = "vymalo.mode";

/// The mode `message` names for its run, checked against `modes`.
///
/// Activation is the caller's to check: only a request that named [`AGENT_MODE_EXTENSION`] may
/// call this, and the backend wires it.
///
/// # Errors
///
/// The sentence for the A2A error: no metadata entry, an entry that is not an object, a `mode`
/// that is missing, blank or not a string, or a mode the agent does not declare.
pub(crate) fn requested(message: &Message, modes: &AgentModes) -> Result<String, String> {
    modes.requested(message)
}

/// The mode a message of `caller` names, when the request activated the extension and the backend
/// declares modes: the check and the sentence of [`requested`], in one place, so that every call
/// that reads a message reads it the same way.
pub(crate) fn named(
    caller: &Caller,
    message: &Message,
    modes: Option<&AgentModes>,
) -> Result<Option<String>, String> {
    match modes {
        Some(modes) if caller.has_extension(AGENT_MODE_EXTENSION) => {
            requested(message, modes).map(Some)
        }
        _ => Ok(None),
    }
}

/// Put the picked `mode` into the context `inbound` carries to the run.
///
/// # Errors
///
/// The sentence for the A2A error when the payload cannot carry a context (it is neither text nor an
/// object) or its `context` is not an object: the mode is never dropped silently.
pub(crate) fn put(inbound: &mut Inbound, mode: &str) -> Result<(), String> {
    if let Value::String(text) = &inbound.payload {
        inbound.payload = json!({ "text": text });
    }
    let payload = inbound.payload.as_object_mut().ok_or_else(|| {
        "the message cannot carry a mode: its payload is not text".to_owned()
    })?;
    let context = payload
        .entry("context")
        .or_insert_with(|| Value::Object(Default::default()))
        .as_object_mut()
        .ok_or_else(|| "the message cannot carry a mode: its context is not an object".to_owned())?;
    context.insert(CONTEXT_MODE.to_owned(), json!({ "mode": mode }));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2a::{Part, Role};
    use adam_a2a::ModeInfo;
    use std::collections::HashMap;

    fn modes() -> AgentModes {
        AgentModes::new(
            [
                ModeInfo::new("build", "Build", "Changes things."),
                ModeInfo::new("plan", "Plan", "Reads and proposes."),
            ],
            "build",
        )
        .expect("declared")
    }

    fn message(entry: Option<Value>) -> Message {
        let mut message = Message::new(Role::User, vec![Part::text("hi")]);
        message.metadata = entry.map(|e| HashMap::from([(AGENT_MODE_EXTENSION.to_owned(), e)]));
        message
    }

    fn picking() -> Caller {
        Caller::new("token-a").with_extensions([AGENT_MODE_EXTENSION])
    }

    #[test]
    fn the_key_is_the_agents_own() {
        assert_eq!(CONTEXT_MODE, "vymalo.mode");
        assert_eq!(CONTEXT_MODE, adam_llm_agent::MODE_CONTEXT_KEY);
        assert!(adam_llm_agent::PROTECTED_CONTEXT_KEYS.contains(&CONTEXT_MODE));
    }

    #[test]
    fn a_declared_mode_is_read_and_anything_else_is_a_sentence_for_the_error() {
        assert_eq!(
            requested(&message(Some(json!({"mode": "plan"}))), &modes()),
            Ok("plan".to_owned())
        );
        for (entry, wanted) in [
            (None, "names no mode"),
            (Some(json!("plan")), "not an object"),
            (Some(json!({})), "no string"),
            (Some(json!({"mode": 3})), "no string"),
            (Some(json!({"mode": "review"})), "does not declare"),
        ] {
            let err = requested(&message(entry.clone()), &modes()).unwrap_err();
            assert!(err.contains(wanted), "{entry:?}: {err}");
        }
    }

    #[test]
    fn only_an_activated_request_with_a_declared_mode_is_read() {
        let modes = modes();
        let naming = message(Some(json!({"mode": "plan"})));
        assert_eq!(
            named(&picking(), &naming, Some(&modes)),
            Ok(Some("plan".to_owned()))
        );
        // Not activated: nothing is read, whatever the metadata says.
        assert_eq!(
            named(&Caller::new("token-a"), &naming, Some(&modes)),
            Ok(None)
        );
        // A backend that declares none reads none.
        assert_eq!(named(&picking(), &naming, None), Ok(None));
        // An activated message with a mode the agent does not declare is refused.
        let unknown = message(Some(json!({"mode": "review"})));
        assert!(
            named(&picking(), &unknown, Some(&modes))
                .unwrap_err()
                .contains("does not declare")
        );
    }

    #[test]
    fn the_mode_goes_into_the_context_of_the_inbound_whatever_its_payload() {
        let mut inbound = Inbound::new("message", json!({"text": "hi"}));
        put(&mut inbound, "plan").unwrap();
        assert_eq!(
            inbound.payload,
            json!({"text": "hi", "context": {"vymalo.mode": {"mode": "plan"}}})
        );
        // Beside what the context already holds.
        let mut inbound = Inbound::new("message", json!({"text": "hi", "context": {"a": 1}}));
        put(&mut inbound, "build").unwrap();
        assert_eq!(
            inbound.payload["context"],
            json!({"a": 1, "vymalo.mode": {"mode": "build"}})
        );
        // A bare string payload becomes an object.
        let mut inbound = Inbound::new("message", json!("hi"));
        put(&mut inbound, "plan").unwrap();
        assert_eq!(inbound.payload["text"], "hi");
        assert_eq!(inbound.payload["context"]["vymalo.mode"]["mode"], "plan");
    }

    #[test]
    fn a_payload_that_cannot_carry_a_mode_is_an_error_and_never_a_silent_drop() {
        for payload in [
            json!(3),
            json!([1]),
            json!(null),
            json!({"text": "hi", "context": "no"}),
            json!({"text": "hi", "context": [1]}),
        ] {
            let mut inbound = Inbound::new("message", payload.clone());
            let err = put(&mut inbound, "plan").unwrap_err();
            assert!(err.contains("cannot carry a mode"), "{payload}: {err}");
        }
    }
}
