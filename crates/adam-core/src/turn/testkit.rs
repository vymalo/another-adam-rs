//! The cases every [`TurnKeys`] passes. Use [`turn_keys_conformance!`](crate::turn_keys_conformance)
//! in an integration test (it needs `tokio` with `macros` and `rt` among the test's dependencies):
//!
//! ```ignore
//! async fn make_keys() -> adam_core::turn::DynTurnKeys {
//!     std::sync::Arc::new(MyKeychainKeys::open().await.unwrap())
//! }
//! adam_core::turn_keys_conformance!(make_keys);
//! ```
//!
//! A case panics on a failure: it is test code.
//!
//! [`Certified`] is a key named by a certificate chain (`KeyIdentity::Certificate`), over an
//! [`Ed25519TurnKeys`]: what a PKI adapter looks like to a signer, for the tests of code that
//! writes `x5c`.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use super::{
    CertificateChain, ChainHash, ConversationHash, CurrentKey, Ed25519TurnKeys, KeyId, KeyIdentity,
    KeySet, KeyStatus, Signature, SignatureType, TurnKeyError, TurnKeys, TurnPosition,
    TurnSignatureError, set_turn_signature, sign_detached, sign_turn, verify_detached, verify_turn,
};
use crate::RunId;

/// A key named by a certificate chain: [`current`](TurnKeys::current) answers
/// `KeyIdentity::Certificate(chain)`, so every signature's header carries `x5c`, and the key set
/// names the key by the chain's `kid` (the leaf's `x5t#S256` unless the chain names another). The
/// chain is carried, not checked: it is test material.
#[derive(Debug)]
pub struct Certified {
    inner: Ed25519TurnKeys,
    chain: CertificateChain,
    status: KeyStatus,
}

impl Certified {
    /// `inner`'s key, named by `chain`, [`KeyStatus::Active`].
    pub fn new(inner: Ed25519TurnKeys, chain: CertificateChain) -> Self {
        Self {
            inner,
            chain,
            status: KeyStatus::Active,
        }
    }

    /// The same, [`KeyStatus::Renewing`].
    #[must_use]
    pub fn renewing(mut self) -> Self {
        self.status = KeyStatus::Renewing;
        self
    }
}

#[async_trait]
impl TurnKeys for Certified {
    async fn current(&self) -> Result<CurrentKey, TurnKeyError> {
        Ok(CurrentKey {
            identity: KeyIdentity::Certificate(self.chain.clone()),
            status: self.status,
        })
    }

    async fn sign(&self, kid: &KeyId, input: &[u8]) -> Result<Signature, TurnKeyError> {
        if kid != self.chain.kid() {
            return Err(TurnKeyError::NotSigning { kid: kid.clone() });
        }
        self.inner.sign(self.inner.kid(), input).await
    }

    async fn key_set(&self) -> Result<Arc<KeySet>, TurnKeyError> {
        let mut set = KeySet::new();
        set.insert(self.chain.kid().clone(), self.inner.public_key())
            .map_err(|e| TurnKeyError::Unavailable(Box::new(e)))?;
        Ok(Arc::new(set))
    }

    fn kind(&self) -> &'static str {
        "certificate"
    }
}

/// The names of the cases, in the order [`turn_keys_conformance!`](crate::turn_keys_conformance)
/// generates them.
pub const CASES: [&str; 5] = [
    "a_signature_verifies_with_the_key_set",
    "a_signature_covers_its_payload_and_its_type",
    "the_current_key_is_in_the_key_set",
    "a_key_it_does_not_hold_does_not_sign",
    "the_key_set_holds_no_private_key",
];

/// A signature by the current key verifies with the key set, and names the current key; a
/// header that names a certificate chain still verifies by its `kid`.
#[allow(
    clippy::unwrap_used,
    reason = "test code: a failed unwrap is a failed case"
)]
pub async fn a_signature_verifies_with_the_key_set(keys: &dyn TurnKeys) {
    let current = keys.current().await.unwrap();
    let set = keys.key_set().await.unwrap();
    let jws = sign_detached(keys, SignatureType::Step, b"payload")
        .await
        .unwrap();
    let kid = verify_detached(&set, SignatureType::Step, b"payload", &jws).unwrap();
    assert_eq!(&kid, current.kid());

    let run = RunId::new();
    let cnv = ConversationHash::of(Some("testkit"), run);
    let message = json!({"role": "assistant", "content": [{"type": "text", "text": "ok"}]});
    let position = TurnPosition {
        run,
        cnv,
        idx: 1,
        prv: ChainHash::over(run, &cnv, [&json!({"role": "user", "content": "hi"})]),
    };
    let mut signed = message.clone();
    assert!(set_turn_signature(
        &mut signed,
        sign_turn(keys, &position, &message).await.unwrap()
    ));
    assert_eq!(verify_turn(&set, &position, &signed).unwrap(), kid);
}

/// The signature does not verify other bytes, or as another type.
#[allow(
    clippy::unwrap_used,
    reason = "test code: a failed unwrap is a failed case"
)]
pub async fn a_signature_covers_its_payload_and_its_type(keys: &dyn TurnKeys) {
    let set = keys.key_set().await.unwrap();
    let jws = sign_detached(keys, SignatureType::Head, b"payload")
        .await
        .unwrap();
    assert!(matches!(
        verify_detached(&set, SignatureType::Head, b"payloaD", &jws),
        Err(TurnSignatureError::Invalid { .. })
    ));
    assert_eq!(
        verify_detached(&set, SignatureType::Turn, b"payload", &jws),
        Err(TurnSignatureError::WrongType)
    );
}

/// The current key's id is in the key set and not revoked.
#[allow(
    clippy::unwrap_used,
    reason = "test code: a failed unwrap is a failed case"
)]
pub async fn the_current_key_is_in_the_key_set(keys: &dyn TurnKeys) {
    let current = keys.current().await.unwrap();
    let set = keys.key_set().await.unwrap();
    assert!(
        set.key(current.kid()).is_some(),
        "the current key `{}` is not in the key set, or is revoked",
        current.kid()
    );
}

/// Asked to sign with a key it does not hold, it answers `NotSigning`, never a signature.
#[allow(
    clippy::unwrap_used,
    reason = "test code: a failed unwrap is a failed case"
)]
pub async fn a_key_it_does_not_hold_does_not_sign(keys: &dyn TurnKeys) {
    let other = KeyId::new("adam-testkit:no-such-key").unwrap();
    assert!(matches!(
        keys.sign(&other, b"input").await,
        Err(TurnKeyError::NotSigning { kid }) if kid == other
    ));
}

/// The key set, written as a JWKS, carries no private member; `Debug` of the keys is its kind.
#[allow(
    clippy::unwrap_used,
    reason = "test code: a failed unwrap is a failed case"
)]
pub async fn the_key_set_holds_no_private_key(keys: &dyn TurnKeys) {
    let jwks = keys.key_set().await.unwrap().to_jwks();
    for key in jwks["keys"].as_array().unwrap() {
        assert!(key.get("d").is_none(), "a private member in the key set");
    }
    assert_eq!(format!("{keys:?}"), format!("TurnKeys({})", keys.kind()));
}

/// Generate one `#[tokio::test]` per case of [`turn::testkit`](crate::turn::testkit). `$make` is a
/// path to an `async fn() -> DynTurnKeys`.
#[macro_export]
macro_rules! turn_keys_conformance {
    ($make:path) => {
        $crate::turn_keys_conformance!(@cases $make;
            a_signature_verifies_with_the_key_set,
            a_signature_covers_its_payload_and_its_type,
            the_current_key_is_in_the_key_set,
            a_key_it_does_not_hold_does_not_sign,
            the_key_set_holds_no_private_key,
        );
    };
    (@cases $make:path; $($case:ident),* $(,)?) => {
        $(
            #[tokio::test]
            async fn $case() {
                let keys = $make().await;
                $crate::turn::testkit::$case(&*keys).await;
            }
        )*
    };
}
