//! `model-pick/v1`: the model a message names reaches the run as its inbound context.
//!
//! * A message that **activated** the extension (the card declares it, and the request named it:
//!   `Caller::extensions`) must carry `metadata[<URI>] = {"model": "<alias>"}`, an alias the card
//!   offers ([`ModelChoices`](adam_a2a::ModelChoices)). Anything else is refused before the message
//!   is written, as an A2A `invalid params` error whose sentence begins `model_not_offered`. The
//!   alias becomes the context key [`CONTEXT_MODEL`], `{"model": ..}`, which the agent loop reads
//!   on every model turn ([`adam_llm_agent::MODEL_CONTEXT_KEY`]) and honours only for a model it
//!   was given a binding for.
//! * A message that did not activate it is read as it always was: no pick is read from it.
//! * A pick is a choice of model, never an authorisation: the call still goes through the
//!   deployment's gateway, which refuses a locked model (`model_locked`, ADR 0067).

use adam_runtime::Inbound;
use serde_json::{Value, json};

/// The context key under which the model the **latest message that named one** picked is kept:
/// `{"model": "<alias>"}`. It is `adam_llm_agent::MODEL_CONTEXT_KEY` (a test keeps the two equal),
/// read on every model turn and ignored by an agent given no binding for the alias (ADR 0064,
/// amended 2026-11-23).
pub const CONTEXT_MODEL: &str = "vymalo.model";

/// Put the picked `alias` into the context `inbound` carries to the run.
///
/// # Errors
///
/// The sentence for the A2A error when the payload cannot carry a context (it is neither text nor an
/// object) or its `context` is not an object: the pick is never dropped silently.
pub(crate) fn put(inbound: &mut Inbound, alias: &str) -> Result<(), String> {
    if let Value::String(text) = &inbound.payload {
        inbound.payload = json!({ "text": text });
    }
    let payload = inbound.payload.as_object_mut().ok_or_else(|| {
        "the message cannot carry a model pick: its payload is not text".to_owned()
    })?;
    let context = payload
        .entry("context")
        .or_insert_with(|| Value::Object(Default::default()))
        .as_object_mut()
        .ok_or_else(|| {
            "the message cannot carry a model pick: its context is not an object".to_owned()
        })?;
    context.insert(CONTEXT_MODEL.to_owned(), json!({ "model": alias }));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_is_the_agents_own() {
        assert_eq!(CONTEXT_MODEL, "vymalo.model");
        assert_eq!(CONTEXT_MODEL, adam_llm_agent::MODEL_CONTEXT_KEY);
    }

    #[test]
    fn the_pick_reaches_a_payload_of_any_shape() {
        // A bare string payload becomes an object, as the tier's does.
        let mut inbound = Inbound::new("message", json!("go"));
        put(&mut inbound, "alias-a").expect("put");
        assert_eq!(inbound.payload["text"], "go");
        assert_eq!(
            inbound.payload["context"][CONTEXT_MODEL],
            json!({"model": "alias-a"})
        );
        // A payload that cannot carry a context is refused, never dropped.
        let mut inbound = Inbound::new("message", json!(3));
        assert!(put(&mut inbound, "alias-a").is_err());
        assert_eq!(inbound.payload, json!(3));
    }
}
