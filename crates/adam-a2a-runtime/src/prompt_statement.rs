//! `prompt-statement/v1` on the run (adam ADR 0077): the prompt statement and the person's envelope
//! a message carries become the inbound payload's [`PAYLOAD_PROMPT_STATEMENT`], which the loop keeps
//! with the run and sends on every model call. The payload never holds one a request did not
//! activate, whatever the inbound function wrote.

use a2a::Message;
use adam_a2a::Caller;
use adam_runtime::Inbound;
use serde_json::{Value, json};

/// The key of the inbound payload that carries the values: `{"statement", "user"?}`. It is
/// `adam_llm_agent::PROMPT_STATEMENT_KEY` (a test keeps the two equal); this crate does not depend
/// on the loop.
pub const PAYLOAD_PROMPT_STATEMENT: &str = "prompt_statement";

/// Put the values `message` carries into `inbound`'s payload when the request **activated**
/// `prompt-statement/v1`, and take out any the payload held otherwise. A message that carries none
/// leaves none, and the run keeps the ones it has.
///
/// # Errors
///
/// A sentence for `InvalidParams` when the request activated the extension and the entry is
/// malformed ([`adam_a2a::statement_of`]); it quotes no value, and nothing is written.
pub(crate) fn put(caller: &Caller, message: &Message, inbound: &mut Inbound) -> Result<(), String> {
    if let Some(payload) = inbound.payload.as_object_mut() {
        payload.remove(PAYLOAD_PROMPT_STATEMENT);
    }
    let Some(prompt) = adam_a2a::statement_of_request(caller, message)? else {
        return Ok(());
    };
    if let Value::String(text) = &inbound.payload {
        inbound.payload = json!({ "text": text });
    }
    let value = serde_json::to_value(&prompt)
        .map_err(|_| "prompt-statement/v1: the values could not be kept".to_owned())?;
    match inbound.payload.as_object_mut() {
        Some(payload) => {
            payload.insert(PAYLOAD_PROMPT_STATEMENT.to_owned(), value);
            Ok(())
        }
        None => Err("prompt-statement/v1: the message cannot carry the values".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use a2a::{Part, Role};
    use adam_a2a::PROMPT_STATEMENT_EXTENSION;
    use std::collections::HashMap;

    const S: &str = "eyJhbGciOiJFZERTQSJ9.eyJwIjoiMSJ9.c3RhdGVtZW50";

    #[test]
    fn the_key_is_the_loops() {
        assert_eq!(
            PAYLOAD_PROMPT_STATEMENT,
            adam_llm_agent::PROMPT_STATEMENT_KEY
        );
    }

    fn message(entry: Value) -> Message {
        let mut m = Message::new(Role::User, vec![Part::text("hi")]);
        m.metadata = Some(HashMap::from([(
            PROMPT_STATEMENT_EXTENSION.to_owned(),
            entry,
        )]));
        m
    }

    #[test]
    fn only_an_activated_request_writes_it_and_nothing_else_can() {
        let on = Caller::new("t").with_extensions([PROMPT_STATEMENT_EXTENSION]);
        let mut inbound = Inbound::new("message", json!("hi"));
        put(&on, &message(json!({"statement": S})), &mut inbound).unwrap();
        assert_eq!(
            inbound.payload,
            json!({"text": "hi", "prompt_statement": {"statement": S}})
        );

        // An inbound function that wrote the key itself: not activated, so it is taken out.
        let mut forged = Inbound::new(
            "message",
            json!({"text": "hi", "prompt_statement": {"statement": S}}),
        );
        put(
            &Caller::new("t"),
            &message(json!({"statement": S})),
            &mut forged,
        )
        .unwrap();
        assert_eq!(forged.payload, json!({"text": "hi"}));

        let mut bad = Inbound::new("message", json!({"text": "hi"}));
        let err = put(&on, &message(json!({"statement": "nope"})), &mut bad).unwrap_err();
        assert!(!err.contains("nope"), "{err}");
    }
}
