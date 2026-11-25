//! The prompt statement of a run and the person's envelope (`prompt-statement/v1`, [ADR 0077]):
//! two signed values a client sends with a message, which every model call of the run carries to
//! the injector in front of the model as headers ([`InjectedRun`](crate::InjectedRun)).
//!
//! adam checks that each is a compact JWS of a bounded size, keeps them with the run, and sends
//! them byte for byte. Whoever verifies a signature is in front of the model, not here. The one
//! thing adam reads is the prompt's id, for its request signature ([`PromptStatement::prompt_jti`]):
//! the statement's `jti`, or, on a device with no statement, the envelope's `nonce`. Neither value is ever printed: `Debug` says only whether a value is there, and
//! no error quotes one.
//!
//! [ADR 0077]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0077-a-model-call-carries-the-prompt-statement-and-the-persons-envelope.md

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

/// The header that carries the prompt statement, the JWS as it came.
pub const PROMPT_STATEMENT_HEADER: &str = "Prompt-Statement";

/// The header that carries the person's envelope, the JWS as it came. Absent when the message
/// carried none.
pub const PROMPT_USER_SIGNATURE_HEADER: &str = "Prompt-User-Signature";

/// The longest JWS kept, in bytes, for each of the two values. A longer one is refused, never cut:
/// a header of that size passes the proxies a model endpoint sits behind, and a statement is a
/// signature over a prompt, not the prompt.
pub const MAX_PROMPT_JWS_BYTES: usize = 8 * 1024;

/// The prompt statement of a run and, when the message carried one, the person's envelope: each a
/// compact JWS (RFC 7515 §7.1) of at most [`MAX_PROMPT_JWS_BYTES`] bytes. A device, which has no
/// orchestrator to state the prompt, sends the envelope alone ([`envelope`](Self::envelope)); at
/// least one of the two is always there.
///
/// Built only through [`new`](Self::new) (deserializing checks the same), so a value of this type
/// is always safe to put in a header: three base64url segments and two dots. Its `Debug` holds no
/// value.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct PromptStatement {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    statement: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user: Option<String>,
}

impl PromptStatement {
    /// The statement `statement` and the envelope `user`, when each is a compact JWS of at most
    /// [`MAX_PROMPT_JWS_BYTES`] bytes.
    ///
    /// # Errors
    ///
    /// [`NotAPromptStatement`] naming which of the two is not one. It quotes neither.
    pub fn new(
        statement: impl Into<String>,
        user: Option<String>,
    ) -> Result<Self, NotAPromptStatement> {
        Self::from_parts(Some(statement.into()), user)
    }

    /// The person's envelope `user` alone, with no statement: a device's (the shared
    /// prompt-signature contract's device path), when it is a compact JWS of at most
    /// [`MAX_PROMPT_JWS_BYTES`] bytes.
    ///
    /// # Errors
    ///
    /// [`NotAPromptStatement::User`]. It does not quote the value.
    pub fn envelope(user: impl Into<String>) -> Result<Self, NotAPromptStatement> {
        Self::from_parts(None, Some(user.into()))
    }

    /// The statement and the envelope, each optional, and not both absent.
    ///
    /// # Errors
    ///
    /// [`NotAPromptStatement`] naming which is not a compact JWS, or
    /// [`NotAPromptStatement::Neither`].
    pub fn from_parts(
        statement: Option<String>,
        user: Option<String>,
    ) -> Result<Self, NotAPromptStatement> {
        if statement.as_deref().is_some_and(|s| !is_compact_jws(s)) {
            return Err(NotAPromptStatement::Statement);
        }
        if user.as_deref().is_some_and(|user| !is_compact_jws(user)) {
            return Err(NotAPromptStatement::User);
        }
        if statement.is_none() && user.is_none() {
            return Err(NotAPromptStatement::Neither);
        }
        Ok(Self { statement, user })
    }

    /// The statement, byte for byte as it came; `None` for a device's envelope alone.
    pub fn statement(&self) -> Option<&str> {
        self.statement.as_deref()
    }

    /// The id of the prompt a request signature binds (`prompt_jti`): the statement's `jti`
    /// (a member of its attached payload) when there is a statement, else the envelope's `nonce`
    /// (a member of its protected header, which is its `jti` on a device).
    ///
    /// # Errors
    ///
    /// [`NoPromptId`] when that member cannot be read as a non-empty string. It quotes nothing.
    pub fn prompt_jti(&self) -> Result<String, NoPromptId> {
        let (jws, segment, member) = match (&self.statement, &self.user) {
            (Some(statement), _) => (statement, 1, "jti"),
            (None, Some(user)) => (user, 0, "nonce"),
            (None, None) => return Err(NoPromptId),
        };
        jws.split('.')
            .nth(segment)
            .and_then(base64url_decode)
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|value| value.get(member)?.as_str().map(str::to_owned))
            .filter(|id| !id.is_empty())
            .ok_or(NoPromptId)
    }

    /// The person's envelope, byte for byte as it came; `None` when the message carried none.
    pub fn user(&self) -> Option<&str> {
        self.user.as_deref()
    }
}

impl fmt::Debug for PromptStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PromptStatement")
            .field("statement", &self.statement.as_ref().map(|_| "[REDACTED]"))
            .field("user", &self.user.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

impl<'de> Deserialize<'de> for PromptStatement {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            statement: Option<String>,
            #[serde(default)]
            user: Option<String>,
        }
        let raw = Raw::deserialize(deserializer)?;
        Self::from_parts(raw.statement, raw.user).map_err(serde::de::Error::custom)
    }
}

/// Which of the two values is not a compact JWS of at most [`MAX_PROMPT_JWS_BYTES`] bytes. The
/// message names the value, never its content.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum NotAPromptStatement {
    /// The statement.
    #[error(
        "the prompt statement is not a compact JWS (three base64url segments joined by dots) of at \
         most {MAX_PROMPT_JWS_BYTES} bytes"
    )]
    Statement,
    /// The person's envelope.
    #[error(
        "the person's envelope is not a compact JWS (three base64url segments joined by dots) of at \
         most {MAX_PROMPT_JWS_BYTES} bytes"
    )]
    User,
    /// Neither a statement nor an envelope.
    #[error("a prompt statement holds a statement, the person's envelope, or both")]
    Neither,
}

/// A prompt statement whose id cannot be read ([`PromptStatement::prompt_jti`]): the statement's
/// payload has no string `jti`, or the device envelope's header no string `nonce`. It quotes
/// nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "the prompt statement names no prompt id (the statement's `jti`, or the envelope's `nonce`)"
)]
pub struct NoPromptId;

/// RFC 4648 §5 without padding, the alphabet [`is_compact_jws`] admits.
fn base64url_decode(text: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text)
        .ok()
}

/// Whether `jws` is the compact serialization of a JWS: `header.payload.signature`, each segment
/// base64url without padding (RFC 7515 §2 and §7.1), the header and the signature not empty (the
/// payload may be, for a detached payload, RFC 7515 Appendix F), at most [`MAX_PROMPT_JWS_BYTES`]
/// bytes. Nothing is decoded and no signature is checked.
fn is_compact_jws(jws: &str) -> bool {
    if jws.len() > MAX_PROMPT_JWS_BYTES {
        return false;
    }
    let segments: Vec<&str> = jws.split('.').collect();
    let [header, payload, signature] = segments.as_slice() else {
        return false;
    };
    let base64url = |segment: &str| {
        segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    };
    !header.is_empty()
        && !signature.is_empty()
        && base64url(header)
        && base64url(payload)
        && base64url(signature)
}

#[cfg(test)]
mod tests {
    use super::*;

    const JWS: &str = "eyJhbGciOiJFZERTQSJ9.eyJwIjoiaGkifQ.c2lnbmF0dXJl";

    #[test]
    fn a_compact_jws_is_kept_as_it_came() {
        let both = PromptStatement::new(JWS, Some(format!("{JWS}x"))).unwrap();
        assert_eq!(both.statement(), Some(JWS));
        assert_eq!(both.user(), Some(format!("{JWS}x").as_str()));
        assert_eq!(PromptStatement::new(JWS, None).unwrap().user(), None);
        // A detached payload is a compact JWS too.
        assert!(PromptStatement::new("eyJh..c2ln", None).is_ok());
    }

    #[test]
    fn anything_else_is_refused_and_named_without_its_content() {
        for bad in [
            "",
            "a.b",
            "a.b.c.d",
            ".b.c",
            "a.b.",
            "a.b=.c",
            "a.b.c\r\nX-Evil: 1",
            "a b.c.d",
            "é.b.c",
        ] {
            let err = PromptStatement::new(bad, None).unwrap_err();
            assert_eq!(err, NotAPromptStatement::Statement, "{bad:?}");
            if !bad.is_empty() {
                assert!(!err.to_string().contains(bad), "{err}");
            }
            assert_eq!(
                PromptStatement::new(JWS, Some(bad.to_owned())).unwrap_err(),
                NotAPromptStatement::User,
                "{bad:?}"
            );
        }
        let long = format!("{}.b.c", "a".repeat(MAX_PROMPT_JWS_BYTES));
        assert!(PromptStatement::new(long, None).is_err());
        let edge = format!("{}.b.c", "a".repeat(MAX_PROMPT_JWS_BYTES - 4));
        assert_eq!(edge.len(), MAX_PROMPT_JWS_BYTES);
        assert!(PromptStatement::new(edge, None).is_ok());
    }

    #[test]
    fn debug_holds_no_value() {
        let both = PromptStatement::new(JWS, Some(JWS.to_owned())).unwrap();
        let shown = format!("{both:?}");
        assert!(!shown.contains("eyJ"), "{shown}");
        assert!(shown.contains("REDACTED"), "{shown}");
    }

    #[test]
    fn it_round_trips_and_deserializing_checks_it() {
        let both = PromptStatement::new(JWS, Some(JWS.to_owned())).unwrap();
        let json = serde_json::to_value(&both).unwrap();
        assert_eq!(json, serde_json::json!({"statement": JWS, "user": JWS}));
        assert_eq!(
            serde_json::from_value::<PromptStatement>(json).unwrap(),
            both
        );
        let only = serde_json::to_value(PromptStatement::new(JWS, None).unwrap()).unwrap();
        assert_eq!(only, serde_json::json!({"statement": JWS}));
        let err = serde_json::from_value::<PromptStatement>(serde_json::json!({"statement": "x"}))
            .unwrap_err();
        assert!(!err.to_string().contains("\"x\""), "{err}");
    }
}
