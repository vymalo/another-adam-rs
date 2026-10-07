//! Agent card signatures: a JWS over the canonicalized card (A2A 1.0 §8.4).
//!
//! **What the specification asks** (*verified* 2026-10-07,
//! <https://a2a-protocol.org/latest/specification/> §4.4.7 and §8.4):
//!
//! * the signed content is the card as JSON with the `signatures` field removed and the properties
//!   with default values removed, canonicalized with the **JSON Canonicalization Scheme, RFC 8785**;
//! * the signature is a JWS (RFC 7515): the protected header holds `alg` (for example `ES256`),
//!   `typ` (should be `JOSE`) and `kid`, and may hold `jku` (a JWKS URL); the signing input is
//!   `BASE64URL(protected) || "." || BASE64URL(canonical payload)`; the card carries
//!   `{"protected": ..., "signature": ...}` in `signatures`, base64url without padding;
//! * a verifier fetches the key by `kid` and `jku` (or from a trusted key store), canonicalizes the
//!   received card the same way and verifies.
//!
//! **What this crate does with it.** [`CardSigner`] holds one private key (PKCS#8 PEM; ES256 on
//! P-256 or EdDSA on Ed25519, by the key's type; the key never leaves the process and is never
//! logged) and signs the public card and the extended card when the server is built. A `kid` is
//! the RFC 7638 thumbprint of the public key unless one is configured. `jku` is in the header only
//! if configured, and the server publishes the key set at `GET /.well-known/jwks.json` when
//! signing is on, so a `jku` that points at the server itself works. [`VerifyingKey`] verifies a
//! card, for clients and tests.
//!
//! **The default-value rule, as implemented.** The specification's "remove properties with default
//! values" is stated for protobuf field presence, which JSON does not carry. This crate takes the
//! card exactly as it serves it (optional fields that were not set are already absent) and
//! additionally drops `null`s and empty arrays and objects, except the fields the specification
//! marks REQUIRED (`capabilities`, `defaultInputModes`, `defaultOutputModes`, `skills`,
//! `supportedInterfaces` and a skill's `tags`), which stay even when empty, and an extension's
//! `required: false`, which proto3 JSON drops as a default. The booleans of `capabilities`
//! (`streaming`, `pushNotifications`, `extendedAgentCard`) are `optional` in the protocol, so
//! they are kept when set, even to `false`, as the specification says for explicitly set optional
//! fields. `securityRequirements` and an extension's `params` are signed as they are. This matters
//! because the same card reaches a client in two JSON forms: the card route serves serde's JSON
//! and the JSON-RPC route (`GetExtendedAgentCard`) the SDK's proto3 JSON, which drops defaults
//! and spells security requirements differently; the client's `AgentCard` parses both to the same
//! value, and the payload is made from that value. Signer and verifier here share the one function
//! ([`canonical_payload`]); *unverified:* byte-for-byte agreement with other SDKs' canonical
//! payloads, which no published test vector in the specification confirms.
//!
//! The canonicalization is RFC 8785: object members sorted by their UTF-16 code units, no
//! insignificant whitespace, strings escaped minimally, numbers as ECMAScript prints them.

use std::sync::Arc;

use a2a::{AgentCard, AgentCardSignature};
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{
    ECDSA_P256_SHA256_FIXED, ECDSA_P256_SHA256_FIXED_SIGNING, ED25519, EcdsaKeyPair,
    Ed25519KeyPair, KeyPair, UnparsedPublicKey,
};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rustls_pki_types::PrivateKeyDer;
use rustls_pki_types::pem::PemObject as _;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

/// Why a signing key could not be used, or a card could not be signed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SigningError {
    /// The PEM holds no private key, or not a PKCS#8 one.
    #[error(
        "the signing key is not a PKCS#8 PEM private key (generate one with `openssl genpkey`)"
    )]
    NotPkcs8,
    /// The key is PKCS#8 but not ECDSA P-256 or Ed25519.
    #[error("the signing key is neither ECDSA P-256 (ES256) nor Ed25519 (EdDSA)")]
    UnsupportedKey,
    /// The configured `kid` or `jku` is empty or has a control character.
    #[error(
        "the signing key id and the key set URL must be non-empty text without control characters"
    )]
    BadIdentifier,
    /// The card could not be serialised.
    #[error("the agent card cannot be serialised")]
    Serialize(#[source] serde_json::Error),
    /// The signing primitive failed.
    #[error("signing failed")]
    Sign,
}

/// Why a card does not verify.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum VerifyError {
    /// The card has no signatures.
    #[error("the card carries no signature")]
    Unsigned,
    /// No signature of the card verifies with this key (altered card, other key, other
    /// algorithm, malformed signature).
    #[error("no signature of the card verifies with this key")]
    Invalid,
    /// The public key given cannot be read.
    #[error("the public key cannot be read")]
    BadKey,
    /// The card cannot be serialised.
    #[error("the card cannot be canonicalized")]
    Canonicalize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Alg {
    Es256,
    EdDsa,
}

impl Alg {
    fn name(self) -> &'static str {
        match self {
            Self::Es256 => "ES256",
            Self::EdDsa => "EdDSA",
        }
    }
}

enum Key {
    Es256(EcdsaKeyPair),
    EdDsa(Ed25519KeyPair),
}

/// Signs agent cards. Cheap to clone; never prints its key.
#[derive(Clone)]
pub struct CardSigner {
    key: Arc<Key>,
    kid: String,
    jku: Option<String>,
}

impl std::fmt::Debug for CardSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CardSigner")
            .field("alg", &self.alg().name())
            .field("kid", &self.kid)
            .field("jku", &self.jku)
            .finish_non_exhaustive()
    }
}

impl CardSigner {
    /// A signer for the PKCS#8 PEM private key `pem`, with `kid` as the key id (`None`: the
    /// RFC 7638 thumbprint of the public key).
    ///
    /// # Errors
    ///
    /// [`SigningError`] when the key cannot be used or `kid` is not usable text.
    pub fn from_pem(pem: &str, kid: Option<&str>) -> Result<Self, SigningError> {
        let der = match PrivateKeyDer::from_pem_slice(pem.as_bytes()) {
            Ok(PrivateKeyDer::Pkcs8(der)) => der,
            _ => return Err(SigningError::NotPkcs8),
        };
        let der = der.secret_pkcs8_der();
        let key = if let Ok(ec) = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, der) {
            Key::Es256(ec)
        } else if let Ok(ed) = Ed25519KeyPair::from_pkcs8_maybe_unchecked(der) {
            Key::EdDsa(ed)
        } else {
            return Err(SigningError::UnsupportedKey);
        };
        let mut signer = Self {
            key: Arc::new(key),
            kid: String::new(),
            jku: None,
        };
        signer.kid = match kid {
            Some(kid) => checked(kid)?.to_owned(),
            None => thumbprint(&signer.public_jwk_members()),
        };
        Ok(signer)
    }

    /// Put `jku`, the URL of the key set, in the protected header. The server publishes the key
    /// set at `/.well-known/jwks.json`; point this at wherever clients should fetch it.
    ///
    /// # Errors
    ///
    /// [`SigningError::BadIdentifier`] for an empty URL or one with a control character.
    pub fn with_jku(mut self, jku: &str) -> Result<Self, SigningError> {
        self.jku = Some(checked(jku)?.to_owned());
        Ok(self)
    }

    /// The key id in the protected header.
    pub fn kid(&self) -> &str {
        &self.kid
    }

    /// The `jku` in the protected header, if configured.
    pub fn jku(&self) -> Option<&str> {
        self.jku.as_deref()
    }

    /// The JOSE algorithm name: `ES256` or `EdDSA`.
    pub fn algorithm(&self) -> &'static str {
        self.alg().name()
    }

    fn alg(&self) -> Alg {
        match &*self.key {
            Key::Es256(_) => Alg::Es256,
            Key::EdDsa(_) => Alg::EdDsa,
        }
    }

    /// The public key as the members of its JWK (without `kid`, `alg`, `use`).
    fn public_jwk_members(&self) -> Map<String, Value> {
        let mut m = Map::new();
        match &*self.key {
            Key::Es256(k) => {
                let point = k.public_key().as_ref();
                // Uncompressed SEC1 point: 0x04, x (32 bytes), y (32 bytes).
                m.insert("kty".into(), json!("EC"));
                m.insert("crv".into(), json!("P-256"));
                if point.len() == 65 {
                    m.insert("x".into(), json!(URL_SAFE_NO_PAD.encode(&point[1..33])));
                    m.insert("y".into(), json!(URL_SAFE_NO_PAD.encode(&point[33..65])));
                }
            }
            Key::EdDsa(k) => {
                m.insert("kty".into(), json!("OKP"));
                m.insert("crv".into(), json!("Ed25519"));
                m.insert(
                    "x".into(),
                    json!(URL_SAFE_NO_PAD.encode(k.public_key().as_ref())),
                );
            }
        }
        m
    }

    /// The public key as a JWKS (RFC 7517), what the server publishes at
    /// `/.well-known/jwks.json` and what `jku` points to.
    pub fn jwks(&self) -> Value {
        let mut jwk = self.public_jwk_members();
        jwk.insert("kid".into(), json!(self.kid));
        jwk.insert("alg".into(), json!(self.algorithm()));
        jwk.insert("use".into(), json!("sig"));
        json!({ "keys": [Value::Object(jwk)] })
    }

    /// The public half, to verify with.
    pub fn verifying_key(&self) -> VerifyingKey {
        let public = match &*self.key {
            Key::Es256(k) => k.public_key().as_ref().to_vec(),
            Key::EdDsa(k) => k.public_key().as_ref().to_vec(),
        };
        VerifyingKey {
            alg: self.alg(),
            public,
            kid: Some(self.kid.clone()),
        }
    }

    /// Sign `card` (its `signatures` are ignored): the JWS of the canonical payload.
    ///
    /// # Errors
    ///
    /// [`SigningError`] when the card cannot be serialised or the primitive fails.
    pub fn sign(&self, card: &AgentCard) -> Result<AgentCardSignature, SigningError> {
        let payload = canonical_payload(card).map_err(SigningError::Serialize)?;
        let kid = serde_json::to_string(&self.kid).map_err(SigningError::Serialize)?;
        let mut header = format!(r#"{{"alg":"{}","typ":"JOSE","kid":{kid}"#, self.algorithm());
        if let Some(jku) = &self.jku {
            let jku = serde_json::to_string(jku).map_err(SigningError::Serialize)?;
            header.push_str(&format!(r#","jku":{jku}"#));
        }
        header.push('}');
        let protected = URL_SAFE_NO_PAD.encode(header.as_bytes());
        let input = format!("{protected}.{}", URL_SAFE_NO_PAD.encode(payload));
        let signature = match &*self.key {
            Key::Es256(k) => k
                .sign(&SystemRandom::new(), input.as_bytes())
                .map_err(|_| SigningError::Sign)?
                .as_ref()
                .to_vec(),
            Key::EdDsa(k) => k.sign(input.as_bytes()).as_ref().to_vec(),
        };
        Ok(AgentCardSignature {
            protected,
            signature: URL_SAFE_NO_PAD.encode(signature),
            header: None,
        })
    }

    /// `card` with its `signatures` replaced by one signature of its content.
    ///
    /// # Errors
    ///
    /// As [`sign`](Self::sign).
    pub fn sign_card(&self, mut card: AgentCard) -> Result<AgentCard, SigningError> {
        card.signatures = None;
        let signature = self.sign(&card)?;
        card.signatures = Some(vec![signature]);
        Ok(card)
    }
}

fn checked(text: &str) -> Result<&str, SigningError> {
    if text.is_empty() || text.chars().any(char::is_control) {
        Err(SigningError::BadIdentifier)
    } else {
        Ok(text)
    }
}

/// RFC 7638: the SHA-256 of the JWK's required members in lexicographic order, base64url.
fn thumbprint(members: &Map<String, Value>) -> String {
    // `Map` is a BTreeMap (no `preserve_order`): members are already in lexicographic order, and
    // `to_string` has no whitespace.
    let text = Value::Object(members.clone()).to_string();
    URL_SAFE_NO_PAD.encode(Sha256::digest(text.as_bytes()))
}

/// A public key that verifies agent cards.
#[derive(Clone, Debug)]
pub struct VerifyingKey {
    alg: Alg,
    public: Vec<u8>,
    kid: Option<String>,
}

impl VerifyingKey {
    /// A key from a JWK (an object with `kty`, `crv`, `x` and, for P-256, `y`): `EC`/`P-256` for
    /// ES256, `OKP`/`Ed25519` for EdDSA. A JWKS (`{"keys": [...]}`) is accepted too: the key with
    /// `kid` if given, else the only key.
    ///
    /// # Errors
    ///
    /// [`VerifyError::BadKey`] when the JWK cannot be read.
    pub fn from_jwk(jwk: &Value, kid: Option<&str>) -> Result<Self, VerifyError> {
        if let Some(keys) = jwk.get("keys").and_then(Value::as_array) {
            let pick = match kid {
                Some(kid) => keys
                    .iter()
                    .find(|k| k.get("kid").and_then(Value::as_str) == Some(kid)),
                None if keys.len() == 1 => keys.first(),
                None => None,
            };
            return Self::from_jwk(pick.ok_or(VerifyError::BadKey)?, kid);
        }
        let field = |name: &str| -> Result<Vec<u8>, VerifyError> {
            let text = jwk
                .get(name)
                .and_then(Value::as_str)
                .ok_or(VerifyError::BadKey)?;
            URL_SAFE_NO_PAD
                .decode(text)
                .map_err(|_| VerifyError::BadKey)
        };
        let kty = jwk.get("kty").and_then(Value::as_str);
        let crv = jwk.get("crv").and_then(Value::as_str);
        let (alg, public) = match (kty, crv) {
            (Some("EC"), Some("P-256")) => {
                let (x, y) = (field("x")?, field("y")?);
                if x.len() != 32 || y.len() != 32 {
                    return Err(VerifyError::BadKey);
                }
                let mut point = vec![0x04];
                point.extend(x);
                point.extend(y);
                (Alg::Es256, point)
            }
            (Some("OKP"), Some("Ed25519")) => {
                let x = field("x")?;
                if x.len() != 32 {
                    return Err(VerifyError::BadKey);
                }
                (Alg::EdDsa, x)
            }
            _ => return Err(VerifyError::BadKey),
        };
        let kid = kid
            .map(str::to_owned)
            .or_else(|| jwk.get("kid").and_then(Value::as_str).map(str::to_owned));
        Ok(Self { alg, public, kid })
    }

    /// Verify that `card` carries a signature made with this key over its current content.
    /// Every signature is tried; one valid one is enough. A signature whose header names another
    /// algorithm than the key's (or `none`), or another `kid` than this key's, is skipped.
    ///
    /// # Errors
    ///
    /// [`VerifyError::Unsigned`] without signatures; [`VerifyError::Invalid`] when none verifies.
    pub fn verify_card(&self, card: &AgentCard) -> Result<(), VerifyError> {
        let signatures = card
            .signatures
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or(VerifyError::Unsigned)?;
        let payload = canonical_payload(card).map_err(|_| VerifyError::Canonicalize)?;
        let payload = URL_SAFE_NO_PAD.encode(payload);
        let valid = signatures.iter().any(|s| self.verifies(s, &payload));
        if valid {
            Ok(())
        } else {
            Err(VerifyError::Invalid)
        }
    }

    fn verifies(&self, signature: &AgentCardSignature, payload_b64: &str) -> bool {
        let Ok(header) = URL_SAFE_NO_PAD.decode(&signature.protected) else {
            return false;
        };
        let Ok(header) = serde_json::from_slice::<Value>(&header) else {
            return false;
        };
        if header.get("alg").and_then(Value::as_str) != Some(self.alg.name()) {
            return false;
        }
        if let (Some(want), Some(got)) = (&self.kid, header.get("kid").and_then(Value::as_str))
            && want != got
        {
            return false;
        }
        let Ok(raw) = URL_SAFE_NO_PAD.decode(&signature.signature) else {
            return false;
        };
        let input = format!("{}.{payload_b64}", signature.protected);
        let algorithm: &'static dyn aws_lc_rs::signature::VerificationAlgorithm = match self.alg {
            Alg::Es256 => &ECDSA_P256_SHA256_FIXED,
            Alg::EdDsa => &ED25519,
        };
        UnparsedPublicKey::new(algorithm, &self.public)
            .verify(input.as_bytes(), &raw)
            .is_ok()
    }
}

/// The bytes that are signed: the card without `signatures`, without defaults (see the module
/// docs), canonicalized with RFC 8785.
///
/// # Errors
///
/// The card cannot be serialised.
pub fn canonical_payload(card: &AgentCard) -> Result<Vec<u8>, serde_json::Error> {
    let mut value = serde_json::to_value(card)?;
    if let Value::Object(map) = &mut value {
        map.remove("signatures");
    }
    Ok(canonicalize(&strip_defaults(value)))
}

/// Fields the specification marks REQUIRED: kept even when empty.
const KEEP_EMPTY: [&str; 6] = [
    "capabilities",
    "defaultInputModes",
    "defaultOutputModes",
    "skills",
    "supportedInterfaces",
    "tags",
];

/// Members whose content is the deployment's own data, not the card's schema: left exactly as
/// it is (security requirements say *which* scheme with *which* scopes, and an empty scope list
/// still means something).
const OPAQUE: [&str; 2] = ["securityRequirements", "params"];

/// The card's JSON in the form that is signed: no `null`s, no empty arrays or objects except the
/// REQUIRED ones, and none of the fields that proto3 JSON drops because they hold their default
/// (`required: false` of an extension). The same card reaches a client in either form (the card
/// route serves serde's JSON, the JSON-RPC route the SDK's proto3 JSON, which drops those), and
/// both must canonicalize alike.
fn strip_defaults(value: Value) -> Value {
    normalize(value, "")
}

/// `context` is the name of the member the value is in, or of the array it is an element of.
fn normalize(value: Value, context: &str) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter_map(|(k, v)| {
                    if v.is_null() {
                        return None;
                    }
                    // An element of `extensions`: `required` is a plain proto3 bool.
                    if context == "extensions" && k == "required" && v == Value::Bool(false) {
                        return None;
                    }
                    if OPAQUE.contains(&k.as_str()) {
                        return Some((k, v));
                    }
                    let v = normalize(v, &k);
                    let empty = match &v {
                        Value::Array(a) => a.is_empty(),
                        Value::Object(o) => o.is_empty(),
                        _ => false,
                    };
                    (!empty || KEEP_EMPTY.contains(&k.as_str())).then_some((k, v))
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| normalize(item, context))
                .collect(),
        ),
        other => other,
    }
}

/// RFC 8785 (JCS) of `value`.
pub fn canonicalize(value: &Value) -> Vec<u8> {
    let mut out = String::new();
    write_value(value, &mut out);
    out.into_bytes()
}

fn write_value(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => write_number(n, out),
        Value::String(s) => write_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut members: Vec<(&String, &Value)> = map.iter().collect();
            // RFC 8785 §3.2.3: sorted by the UTF-16 code units of the names.
            members.sort_by(|(a, _), (b, _)| a.encode_utf16().cmp(b.encode_utf16()));
            out.push('{');
            for (i, (k, v)) in members.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(k, out);
                out.push(':');
                write_value(v, out);
            }
            out.push('}');
        }
    }
}

/// RFC 8785 §3.2.2.2: only `"` and `\` and the control characters are escaped, the control
/// characters with the short forms where JSON has them and `\u00xx` (lowercase) otherwise.
fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{9}' => out.push_str("\\t"),
            '\u{a}' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\u{d}' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// RFC 8785 §3.2.2.3: numbers as ECMAScript's `Number::toString` prints them (via `ryu-js`).
fn write_number(n: &serde_json::Number, out: &mut String) {
    const EXACT: u64 = 1 << 53;
    if let Some(i) = n.as_i64()
        && i.unsigned_abs() <= EXACT
    {
        out.push_str(&i.to_string());
    } else if let Some(u) = n.as_u64()
        && u <= EXACT
    {
        out.push_str(&u.to_string());
    } else if let Some(f) = n.as_f64() {
        if f == 0.0 {
            out.push('0'); // -0 prints as 0
        } else {
            out.push_str(ryu_js::Buffer::new().format_finite(f));
        }
    } else {
        out.push_str(&n.to_string());
    }
}

/// A fresh private key as a PKCS#8 PEM, for tests and examples: Ed25519 when `ed25519`, else
/// ECDSA P-256. Nothing is protected by it.
///
/// # Panics
///
/// The system random number generator fails.
#[cfg(any(test, feature = "test-util"))]
#[allow(
    clippy::expect_used,
    reason = "a test helper: a failing RNG is a failed test"
)]
pub fn generate_signing_key_pem(ed25519: bool) -> String {
    let rng = SystemRandom::new();
    let der = if ed25519 {
        Ed25519KeyPair::generate_pkcs8(&rng)
            .expect("generate an Ed25519 key")
            .as_ref()
            .to_vec()
    } else {
        EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
            .expect("generate a P-256 key")
            .as_ref()
            .to_vec()
    };
    let body = base64::engine::general_purpose::STANDARD.encode(der);
    let mut pem = String::from("-----BEGIN PRIVATE KEY-----\n");
    for line in body.as_bytes().chunks(64) {
        pem.push_str(&String::from_utf8_lossy(line));
        pem.push('\n');
    }
    pem.push_str("-----END PRIVATE KEY-----\n");
    pem
}

#[cfg(test)]
mod tests {
    use a2a::{AgentCapabilities, AgentInterface, AgentSkill};

    use super::*;

    fn card() -> AgentCard {
        AgentCard {
            name: "Example Agent".into(),
            description: String::new(),
            version: "1.0.0".into(),
            supported_interfaces: vec![AgentInterface::new("https://example.com/a2a", "JSONRPC")],
            capabilities: AgentCapabilities {
                streaming: Some(false),
                push_notifications: Some(false),
                extensions: Some(vec![]),
                extended_agent_card: None,
            },
            default_input_modes: vec!["text/plain".into()],
            default_output_modes: vec!["text/plain".into()],
            skills: vec![AgentSkill {
                id: "s".into(),
                name: "S".into(),
                description: "d".into(),
                tags: vec![],
                examples: Some(vec![]),
                input_modes: None,
                output_modes: None,
                security_requirements: None,
            }],
            provider: None,
            documentation_url: None,
            icon_url: None,
            security_schemes: None,
            security_requirements: None,
            signatures: None,
        }
    }

    fn canon(v: &Value) -> String {
        String::from_utf8(canonicalize(v)).unwrap()
    }

    /// The ordering rule of RFC 8785 §3.2.3 and number formatting of §3.2.2.3 (the expected
    /// strings are what ECMAScript prints for these numbers).
    #[test]
    fn jcs_orders_members_by_utf16_and_prints_numbers_as_ecmascript_does() {
        // §3.2.3: names sort by UTF-16 code units, so the emoji (a surrogate pair, 0xD83D first)
        // sorts before U+FF5E, where a sort by code point would put it after.
        let v: Value =
            serde_json::from_str("{\"\u{ff5e}\":1,\"😀\":2,\"a\":3,\"\\r\":4,\"1\":5}").unwrap();
        assert_eq!(
            canon(&v),
            "{\"\\r\":4,\"1\":5,\"a\":3,\"😀\":2,\"\u{ff5e}\":1}"
        );
        let v: Value = serde_json::from_str(
            "[1e30, 4.5, 2e-3, 0.000001, 1e-7, 100, -0.0, 1.5e300, 9007199254740993]",
        )
        .unwrap();
        assert_eq!(
            canon(&v),
            "[1e+30,4.5,0.002,0.000001,1e-7,100,0,1.5e+300,9007199254740992]"
        );
        assert_eq!(
            canon(&json!("a\u{7f}\u{1}\"\\/\u{8}")),
            "\"a\u{7f}\\u0001\\\"\\\\/\\b\""
        );
        assert_eq!(
            canon(&json!({"b": [true, null, {"y": 1, "x": 2}], "a": "é"})),
            "{\"a\":\"é\",\"b\":[true,null,{\"x\":2,\"y\":1}]}"
        );
    }

    #[test]
    fn the_payload_drops_signatures_and_defaults_but_keeps_what_is_required() {
        let payload = String::from_utf8(canonical_payload(&card()).unwrap()).unwrap();
        // Required empty fields stay, explicitly set optional booleans stay, empty optional
        // arrays go.
        assert!(payload.contains("\"description\":\"\""), "{payload}");
        assert!(payload.contains("\"streaming\":false"), "{payload}");
        assert!(payload.contains("\"pushNotifications\":false"), "{payload}");
        assert!(payload.contains("\"tags\":[]"), "{payload}");
        assert!(!payload.contains("extensions"), "{payload}");
        assert!(!payload.contains("examples"), "{payload}");
        assert!(!payload.contains("signatures"), "{payload}");
        assert!(!payload.contains("null"), "{payload}");
        // Members are sorted.
        assert!(payload.starts_with("{\"capabilities\":"), "{payload}");
        // Signing does not change what is signed.
        let key = generate_signing_key_pem(false);
        let signed = CardSigner::from_pem(&key, None)
            .unwrap()
            .sign_card(card())
            .unwrap();
        assert_eq!(
            canonical_payload(&signed).unwrap(),
            canonical_payload(&card()).unwrap()
        );
    }

    #[test]
    fn what_proto3_json_drops_does_not_change_the_payload_and_what_matters_stays() {
        use a2a::{AgentExtension, SecurityScheme};
        let mut with = card();
        with.capabilities.extensions = Some(vec![AgentExtension {
            uri: "https://example.com/x/v1".into(),
            description: None,
            required: Some(false),
            params: Some([("k".to_owned(), json!({"empty": {}, "list": []}))].into()),
        }]);
        let mut without = with.clone();
        without.capabilities.extensions.as_mut().unwrap()[0].required = None;
        assert_eq!(
            canonical_payload(&with).unwrap(),
            canonical_payload(&without).unwrap(),
            "`required: false` is a default"
        );
        let mut required = with.clone();
        required.capabilities.extensions.as_mut().unwrap()[0].required = Some(true);
        assert_ne!(
            canonical_payload(&required).unwrap(),
            canonical_payload(&with).unwrap()
        );
        let payload = String::from_utf8(canonical_payload(&with).unwrap()).unwrap();
        assert!(
            payload.contains(r#""params":{"k":{"empty":{},"list":[]}}"#),
            "{payload}"
        );

        // The scheme a requirement names is signed, even with no scopes.
        let mut secured = card();
        secured.security_schemes = Some(
            [(
                "bearer".to_owned(),
                SecurityScheme::HttpAuth(a2a::HttpAuthSecurityScheme {
                    scheme: "Bearer".into(),
                    description: None,
                    bearer_format: None,
                }),
            )]
            .into(),
        );
        secured.security_requirements = Some(vec![[("bearer".to_owned(), vec![])].into()]);
        let mut renamed = secured.clone();
        renamed.security_requirements = Some(vec![[("basic".to_owned(), vec![])].into()]);
        assert_ne!(
            canonical_payload(&secured).unwrap(),
            canonical_payload(&renamed).unwrap()
        );
        assert!(
            String::from_utf8(canonical_payload(&secured).unwrap())
                .unwrap()
                .contains("bearer")
        );
    }

    #[test]
    fn a_signed_card_verifies_and_an_altered_one_does_not() {
        for ed25519 in [false, true] {
            let signer = CardSigner::from_pem(&generate_signing_key_pem(ed25519), None).unwrap();
            let signed = signer.sign_card(card()).unwrap();
            let key = signer.verifying_key();
            assert_eq!(key.verify_card(&signed), Ok(()), "ed25519={ed25519}");
            assert_eq!(key.verify_card(&card()), Err(VerifyError::Unsigned));

            let mut altered = signed.clone();
            altered.name = "Evil Agent".into();
            assert_eq!(key.verify_card(&altered), Err(VerifyError::Invalid));
            let mut altered = signed.clone();
            altered.capabilities.push_notifications = Some(true);
            assert_eq!(key.verify_card(&altered), Err(VerifyError::Invalid));
            let mut altered = signed.clone();
            altered.skills[0].id = "other".into();
            assert_eq!(key.verify_card(&altered), Err(VerifyError::Invalid));

            // Another key does not verify it, and the header says what it was signed with.
            let other = CardSigner::from_pem(&generate_signing_key_pem(ed25519), None).unwrap();
            assert_eq!(
                other.verifying_key().verify_card(&signed),
                Err(VerifyError::Invalid)
            );
            let protected = URL_SAFE_NO_PAD
                .decode(&signed.signatures.as_ref().unwrap()[0].protected)
                .unwrap();
            let header: Value = serde_json::from_slice(&protected).unwrap();
            assert_eq!(header["alg"], if ed25519 { "EdDSA" } else { "ES256" });
            assert_eq!(header["typ"], "JOSE");
            assert_eq!(header["kid"], signer.kid());
            assert!(header.get("jku").is_none());
        }
    }

    #[test]
    fn the_key_set_verifies_and_the_kid_is_the_thumbprint_unless_configured() {
        for ed25519 in [false, true] {
            let pem = generate_signing_key_pem(ed25519);
            let signer = CardSigner::from_pem(&pem, None).unwrap();
            let signed = signer.sign_card(card()).unwrap();
            // The public key set is enough to verify: what a client fetches from the `jku`.
            let jwks = signer.jwks();
            let key = VerifyingKey::from_jwk(&jwks, None).unwrap();
            assert_eq!(key.verify_card(&signed), Ok(()));
            assert_eq!(
                VerifyingKey::from_jwk(&jwks, Some(signer.kid()))
                    .unwrap()
                    .verify_card(&signed),
                Ok(())
            );
            assert!(VerifyingKey::from_jwk(&jwks, Some("nope")).is_err());
            assert_eq!(jwks["keys"][0]["use"], "sig");
            assert!(
                jwks["keys"][0].get("d").is_none(),
                "no private member in the key set"
            );
            // The same key gives the same kid; a configured one wins.
            assert_eq!(
                CardSigner::from_pem(&pem, None).unwrap().kid(),
                signer.kid()
            );
            let named = CardSigner::from_pem(&pem, Some("key-1"))
                .unwrap()
                .with_jku("https://example.com/jwks.json")
                .unwrap();
            assert_eq!(named.kid(), "key-1");
            let signed = named.sign_card(card()).unwrap();
            let protected = URL_SAFE_NO_PAD
                .decode(&signed.signatures.as_ref().unwrap()[0].protected)
                .unwrap();
            let header: Value = serde_json::from_slice(&protected).unwrap();
            assert_eq!(header["jku"], "https://example.com/jwks.json");
            let named_key = VerifyingKey::from_jwk(&named.jwks(), None).unwrap();
            assert_eq!(named_key.verify_card(&signed), Ok(()));
            assert_eq!(
                key.verify_card(&signed),
                Err(VerifyError::Invalid),
                "the key id is checked"
            );
        }
    }

    #[test]
    fn a_key_that_is_not_usable_is_refused_and_a_key_mismatch_in_alg_is_not_verified() {
        assert!(matches!(
            CardSigner::from_pem("not a pem", None),
            Err(SigningError::NotPkcs8)
        ));
        assert!(matches!(
            CardSigner::from_pem(
                "-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----\n",
                None
            ),
            Err(SigningError::NotPkcs8)
        ));
        assert!(matches!(
            CardSigner::from_pem(
                "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n",
                None
            ),
            Err(SigningError::UnsupportedKey)
        ));
        let pem = generate_signing_key_pem(false);
        assert!(matches!(
            CardSigner::from_pem(&pem, Some("")),
            Err(SigningError::BadIdentifier)
        ));
        assert!(matches!(
            CardSigner::from_pem(&pem, Some("a\nb")),
            Err(SigningError::BadIdentifier)
        ));
        // An ES256 signature is not accepted by an EdDSA key, and `none` is not an algorithm.
        let es = CardSigner::from_pem(&pem, None).unwrap();
        let ed = CardSigner::from_pem(&generate_signing_key_pem(true), None).unwrap();
        let signed = es.sign_card(card()).unwrap();
        assert_eq!(
            ed.verifying_key().verify_card(&signed),
            Err(VerifyError::Invalid)
        );
        let mut none = signed;
        none.signatures.as_mut().unwrap()[0].protected =
            URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        assert_eq!(
            es.verifying_key().verify_card(&none),
            Err(VerifyError::Invalid)
        );
        let debug = format!("{es:?}");
        assert!(
            !debug.contains("PRIVATE") && debug.contains("ES256"),
            "{debug}"
        );
    }
}
