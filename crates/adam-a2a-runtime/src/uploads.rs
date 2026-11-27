//! `uploads/v1` on the run: a message that **activated** the extension tells the run that a file
//! may go to the bucket by reference, and with what limits. The fact rides the run's context under
//! [`CONTEXT_UPLOADS`]; a tool that holds a file over its inline cap reads it before it asks the
//! thread tools for a slot (the contract: an agent MUST NOT call `request_upload` unless a message
//! activated the extension).
//!
//! The entry is `{maxBytes?, ttlSecs?}`, each a positive whole number the message's metadata under
//! the extension's URI carried (a double with no fraction counts: A2A holds metadata numbers as
//! doubles). A message that did not activate the extension writes `null`, which deletes an
//! earlier yes: a run never uploads on the strength of a message that did not offer it.

use a2a::Message;
use adam_a2a::{Caller, UPLOADS_EXTENSION};
use adam_runtime::Inbound;
use serde_json::{Map, Value, json};

/// The context key of the `uploads/v1` activation: `{maxBytes?, ttlSecs?}`, or absent (`null`
/// deletes it) when the latest message did not activate the extension. Carries no credential: the
/// grant is [`CONTEXT_THREAD_TOOLS`](crate::CONTEXT_THREAD_TOOLS).
pub const CONTEXT_UPLOADS: &str = "vymalo.uploads";

/// A positive whole number of `value`, which may be a double with no fraction.
fn whole(value: &Value) -> Option<u64> {
    if let Some(n) = value.as_u64() {
        return (n > 0).then_some(n);
    }
    let f = value.as_f64()?;
    // 2^53: the largest whole number a double holds exactly.
    (f.fract() == 0.0 && (1.0..=9_007_199_254_740_991.0).contains(&f)).then_some(f as u64)
}

/// Put whether this request activated `uploads/v1` (and the limits it carries) into `inbound`'s
/// context. Never refuses a message: a limit that is not a positive whole number is left out.
pub(crate) fn put(caller: &Caller, message: &Message, inbound: &mut Inbound) {
    if let Value::String(text) = &inbound.payload {
        inbound.payload = json!({ "text": text });
    }
    let Some(payload) = inbound.payload.as_object_mut() else {
        return;
    };
    let Some(context) = payload
        .entry("context")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
    else {
        return;
    };
    if !caller.has_extension(UPLOADS_EXTENSION) {
        context.insert(CONTEXT_UPLOADS.to_owned(), Value::Null);
        return;
    }
    let mut entry = Map::new();
    let limits = message
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get(UPLOADS_EXTENSION));
    for key in ["maxBytes", "ttlSecs"] {
        if let Some(n) = limits.and_then(|l| l.get(key)).and_then(whole) {
            entry.insert(key.to_owned(), json!(n));
        }
    }
    context.insert(CONTEXT_UPLOADS.to_owned(), Value::Object(entry));
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use a2a::{Part, Role};

    use super::*;

    fn message(metadata: Option<Value>) -> Message {
        let mut m = Message::new(Role::User, vec![Part::text("hi")]);
        m.metadata = metadata.map(|v| HashMap::from([(UPLOADS_EXTENSION.to_owned(), v)]));
        m
    }

    fn context(caller: &Caller, message: &Message) -> Value {
        let mut inbound = Inbound::new("message", json!({"text": "hi"}));
        put(caller, message, &mut inbound);
        inbound.payload["context"][CONTEXT_UPLOADS].clone()
    }

    #[test]
    fn an_activating_message_carries_its_limits_and_a_double_with_no_fraction_counts() {
        let caller = Caller::new("t").with_extensions([UPLOADS_EXTENSION]);
        let m = message(Some(json!({"maxBytes": 1_073_741_824.0, "ttlSecs": 300})));
        assert_eq!(
            context(&caller, &m),
            json!({"maxBytes": 1_073_741_824u64, "ttlSecs": 300})
        );
    }

    #[test]
    fn a_limit_that_is_not_a_positive_whole_number_is_left_out_and_activation_stands() {
        let caller = Caller::new("t").with_extensions([UPLOADS_EXTENSION]);
        for bad in [json!(0), json!(-4), json!(1.5), json!("9"), json!(null)] {
            let m = message(Some(json!({"maxBytes": bad, "ttlSecs": 60})));
            assert_eq!(context(&caller, &m), json!({"ttlSecs": 60}), "{bad}");
        }
        assert_eq!(context(&caller, &message(None)), json!({}));
    }

    #[test]
    fn a_request_that_did_not_activate_it_clears_an_earlier_yes_whatever_the_metadata_says() {
        let m = message(Some(json!({"maxBytes": 10})));
        assert_eq!(context(&Caller::new("t"), &m), Value::Null);
    }
}
