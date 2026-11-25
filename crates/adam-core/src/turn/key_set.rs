//! The public keys a turn signature is verified with, read from and written as a JWKS.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use adam_error::{Classify, ErrorClass};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::VerifyingKey;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use super::{KeyId, Signature};

/// An Ed25519 public key. It verifies; nothing can sign with it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PublicKey(VerifyingKey);

impl PublicKey {
    /// The public key `bytes` (the JWK's `x`).
    ///
    /// # Errors
    ///
    /// [`KeySetError::BadKey`] when the bytes are not a point of the curve, or are a weak key (of
    /// small order: it would verify forged signatures).
    pub fn from_bytes(bytes: &[u8; 32]) -> Result<Self, KeySetError> {
        let key = VerifyingKey::from_bytes(bytes).map_err(|_| KeySetError::BadKey { index: 0 })?;
        if key.is_weak() {
            return Err(KeySetError::BadKey { index: 0 });
        }
        Ok(Self(key))
    }

    pub(super) fn from_verifying(key: VerifyingKey) -> Self {
        Self(key)
    }

    /// The key's 32 bytes.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.0.to_bytes()
    }

    /// The RFC 7638 thumbprint of the key's JWK: the default `kid`.
    pub fn thumbprint(&self) -> String {
        // RFC 7638 §3.2: the required members, in lexicographic order, no whitespace.
        let text = format!(
            r#"{{"crv":"Ed25519","kty":"OKP","x":"{}"}}"#,
            URL_SAFE_NO_PAD.encode(self.to_bytes())
        );
        URL_SAFE_NO_PAD.encode(Sha256::digest(text.as_bytes()))
    }

    /// Whether `signature` is this key's over `input` (RFC 8032, strict: a non-canonical
    /// signature is refused).
    pub(super) fn verifies(&self, input: &[u8], signature: &Signature) -> bool {
        let signature = ed25519_dalek::Signature::from_bytes(&signature.to_bytes());
        self.0.verify_strict(input, &signature).is_ok()
    }

    fn jwk(&self, kid: &KeyId) -> Value {
        json!({
            "kty": "OKP",
            "crv": "Ed25519",
            "x": URL_SAFE_NO_PAD.encode(self.to_bytes()),
            "kid": kid.as_str(),
            "alg": "EdDSA",
            "use": "sig",
        })
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey({})", URL_SAFE_NO_PAD.encode(self.to_bytes()))
    }
}

/// The public keys a turn signature is verified with, by `kid`, and the ids no longer accepted
/// ([ADR 0068] decision 8): a key that leaked is named under `revoked`, and every signature it made
/// is refused, whether its public key is still listed or not.
///
/// It holds public keys only: a JWKS that carries a private member (`d`) is refused.
///
/// The JWKS form ([`from_jwks`](Self::from_jwks), [`to_jwks`](Self::to_jwks)) is RFC 7517 with
/// one member more, `revoked`, a list of key ids:
///
/// ```json
/// {"keys": [{"kty": "OKP", "crv": "Ed25519", "x": "…", "kid": "vault:turn#v2"}],
///  "revoked": ["vault:turn#v1"]}
/// ```
///
/// [ADR 0068]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0068-adam-signs-its-own-history.md
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeySet {
    keys: BTreeMap<KeyId, PublicKey>,
    revoked: BTreeSet<KeyId>,
}

impl KeySet {
    /// No key: verifies nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// The set with `key` under `kid`.
    ///
    /// # Errors
    ///
    /// [`KeySetError::Conflict`] when the set holds another key under `kid`.
    pub fn with_key(mut self, kid: KeyId, key: PublicKey) -> Result<Self, KeySetError> {
        self.insert(kid, key)?;
        Ok(self)
    }

    /// Add `key` under `kid`; adding the same key again changes nothing.
    ///
    /// # Errors
    ///
    /// [`KeySetError::Conflict`] when the set holds another key under `kid`.
    pub fn insert(&mut self, kid: KeyId, key: PublicKey) -> Result<(), KeySetError> {
        match self.keys.get(&kid) {
            Some(held) if *held != key => Err(KeySetError::Conflict { kid }),
            Some(_) => Ok(()),
            None => {
                self.keys.insert(kid, key);
                Ok(())
            }
        }
    }

    /// Refuse every signature of `kid` from now on.
    pub fn revoke(&mut self, kid: KeyId) {
        self.revoked.insert(kid);
    }

    /// Whether `kid` is revoked.
    pub fn is_revoked(&self, kid: &KeyId) -> bool {
        self.revoked.contains(kid)
    }

    /// The key `kid` names, unless it is revoked.
    pub fn key(&self, kid: &KeyId) -> Option<&PublicKey> {
        if self.is_revoked(kid) {
            None
        } else {
            self.keys.get(kid)
        }
    }

    /// The ids of the keys it holds (revoked ones included), in order.
    pub fn ids(&self) -> impl Iterator<Item = &KeyId> {
        self.keys.keys()
    }

    /// The revoked ids, in order.
    pub fn revoked(&self) -> impl Iterator<Item = &KeyId> {
        self.revoked.iter()
    }

    /// Whether it holds no key.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// The set a JWKS describes (see the type's docs). Every key is `kty` `OKP`, `crv` `Ed25519`,
    /// with a `kid`; an `alg` must be `EdDSA`, a `use` must be `sig`; other members are ignored, a
    /// private one is refused.
    ///
    /// # Errors
    ///
    /// [`KeySetError`], naming the position of the key at fault, never its content.
    pub fn from_jwks(jwks: &Value) -> Result<Self, KeySetError> {
        let keys = jwks
            .get("keys")
            .and_then(Value::as_array)
            .ok_or(KeySetError::NotAKeySet)?;
        let mut set = Self::new();
        for (index, jwk) in keys.iter().enumerate() {
            let jwk = jwk.as_object().ok_or(KeySetError::BadKey { index })?;
            let (kid, key) = read_jwk(jwk, index)?;
            set.insert(kid, key)?;
        }
        match jwks.get("revoked") {
            None => {}
            Some(Value::Array(ids)) => {
                for (index, id) in ids.iter().enumerate() {
                    let kid = id
                        .as_str()
                        .and_then(|id| KeyId::new(id).ok())
                        .ok_or(KeySetError::BadRevoked { index })?;
                    set.revoke(kid);
                }
            }
            Some(_) => return Err(KeySetError::NotAKeySet),
        }
        Ok(set)
    }

    /// As [`from_jwks`](Self::from_jwks), from the JSON text `bytes`.
    ///
    /// # Errors
    ///
    /// [`KeySetError::NotJson`], or as `from_jwks`.
    pub fn from_jwks_slice(bytes: &[u8]) -> Result<Self, KeySetError> {
        let value: Value = serde_json::from_slice(bytes).map_err(|_| KeySetError::NotJson)?;
        Self::from_jwks(&value)
    }

    /// The set as a JWKS: every key with its `kid`, `alg` `EdDSA` and `use` `sig`, and `revoked`
    /// when an id is.
    pub fn to_jwks(&self) -> Value {
        let keys: Vec<Value> = self.keys.iter().map(|(kid, key)| key.jwk(kid)).collect();
        let mut out = Map::new();
        out.insert("keys".into(), Value::Array(keys));
        if !self.revoked.is_empty() {
            out.insert(
                "revoked".into(),
                self.revoked.iter().map(|k| json!(k.as_str())).collect(),
            );
        }
        Value::Object(out)
    }
}

fn read_jwk(jwk: &Map<String, Value>, index: usize) -> Result<(KeyId, PublicKey), KeySetError> {
    if jwk.contains_key("d") {
        return Err(KeySetError::PrivateMember { index });
    }
    let text = |name: &str| jwk.get(name).and_then(Value::as_str);
    if text("kty") != Some("OKP") || text("crv") != Some("Ed25519") {
        return Err(KeySetError::NotEd25519 { index });
    }
    if jwk.get("alg").is_some_and(|alg| alg != "EdDSA")
        || jwk.get("use").is_some_and(|u| u != "sig")
    {
        return Err(KeySetError::NotEd25519 { index });
    }
    let kid = text("kid")
        .and_then(|kid| KeyId::new(kid).ok())
        .ok_or(KeySetError::NoKid { index })?;
    let x = text("x")
        .and_then(|x| URL_SAFE_NO_PAD.decode(x).ok())
        .and_then(|x| <[u8; 32]>::try_from(x).ok())
        .ok_or(KeySetError::BadKey { index })?;
    let key = PublicKey::from_bytes(&x).map_err(|_| KeySetError::BadKey { index })?;
    Ok((kid, key))
}

/// Why a key set cannot be read or built. Names a key by its position or its id, never by its
/// bytes.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum KeySetError {
    /// The text is not JSON.
    #[error("the turn key set is not JSON")]
    NotJson,
    /// It is not an object with a `keys` array (and, if any, a `revoked` array).
    #[error("the turn key set is not a JWKS: an object with a `keys` array")]
    NotAKeySet,
    /// The key at `index` is not an Ed25519 key for signatures.
    #[error(
        "key {index} of the turn key set is not an Ed25519 signature key (`kty` OKP, `crv` Ed25519)"
    )]
    NotEd25519 {
        /// Its position in `keys`.
        index: usize,
    },
    /// The key at `index` has no usable `kid`.
    #[error("key {index} of the turn key set has no `kid` of printable ASCII")]
    NoKid {
        /// Its position in `keys`.
        index: usize,
    },
    /// The key at `index` carries a private member: a verify set holds public keys only.
    #[error(
        "key {index} of the turn key set holds a private key (`d`): a verify set holds public keys only"
    )]
    PrivateMember {
        /// Its position in `keys`.
        index: usize,
    },
    /// The key at `index` is not a usable Ed25519 public key.
    #[error("key {index} of the turn key set is not a usable Ed25519 public key")]
    BadKey {
        /// Its position in `keys`.
        index: usize,
    },
    /// Two different keys under one `kid`.
    #[error("the turn key set holds two different keys under `{kid}`")]
    Conflict {
        /// The id named twice.
        kid: KeyId,
    },
    /// The entry at `index` of `revoked` is not a key id.
    #[error("entry {index} of `revoked` in the turn key set is not a key id")]
    BadRevoked {
        /// Its position in `revoked`.
        index: usize,
    },
}

impl Classify for KeySetError {
    fn class(&self) -> ErrorClass {
        ErrorClass::Invalid
    }
}
