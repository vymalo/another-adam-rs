//! `reasoning/v1`: the reasoning effort a message names reaches the run as its inbound context.
//!
//! * A message that **activated** the extension (the card declares it, and the request named it:
//!   `Caller::extensions`) must carry `metadata[<URI>] = {"effort": "<level>"}`, where the level is
//!   one the model of the run it reaches takes ([`adam_a2a::ReasoningEfforts`]). Anything else is
//!   refused before the message is written, as an A2A `invalid params` error whose sentence names
//!   the agent and the levels it takes. The level becomes the context key
//!   [`CONTEXT_REASONING_EFFORT`], `{"effort": ..}`, which the run merges like any other key, so the
//!   newest message wins from the agent's next model call on.
//! * A message that did not activate it is read as it always was: no level is read from it, and a
//!   message that names none leaves the run's effort as it is.
//! * The effort is a **protected** key of the loop (`adam_llm_agent::PROTECTED_CONTEXT_KEYS`): a
//!   message whose rest of context is too large keeps it, and a child run starts with its parent's.
//!   The extension can only set: no member clears an effort.

use adam_a2a::ReasoningEffort;
use adam_runtime::Inbound;
use serde_json::json;

/// The context key under which the effort of the **latest message that named one** is kept:
/// `{"effort": "<level>"}`. It is `adam_llm_agent::REASONING_EFFORT_CONTEXT_KEY` (a test keeps the
/// two equal), read on every model turn; absent sends nothing and leaves the model's own default
/// (ADR 0063, decisions 4 and 5).
pub const CONTEXT_REASONING_EFFORT: &str = "vymalo.reasoningEffort";

/// Put the effort `message` named into the context `inbound` carries to the run.
///
/// # Errors
///
/// The sentence for the A2A error when the payload cannot carry a context (it is neither text nor an
/// object) or its `context` is not an object: the level is never dropped silently.
pub(crate) fn put(inbound: &mut Inbound, effort: ReasoningEffort) -> Result<(), String> {
    if let serde_json::Value::String(text) = &inbound.payload {
        inbound.payload = json!({ "text": text });
    }
    let payload = inbound.payload.as_object_mut().ok_or_else(|| {
        "the message cannot carry a reasoning effort: its payload is not text".to_owned()
    })?;
    let context = payload
        .entry("context")
        .or_insert_with(|| serde_json::Value::Object(Default::default()))
        .as_object_mut()
        .ok_or_else(|| {
            "the message cannot carry a reasoning effort: its context is not an object".to_owned()
        })?;
    context.insert(
        CONTEXT_REASONING_EFFORT.to_owned(),
        json!({ "effort": effort.as_str() }),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_is_the_loops_own() {
        assert_eq!(CONTEXT_REASONING_EFFORT, "vymalo.reasoningEffort");
        assert_eq!(
            CONTEXT_REASONING_EFFORT,
            adam_llm_agent::REASONING_EFFORT_CONTEXT_KEY
        );
        assert!(
            adam_llm_agent::PROTECTED_CONTEXT_KEYS.contains(&CONTEXT_REASONING_EFFORT),
            "the effort survives a dropped context, as the tier does"
        );
    }

    #[test]
    fn the_level_reaches_a_payload_of_any_shape() {
        for effort in ReasoningEffort::ALL {
            // A bare string payload becomes an object, as the tier's does.
            let mut inbound = Inbound::new("message", json!("go"));
            put(&mut inbound, effort).expect("put");
            assert_eq!(inbound.payload["text"], "go");
            assert_eq!(
                inbound.payload["context"][CONTEXT_REASONING_EFFORT],
                json!({ "effort": effort.as_str() })
            );
            // An object that already carries a context keeps the rest of it.
            let mut inbound = Inbound::new("message", json!({"text": "go", "context": {"k": 1}}));
            put(&mut inbound, effort).expect("put");
            assert_eq!(inbound.payload["context"]["k"], 1);
        }
        // A payload that cannot carry a context is refused, never dropped.
        let mut inbound = Inbound::new("message", json!(3));
        assert!(put(&mut inbound, ReasoningEffort::High).is_err());
        assert_eq!(inbound.payload, json!(3));
        let mut inbound = Inbound::new("message", json!({"context": 3}));
        assert!(put(&mut inbound, ReasoningEffort::High).is_err());
    }
}
