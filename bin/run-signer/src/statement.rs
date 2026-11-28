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
//! The value itself is read by `adam_model::PromptStatement`, the same type the agent carries: the
//! form (three base64url segments, at most [`MAX_PROMPT_JWS_BYTES`]) is checked there, so the sidecar
//! declines exactly what the agent declines, and [`person`] and [`prompt_jti`] read the two members.
//!
//! Nothing is verified here: a signature is the gateway's to check, against the CA the sidecar
//! trusts. What is read here is only what the sidecar's own key needs. In particular `sub` is
//! **not** verified: the gateway is the only verifier, and see the README on what that means.
//!
//! [`MAX_PROMPT_JWS_BYTES`]: adam_model::MAX_PROMPT_JWS_BYTES
//! [`person`]: adam_model::PromptStatement::person
//! [`prompt_jti`]: adam_model::PromptStatement::prompt_jti

use adam_core::turn::RunSubject;
use adam_error::{Classify, ErrorClass};
use adam_model::PromptStatement;

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
/// [`StatementError`]: it is not a compact JWS of three non-empty segments and at most
/// [`MAX_PROMPT_JWS_BYTES`](adam_model::MAX_PROMPT_JWS_BYTES), its payload is not base64url JSON, it
/// names no `sub` or no `jti`, or the person is longer than [`RunSubject::MAX_PERSON_LEN`]. No error
/// repeats a value.
pub fn person_and_jti(statement: &str) -> Result<PromptIdentity, StatementError> {
    let parsed =
        PromptStatement::new(statement, None).map_err(|_| StatementError::NotAStatement)?;
    let person = parsed.person().map_err(|_| StatementError::NoPerson)?;
    if person.len() > RunSubject::MAX_PERSON_LEN {
        return Err(StatementError::PersonTooLong);
    }
    let jti = parsed
        .prompt_jti()
        .map_err(|_| StatementError::NoPromptId)?;
    Ok(PromptIdentity { person, jti })
}

/// Why a `Prompt-Statement` does not name a person and a prompt. Carries no value. Classified as
/// [`ErrorClass::Invalid`]: the same statement never works.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum StatementError {
    /// Not a compact JWS: not three segments joined by dots, an empty header or signature, or a
    /// segment that is not base64url. A payload that is not a JSON object is
    /// [`NoPerson`](Self::NoPerson), because that is what `adam-model` reports for it.
    #[error("the prompt statement is not a compact JWS (three base64url segments, at most 8 KiB)")]
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
                "the Prompt-Statement is not a compact JWS (three base64url segments, at most 8 KiB)"
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
    use adam_model::MAX_PROMPT_JWS_BYTES;
    use base64::Engine as _;
    use serde_json::{Value, json};

    use super::*;

    fn statement(payload: Value) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"alg":"EdDSA","typ":"prompt-statement/v1"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&payload).unwrap());
        let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"a signature");
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
        ] {
            assert_eq!(
                person_and_jti(&bad),
                Err(StatementError::NotAStatement),
                "{bad:?}"
            );
        }
        // A payload that is a compact JWS's but not a JSON object names no person: the form is
        // `adam-model`'s now, and it does not read the payload.
        for payload in ["bm90IGpzb24", "WzEsMl0"] {
            let bad = format!("header.{payload}.signature");
            assert_eq!(
                person_and_jti(&bad),
                Err(StatementError::NoPerson),
                "{bad:?}"
            );
        }
    }

    /// The bound `adam-model` sets, which the README states: a longer statement is refused here too.
    #[test]
    fn an_oversized_statement_is_refused() {
        let oversized = format!("{}.b.c", "a".repeat(MAX_PROMPT_JWS_BYTES));
        assert_eq!(
            person_and_jti(&oversized),
            Err(StatementError::NotAStatement)
        );
        // An empty header segment, which `adam-model` refuses: not a compact JWS.
        assert_eq!(person_and_jti("..sig"), Err(StatementError::NotAStatement));
        // An empty payload is a detached payload (RFC 7515 Appendix F), which `adam-model` admits
        // and which names no person here.
        assert_eq!(person_and_jti("e30..c2ln"), Err(StatementError::NoPerson));
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
