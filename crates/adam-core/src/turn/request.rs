//! The request signature: adam's signature of **one model request** as it sends it
//! (`adam-request/v1`, [ADR 0068], *Amended 2026-11-25 (the request signature)*), carried in the
//! header [`TURN_SIGNATURE_HEADER`].
//!
//! The payload is the RFC 8785 form of
//! `{typ, run, method, path, body_sha256, prompt_jti, iat, nonce}` and rides **attached**
//! ([`sign_attached`]): whoever verifies it cannot rebuild `iat` and `nonce` from the request.
//!
//! [ADR 0068]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0068-adam-signs-its-own-history.md

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::form::{SignatureType, TurnSignatureError, sign_attached, verify_attached};
use super::{KeyId, KeySet, TurnKeyError, TurnKeys};
use crate::jcs::canonicalize;

/// The header a signed model request carries its signature in.
pub const TURN_SIGNATURE_HEADER: &str = "Turn-Signature";

/// The bytes of a request signature's `nonce`, before base64url.
pub const REQUEST_NONCE_LEN: usize = 16;

/// How old a request signature's `iat` may be, in seconds, when it is verified.
pub const REQUEST_MAX_AGE_SECS: i64 = 300;

/// How far ahead of the verifier's clock a request signature's `iat` may be, in seconds.
pub const REQUEST_MAX_SKEW_SECS: i64 = 60;

/// The members of a request signature's payload, sorted as RFC 8785 writes them.
const MEMBERS: [&str; 8] = [
    "body_sha256",
    "iat",
    "method",
    "nonce",
    "path",
    "prompt_jti",
    "run",
    "typ",
];

/// One model request as adam sends it: what a request signature covers.
///
/// `Debug` shows the run, the method, the path and the body's length: never the body, which
/// holds the prompt.
#[derive(Clone, Copy)]
pub struct ModelCall<'a> {
    /// The root run the request belongs to: the value of `X-Run-Id` beside it.
    pub run: &'a str,
    /// The HTTP method, upper case (`POST`).
    pub method: &'a str,
    /// The request target as sent: the path and the query (`/v1/chat/completions`).
    pub path: &'a str,
    /// The body's exact bytes.
    pub body: &'a [u8],
    /// The `jti` of the run's prompt statement, or the `nonce` of the person's envelope when there
    /// is no statement; `None` when the run has neither, written as JSON `null`.
    pub prompt_jti: Option<&'a str>,
}

impl fmt::Debug for ModelCall<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelCall")
            .field("run", &self.run)
            .field("method", &self.method)
            .field("path", &self.path)
            .field("body_len", &self.body.len())
            .field("prompt_jti", &self.prompt_jti.map(|_| "[set]"))
            .finish()
    }
}

/// The base64url (no padding) SHA-256 of `body`: the payload's `body_sha256`.
pub fn body_sha256(body: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(body))
}

/// The RFC 8785 bytes of the payload of `request`, signed at `iat` (seconds since the epoch) with
/// the base64url `nonce`.
pub fn request_payload(request: &ModelCall<'_>, iat: i64, nonce: &str) -> Vec<u8> {
    canonicalize(&json!({
        "typ": SignatureType::Request.typ(),
        "run": request.run,
        "method": request.method,
        "path": request.path,
        "body_sha256": body_sha256(request.body),
        "prompt_jti": request.prompt_jti,
        "iat": iat,
        "nonce": nonce,
    }))
}

/// Sign `request` with the current key of `keys`, now, with a new random nonce: the value of the
/// [`TURN_SIGNATURE_HEADER`]. Each attempt of a request is signed anew.
///
/// # Errors
///
/// [`TurnKeyError`] from the keys, and [`TurnKeyError::Unavailable`] when the system gives no
/// random numbers.
pub async fn sign_request(
    keys: &(impl TurnKeys + ?Sized),
    request: &ModelCall<'_>,
) -> Result<String, TurnKeyError> {
    let mut nonce = [0_u8; REQUEST_NONCE_LEN];
    getrandom::fill(&mut nonce).map_err(|_| TurnKeyError::Unavailable("no randomness".into()))?;
    let iat = jiff::Timestamp::now().as_second();
    sign_request_at(keys, request, iat, nonce).await
}

/// [`sign_request`] with a given `iat` and `nonce`: for a test vector, never for a real request
/// (a repeated nonce is a replay).
///
/// # Errors
///
/// [`TurnKeyError`] from the keys.
pub async fn sign_request_at(
    keys: &(impl TurnKeys + ?Sized),
    request: &ModelCall<'_>,
    iat: i64,
    nonce: [u8; REQUEST_NONCE_LEN],
) -> Result<String, TurnKeyError> {
    let payload = request_payload(request, iat, &URL_SAFE_NO_PAD.encode(nonce));
    sign_attached(keys, SignatureType::Request, &payload).await
}

/// What a verified request signature says beyond the request it covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedRequest {
    /// The key that made it.
    pub kid: KeyId,
    /// When adam signed, in seconds since the epoch.
    pub iat: i64,
    /// Its nonce, base64url: with the key, what a replay store remembers.
    pub nonce: String,
}

/// Verify that `jws` is adam's signature of `request` by a key of `set`, at `now` (seconds since
/// the epoch), in the order the shared prompt-signature contract checks it: the form, the key and
/// the signature; then each member against the request (`run`, `method`, `path`, `body_sha256`,
/// `prompt_jti`); then `iat` within [`REQUEST_MAX_AGE_SECS`] before and [`REQUEST_MAX_SKEW_SECS`]
/// after `now`. A replay store is the verifier's, not this function's.
///
/// Trusting a certificate chain in the header (`x5c`) is the verifier's too: this verifies by the
/// header's `kid` against `set`.
///
/// # Errors
///
/// [`TurnSignatureError`]: `Malformed` for anything but the form adam writes (a payload that is not
/// the RFC 8785 form of exactly its eight members, a nonce that is not 16 bytes), the key and
/// signature errors of [`verify_attached`], [`Mismatch`](TurnSignatureError::Mismatch) naming the
/// first member that differs, and [`Expired`](TurnSignatureError::Expired).
pub fn verify_request(
    set: &KeySet,
    jws: &str,
    request: &ModelCall<'_>,
    now: i64,
) -> Result<VerifiedRequest, TurnSignatureError> {
    let (kid, payload) = verify_attached(set, SignatureType::Request, jws)?;
    let value: Value =
        serde_json::from_slice(&payload).map_err(|_| TurnSignatureError::Malformed)?;
    let Value::Object(map) = &value else {
        return Err(TurnSignatureError::Malformed);
    };
    if canonicalize(&value) != payload
        || map.len() != MEMBERS.len()
        || !MEMBERS.iter().all(|m| map.contains_key(*m))
    {
        return Err(TurnSignatureError::Malformed);
    }
    let text = |name: &str| map.get(name).and_then(Value::as_str);
    if text("typ") != Some(SignatureType::Request.typ()) {
        return Err(TurnSignatureError::WrongType);
    }
    let iat = map
        .get("iat")
        .and_then(Value::as_i64)
        .ok_or(TurnSignatureError::Malformed)?;
    let nonce = text("nonce")
        .filter(|n| {
            URL_SAFE_NO_PAD
                .decode(n)
                .is_ok_and(|b| b.len() == REQUEST_NONCE_LEN)
        })
        .ok_or(TurnSignatureError::Malformed)?
        .to_owned();
    let prompt_jti = match map.get("prompt_jti") {
        Some(Value::Null) => None,
        Some(Value::String(jti)) => Some(jti.as_str()),
        _ => return Err(TurnSignatureError::Malformed),
    };
    let digest = body_sha256(request.body);
    for (member, wanted) in [
        ("run", request.run),
        ("method", request.method),
        ("path", request.path),
        ("body_sha256", digest.as_str()),
    ] {
        if text(member) != Some(wanted) {
            return Err(TurnSignatureError::Mismatch { member });
        }
    }
    if prompt_jti != request.prompt_jti {
        return Err(TurnSignatureError::Mismatch {
            member: "prompt_jti",
        });
    }
    if iat < now - REQUEST_MAX_AGE_SECS || iat > now + REQUEST_MAX_SKEW_SECS {
        return Err(TurnSignatureError::Expired);
    }
    Ok(VerifiedRequest { kid, iat, nonce })
}
