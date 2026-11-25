//! `prompt-statement/v1`: the prompt statement and the person's envelope a message carries.
//!
//! A client that signs what a person asked sends, in the message's metadata under
//! [`PROMPT_STATEMENT_EXTENSION`], `{"statement": "<JWS>", "user": "<JWS>"}` (`user` may be absent;
//! on a device, which has no orchestrator, `statement` is absent and `user` is required).
//! The agent keeps both with the run and every model call of the run carries them, byte for byte,
//! as the headers `Prompt-Statement` and `Prompt-User-Signature` (adam ADR 0077). The agent checks
//! the shape of each value and nothing more: whoever verifies a signature sits in front of the model.
//!
//! * **Only an activated extension is read** ([`statement_of_request`]): a message whose request did
//!   not name the extension carries nothing, whatever its metadata says.
//! * **A malformed entry refuses the message.** A statement that is missing, not a string or not a
//!   compact JWS, or an envelope that is not one, is an error naming the member and never its
//!   value. The message is not taken without it: a message that brings no values keeps the run's
//!   current ones, so taking a broken entry as "none" would sign the new prompt with the old
//!   statement.

use std::collections::HashMap;

use a2a::Message;
use adam_model::PromptStatement;
use serde_json::Value;

use crate::Caller;
use crate::extensions::PROMPT_STATEMENT_EXTENSION;

/// The member of the entry that carries the statement: `{"statement": "<JWS>"}`. Part of the wire.
pub const PROMPT_STATEMENT_MEMBER: &str = "statement";

/// The member of the entry that carries the person's envelope: `{"user": "<JWS>"}`. Part of the
/// wire; absent (or `null`) when the client has no envelope.
pub const PROMPT_USER_MEMBER: &str = "user";

/// The values of `metadata` under [`PROMPT_STATEMENT_EXTENSION`].
///
/// `Ok(None)` for no entry under the URI. Activation is the caller's to check, as for every
/// extension: see [`statement_of_request`].
///
/// # Errors
///
/// A sentence naming what is wrong, for `InvalidParams`: an entry that is not an object, a
/// [`PROMPT_STATEMENT_MEMBER`] or a [`PROMPT_USER_MEMBER`] that is neither absent, `null` nor a
/// string, neither of the two, or a value that is not a compact JWS of at most
/// [`adam_model::MAX_PROMPT_JWS_BYTES`] bytes. It never quotes a value.
pub fn statement_of(metadata: &HashMap<String, Value>) -> Result<Option<PromptStatement>, String> {
    let Some(entry) = metadata.get(PROMPT_STATEMENT_EXTENSION) else {
        return Ok(None);
    };
    let Some(entry) = entry.as_object() else {
        return Err("prompt-statement/v1: the entry is not an object".to_owned());
    };
    let text = |member: &str| match entry.get(member) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(format!("prompt-statement/v1: `{member}` is not a string")),
    };
    let (statement, user) = (text(PROMPT_STATEMENT_MEMBER)?, text(PROMPT_USER_MEMBER)?);
    if statement.is_none() && user.is_none() {
        return Err(
            "prompt-statement/v1: the entry has neither a string `statement` nor a string `user`"
                .to_owned(),
        );
    }
    PromptStatement::from_parts(statement, user)
        .map(Some)
        .map_err(|why| format!("prompt-statement/v1: {why}"))
}

/// The values a request carries: those of `message`, when the request **activated**
/// [`PROMPT_STATEMENT_EXTENSION`] ([`Caller::has_extension`]). `Ok(None)` when it did not, or the
/// message carries no entry.
///
/// # Errors
///
/// As [`statement_of`], only for a request that activated the extension.
pub fn statement_of_request(
    caller: &Caller,
    message: &Message,
) -> Result<Option<PromptStatement>, String> {
    if !caller.has_extension(PROMPT_STATEMENT_EXTENSION) {
        return Ok(None);
    }
    match &message.metadata {
        Some(metadata) => statement_of(metadata),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2a::{Part, Role};
    use serde_json::json;

    const S: &str = "eyJhbGciOiJFZERTQSJ9.eyJwIjoiMSJ9.c3RhdGVtZW50";
    const U: &str = "eyJhbGciOiJFUzI1NiJ9.eyJ1IjoiMSJ9.ZW52ZWxvcGU";

    fn with(entry: Value) -> Message {
        let mut m = Message::new(Role::User, vec![Part::text("hi")]);
        m.metadata = Some(HashMap::from([(
            PROMPT_STATEMENT_EXTENSION.to_owned(),
            entry,
        )]));
        m
    }

    fn activated() -> Caller {
        Caller::new("t").with_extensions([PROMPT_STATEMENT_EXTENSION])
    }

    #[test]
    fn an_activated_request_reads_both_values() {
        let read = statement_of_request(&activated(), &with(json!({"statement": S, "user": U})))
            .unwrap()
            .unwrap();
        assert_eq!((read.statement(), read.user()), (Some(S), Some(U)));
        for absent in [
            json!({"statement": S}),
            json!({"statement": S, "user": null}),
        ] {
            let read = statement_of_request(&activated(), &with(absent))
                .unwrap()
                .unwrap();
            assert_eq!((read.statement(), read.user()), (Some(S), None));
        }
        // A device's envelope alone.
        let device = statement_of_request(&activated(), &with(json!({"user": U})))
            .unwrap()
            .unwrap();
        assert_eq!((device.statement(), device.user()), (None, Some(U)));
        let bare = Message::new(Role::User, vec![Part::text("hi")]);
        assert_eq!(statement_of_request(&activated(), &bare), Ok(None));
    }

    #[test]
    fn a_request_that_did_not_activate_it_reads_nothing() {
        let plain = Caller::new("t");
        assert_eq!(
            statement_of_request(&plain, &with(json!({"statement": S, "user": U}))),
            Ok(None)
        );
        // Not even a malformed entry is read.
        assert_eq!(statement_of_request(&plain, &with(json!(7))), Ok(None));
    }

    #[test]
    fn a_malformed_entry_is_refused_without_its_value() {
        for (entry, wanted) in [
            (json!("x.y.z"), "not an object"),
            (json!({}), "neither"),
            (json!({"statement": null, "user": null}), "neither"),
            (json!({"statement": 3}), "`statement` is not a string"),
            (
                json!({"statement": 3, "user": U}),
                "`statement` is not a string",
            ),
            (json!({"statement": "not-a-jws"}), "prompt statement"),
            (json!({"statement": S, "user": 4}), "`user` is not a string"),
            (json!({"statement": S, "user": "not-a-jws"}), "envelope"),
        ] {
            let err = statement_of_request(&activated(), &with(entry.clone())).unwrap_err();
            assert!(err.contains(wanted), "{entry}: {err}");
            assert!(!err.contains("not-a-jws") && !err.contains(S), "{err}");
        }
    }
}
