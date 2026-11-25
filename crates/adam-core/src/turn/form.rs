//! The form of a signature: a compact JWS, its payload detached (RFC 7515 appendix F,
//! `BASE64URL(header)..BASE64URL(signature)`) for what adam keeps, or attached
//! (`BASE64URL(header).BASE64URL(payload).BASE64URL(signature)`) for a model request, whose
//! verifier cannot rebuild the payload ([`SignatureType::Request`]).

use adam_error::{Classify, ErrorClass};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use serde_json::{Value, json};

use super::{CertificateChain, KeyId, KeyIdentity, KeySet, Signature, TurnKeyError, TurnKeys};
use crate::jcs::canonicalize;

/// The longest signature accepted, in bytes: a header with a [`KeyId::MAX_LEN`] id and a full
/// certificate chain fits; anything longer is refused before it is decoded.
pub const MAX_SIGNATURE_LEN: usize = 16 * 1024;

/// What a signature covers: its header's `typ` ([ADR 0068] decisions 3 and 4). A signature made
/// for one never verifies as another.
///
/// Closed on purpose: each is its own payload.
///
/// [ADR 0068]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0068-adam-signs-its-own-history.md
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SignatureType {
    /// An assistant or tool message at its place in a run (`adam-turn/v1`, [`turn_payload`](super::turn_payload)).
    Turn,
    /// A run's head (`adam-head/v1`; its payload is slice T3's).
    Head,
    /// A journal entry (`adam-step/v1`; its payload is slice T3's).
    Step,
    /// One model request as adam sends it (`adam-request/v1`, [`request_payload`](super::request_payload)):
    /// the payload is **attached** ([`sign_attached`]), never kept, and a history signature never
    /// verifies as one ([ADR 0068], *Amended 2026-11-25 (the request signature)*).
    Request,
}

impl SignatureType {
    /// The header's `typ`.
    pub const fn typ(self) -> &'static str {
        match self {
            Self::Turn => "adam-turn/v1",
            Self::Head => "adam-head/v1",
            Self::Step => "adam-step/v1",
            Self::Request => "adam-request/v1",
        }
    }
}

fn header(identity: &KeyIdentity, typ: SignatureType) -> Vec<u8> {
    let mut header = json!({"alg": "EdDSA", "kid": identity.kid().as_str(), "typ": typ.typ()});
    if let (Some(chain), Value::Object(map)) = (identity.chain(), &mut header) {
        let x5c: Vec<Value> = chain
            .certificates()
            .iter()
            .map(|der| json!(STANDARD.encode(der)))
            .collect();
        map.insert("x5c".into(), Value::Array(x5c));
    }
    canonicalize(&header)
}

fn signing_input(protected: &str, payload: &[u8]) -> Vec<u8> {
    let mut input = Vec::with_capacity(protected.len() + 1 + payload.len() * 4 / 3 + 4);
    input.extend_from_slice(protected.as_bytes());
    input.push(b'.');
    input.extend_from_slice(URL_SAFE_NO_PAD.encode(payload).as_bytes());
    input
}

/// Sign `payload` as `typ` with the current key of `keys`: the detached JWS, whose header is the
/// RFC 8785 form of `{"alg":"EdDSA","kid":…,"typ":…}` (and `x5c` when the key is named by a
/// certificate chain).
///
/// # Errors
///
/// [`TurnKeyError`] from the keys.
pub async fn sign_detached(
    keys: &(impl TurnKeys + ?Sized),
    typ: SignatureType,
    payload: &[u8],
) -> Result<String, TurnKeyError> {
    let (protected, signature) = sign_compact(keys, typ, payload).await?;
    Ok(format!("{protected}..{signature}"))
}

/// Sign `payload` as `typ` with the current key of `keys`, the payload **attached**:
/// `BASE64URL(header).BASE64URL(payload).BASE64URL(signature)`. The header and the signing input
/// are byte for byte those of [`sign_detached`]; only the middle segment differs.
///
/// # Errors
///
/// [`TurnKeyError`] from the keys.
pub async fn sign_attached(
    keys: &(impl TurnKeys + ?Sized),
    typ: SignatureType,
    payload: &[u8],
) -> Result<String, TurnKeyError> {
    let (protected, signature) = sign_compact(keys, typ, payload).await?;
    Ok(format!(
        "{protected}.{}.{signature}",
        URL_SAFE_NO_PAD.encode(payload)
    ))
}

/// The protected header and the signature, each base64url, of `payload` signed as `typ`.
async fn sign_compact(
    keys: &(impl TurnKeys + ?Sized),
    typ: SignatureType,
    payload: &[u8],
) -> Result<(String, String), TurnKeyError> {
    let current = keys.current().await?;
    let protected = URL_SAFE_NO_PAD.encode(header(&current.identity, typ));
    let signature = keys
        .sign(current.kid(), &signing_input(&protected, payload))
        .await?;
    Ok((protected, URL_SAFE_NO_PAD.encode(signature.to_bytes())))
}

/// Verify that `jws` is a signature of `payload` as `typ` by a key of `set`; the id of the key
/// that made it.
///
/// Fails closed on anything but the form [`sign_detached`] makes: a header that is not the RFC 8785
/// form of exactly `alg`, `kid`, `typ` (and `x5c`), another `alg` or `typ`, an attached payload, a
/// signature that is not 64 bytes, or more than [`MAX_SIGNATURE_LEN`] bytes in all.
///
/// # Errors
///
/// [`TurnSignatureError`], all of class `Corrupt`.
pub fn verify_detached(
    set: &KeySet,
    typ: SignatureType,
    payload: &[u8],
    jws: &str,
) -> Result<KeyId, TurnSignatureError> {
    if jws.len() > MAX_SIGNATURE_LEN {
        return Err(TurnSignatureError::TooLong);
    }
    let mut parts = jws.split('.');
    let (Some(protected), Some(""), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(TurnSignatureError::Malformed);
    };
    verify_compact(set, typ, protected, payload, signature)
}

/// Verify that `jws` is an attached signature as `typ` by a key of `set` ([`sign_attached`]): the
/// id of the key that made it and the payload it carries. What the payload says is the caller's to
/// check ([`verify_request`](super::verify_request) for a request).
///
/// Fails closed as [`verify_detached`] does, and on an empty or detached payload.
///
/// # Errors
///
/// [`TurnSignatureError`], all of class `Corrupt`.
pub fn verify_attached(
    set: &KeySet,
    typ: SignatureType,
    jws: &str,
) -> Result<(KeyId, Vec<u8>), TurnSignatureError> {
    if jws.len() > MAX_SIGNATURE_LEN {
        return Err(TurnSignatureError::TooLong);
    }
    let mut parts = jws.split('.');
    let (Some(protected), Some(encoded), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(TurnSignatureError::Malformed);
    };
    if encoded.is_empty() {
        return Err(TurnSignatureError::Malformed);
    }
    let payload = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| TurnSignatureError::Malformed)?;
    // One encoding only: the segment must be the payload's own base64url, so the bytes signed
    // (`header.segment`) are the bytes the payload re-encodes to.
    if URL_SAFE_NO_PAD.encode(&payload) != encoded {
        return Err(TurnSignatureError::Malformed);
    }
    let kid = verify_compact(set, typ, protected, &payload, signature)?;
    Ok((kid, payload))
}

fn verify_compact(
    set: &KeySet,
    typ: SignatureType,
    protected: &str,
    payload: &[u8],
    signature: &str,
) -> Result<KeyId, TurnSignatureError> {
    let raw = URL_SAFE_NO_PAD
        .decode(protected)
        .map_err(|_| TurnSignatureError::Malformed)?;
    let kid = read_header(&raw, typ)?;
    let signature = URL_SAFE_NO_PAD
        .decode(signature)
        .ok()
        .and_then(|s| <[u8; 64]>::try_from(s).ok())
        .map(Signature::from_bytes)
        .ok_or(TurnSignatureError::Malformed)?;
    if set.is_revoked(&kid) {
        return Err(TurnSignatureError::Revoked { kid });
    }
    let Some(key) = set.key(&kid) else {
        return Err(TurnSignatureError::UnknownKey { kid });
    };
    if key.verifies(&signing_input(protected, payload), &signature) {
        Ok(kid)
    } else {
        Err(TurnSignatureError::Invalid { kid })
    }
}

fn read_header(raw: &[u8], typ: SignatureType) -> Result<KeyId, TurnSignatureError> {
    let value: Value = serde_json::from_slice(raw).map_err(|_| TurnSignatureError::Malformed)?;
    let Value::Object(map) = &value else {
        return Err(TurnSignatureError::Malformed);
    };
    // One encoding only: the bytes signed are the canonical form, so a header written any other
    // way was not written by adam.
    if canonicalize(&value) != raw
        || map
            .keys()
            .any(|k| !matches!(k.as_str(), "alg" | "kid" | "typ" | "x5c"))
    {
        return Err(TurnSignatureError::Malformed);
    }
    let text = |name: &str| map.get(name).and_then(Value::as_str);
    if text("alg") != Some("EdDSA") {
        return Err(TurnSignatureError::Malformed);
    }
    if text("typ") != Some(typ.typ()) {
        return Err(TurnSignatureError::WrongType);
    }
    if let Some(x5c) = map.get("x5c") {
        read_x5c(x5c)?;
    }
    text("kid")
        .and_then(|kid| KeyId::new(kid).ok())
        .ok_or(TurnSignatureError::Malformed)
}

fn read_x5c(x5c: &Value) -> Result<CertificateChain, TurnSignatureError> {
    let der = x5c
        .as_array()
        .ok_or(TurnSignatureError::Malformed)?
        .iter()
        .map(|c| c.as_str().and_then(|c| STANDARD.decode(c).ok()))
        .collect::<Option<Vec<_>>>()
        .ok_or(TurnSignatureError::Malformed)?;
    CertificateChain::new(der).map_err(|_| TurnSignatureError::Malformed)
}

/// Why a signature does not verify. Every one is class `Corrupt`: a stored row, a journal entry or a
/// history that does not verify was not made by adam, or was changed ([ADR 0068] decision 6). It
/// names the key by its id, never by its bytes, and holds nothing of the signed content.
///
/// [ADR 0068]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0068-adam-signs-its-own-history.md
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TurnSignatureError {
    /// There is no signature where one must be.
    #[error("turn signature: missing")]
    Unsigned,
    /// The signature is longer than [`MAX_SIGNATURE_LEN`]; it was not read.
    #[error("turn signature: longer than {MAX_SIGNATURE_LEN} bytes")]
    TooLong,
    /// It is not the form adam writes.
    #[error("turn signature: not a detached EdDSA JWS of the form adam writes")]
    Malformed,
    /// It was made for another payload type (a head's signature on a turn).
    #[error("turn signature: made for another type of payload")]
    WrongType,
    /// Its key is not in the key set.
    #[error("turn signature: the key `{kid}` is not in the key set")]
    UnknownKey {
        /// The key the signature names.
        kid: KeyId,
    },
    /// Its key is revoked.
    #[error("turn signature: the key `{kid}` is revoked")]
    Revoked {
        /// The key the signature names.
        kid: KeyId,
    },
    /// It does not verify: the content, its place or the signature changed.
    #[error("turn signature: does not verify with the key `{kid}`")]
    Invalid {
        /// The key the signature names.
        kid: KeyId,
    },
    /// A request signature verifies, and covers another request: the member named differs from
    /// the request it came with (`run`, `method`, `path`, `body_sha256`, `prompt_jti`).
    #[error("turn signature: its `{member}` is not the request's")]
    Mismatch {
        /// The payload member that differs. Never its value.
        member: &'static str,
    },
    /// A request signature verifies, and its `iat` is outside the window.
    #[error("turn signature: its `iat` is outside the window")]
    Expired,
}

impl Classify for TurnSignatureError {
    fn class(&self) -> ErrorClass {
        ErrorClass::Corrupt
    }
}
