//! The person and the prompt id a run's `Prompt-Statement` names.
//!
//! The statement is a compact JWS (RFC 7515 §7.1), `header.payload.signature`, whose payload is the
//! JSON the client signed. The sidecar reads two members of that payload:
//!
//! * `sub`, the person the prompt is for. The run's leaf certifies `urn:another:person:<sub>:agent:<run>`,
//!   so the sidecar has to read it: it binds the run's key to the person, and a run whose person
//!   changes is refused rather than re-keyed.
//! * `jti`, the prompt's id. It is what the request signature binds (`prompt_jti`), so a signature
//!   cannot be moved to another prompt.
//!
//! `adam_model::PromptStatement` reads the second one ([`prompt_jti`]) and deliberately not the
//! first: the orchestrator's job there is to carry the value byte for byte, and only a verifier in
//! front of the model needs the person. This module is that verifier's reader. It does not modify
//! `adam-model`.
//!
//! Nothing is verified here: a signature is the gateway's to check, against the CA the sidecar
//! trusts. What is read here is only what the sidecar's own key needs.
//!
//! [`prompt_jti`]: adam_model::PromptStatement::prompt_jti

use adam_core::turn::RunSubject;
use adam_error::{Classify, ErrorClass};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Value;

/// The two values the sidecar reads from a prompt statement.
#[derive(Clone, PartialEq, Eq)]
pub struct PromptIdentity {
    /// The `sub` of the statement's payload: the person the run's leaf names.
    pub person: String,
    /// The `jti` of the statement's payload: the prompt id the request signature binds.
    pub jti: String,
}

impl std::fmt::Debug for PromptIdentity {
    /// Says that both are there, never their values: a statement is a person's signed prompt.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromptIdentity")
            .field("person", &"[set]")
            .field("jti", &"[set]")
            .finish()
    }
}

/// The person and the prompt id of `statement` (the `Prompt-Statement` header value).
///
/// # Errors
///
/// [`StatementError`]: it is not a compact JWS of three non-empty segments, its payload is not
/// base64url JSON, it names no `sub` or no `jti`, or the person is longer than
/// [`RunSubject::MAX_PERSON_LEN`]. No error repeats a value.
pub fn person_and_jti(statement: &str) -> Result<PromptIdentity, StatementError> {
    let mut segments = statement.split('.');
    let (Some(header), Some(payload), Some(signature), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        return Err(StatementError::NotAStatement);
    };
    if header.is_empty() || signature.is_empty() {
        return Err(StatementError::NotAStatement);
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| StatementError::NotAStatement)?;
    let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(&bytes) else {
        return Err(StatementError::NotAStatement);
    };
    let person = map
        .get("sub")
        .and_then(Value::as_str)
        .filter(|person| !person.is_empty())
        .ok_or(StatementError::NoPerson)?;
    if person.len() > RunSubject::MAX_PERSON_LEN {
        return Err(StatementError::PersonTooLong);
    }
    let jti = map
        .get("jti")
        .and_then(Value::as_str)
        .filter(|jti| !jti.is_empty())
        .ok_or(StatementError::NoPromptId)?;
    Ok(PromptIdentity {
        person: person.to_owned(),
        jti: jti.to_owned(),
    })
}

/// Why a `Prompt-Statement` does not name a person and a prompt. Carries no value. Classified as
/// [`ErrorClass::Invalid`]: the same statement never works.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum StatementError {
    /// Not a compact JWS: not three segments joined by dots, an empty segment, a payload that is not
    /// base64url, or one that is not a JSON object.
    #[error("the prompt statement is not a compact JWS whose payload is a JSON object")]
    NotAStatement,
    /// The payload has no `sub`, or it is empty or not a string.
    #[error("the prompt statement names no person")]
    NoPerson,
    /// The `sub` is longer than [`RunSubject::MAX_PERSON_LEN`] bytes.
    #[error(
        "the prompt statement's person is longer than {max} bytes",
        max = RunSubject::MAX_PERSON_LEN
    )]
    PersonTooLong,
    /// The payload has no `jti`, or it is empty or not a string.
    #[error("the prompt statement names no prompt id")]
    NoPromptId,
}

impl StatementError {
    /// The refusal's detail, for the problem document and the log line: no value of the statement.
    pub const fn detail(self) -> &'static str {
        match self {
            Self::NotAStatement => {
                "the Prompt-Statement is not a compact JWS whose payload is a JSON object"
            }
            Self::NoPerson => "the prompt statement names no person (sub)",
            Self::PersonTooLong => "the prompt statement's person is too long",
            Self::NoPromptId => "the prompt statement names no prompt id (jti)",
        }
    }
}

impl Classify for StatementError {
    fn class(&self) -> ErrorClass {
        ErrorClass::Invalid
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
    use serde_json::json;

    fn statement(payload: Value) -> String {
        let header = B64.encode(br#"{"alg":"EdDSA","typ":"prompt-statement/v1"}"#);
        let payload = B64.encode(serde_json::to_vec(&payload).unwrap());
        let signature = B64.encode(b"a signature");
        format!("{header}.{payload}.{signature}")
    }

    #[test]
    fn a_well_formed_statement_names_the_person_and_the_prompt() {
        let identity =
            person_and_jti(&statement(json!({"jti": "jti-1", "sub": "person-1"}))).unwrap();
        assert_eq!(identity.person, "person-1");
        assert_eq!(identity.jti, "jti-1");
        assert!(!format!("{identity:?}").contains("person-1"));
        assert!(!format!("{identity:?}").contains("jti-1"));
    }

    #[test]
    fn a_missing_sub_names_no_person() {
        for payload in [
            json!({"jti": "jti-1"}),
            json!({"jti": "jti-1", "sub": ""}),
            json!({"jti": "jti-1", "sub": 7}),
            json!({"jti": "jti-1", "sub": null}),
        ] {
            assert_eq!(
                person_and_jti(&statement(payload.clone())),
                Err(StatementError::NoPerson),
                "{payload}"
            );
        }
    }

    #[test]
    fn a_missing_jti_names_no_prompt() {
        for payload in [
            json!({"sub": "person-1"}),
            json!({"sub": "person-1", "jti": ""}),
            json!({"sub": "person-1", "jti": ["jti-1"]}),
        ] {
            assert_eq!(
                person_and_jti(&statement(payload.clone())),
                Err(StatementError::NoPromptId),
                "{payload}"
            );
        }
    }

    #[test]
    fn a_non_jws_is_refused() {
        let good = statement(json!({"jti": "jti-1", "sub": "person-1"}));
        let payload = good.split('.').nth(1).unwrap();
        for bad in [
            String::new(),
            "not-a-jws".to_owned(),
            format!("{good}.extra"),
            format!(".{payload}.sig"),
            format!("header.{payload}."),
            format!("header.{payload}"),
            "header.%%%.signature".to_owned(),
            // A payload that decodes but is not a JSON object.
            format!("header.{}.signature", B64.encode(b"not json")),
            format!("header.{}.signature", B64.encode(b"[1,2]")),
        ] {
            assert_eq!(
                person_and_jti(&bad),
                Err(StatementError::NotAStatement),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn an_oversized_person_is_refused() {
        let person = "p".repeat(RunSubject::MAX_PERSON_LEN + 1);
        assert_eq!(
            person_and_jti(&statement(json!({"jti": "jti-1", "sub": person}))),
            Err(StatementError::PersonTooLong)
        );
        let person = "p".repeat(RunSubject::MAX_PERSON_LEN);
        assert_eq!(
            person_and_jti(&statement(json!({"jti": "jti-1", "sub": person})))
                .unwrap()
                .person
                .len(),
            RunSubject::MAX_PERSON_LEN
        );
    }

    #[test]
    fn each_error_has_a_class_and_a_detail() {
        for error in [
            StatementError::NotAStatement,
            StatementError::NoPerson,
            StatementError::PersonTooLong,
            StatementError::NoPromptId,
        ] {
            assert_eq!(error.class(), ErrorClass::Invalid);
            assert!(!error.detail().is_empty());
            assert!(!format!("{error:?}").contains("person-1"));
        }
    }
}
