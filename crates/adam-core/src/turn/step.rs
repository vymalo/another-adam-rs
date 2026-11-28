//! A journal entry's signature ([ADR 0068] decision 2): `typ` `adam-step/v1` over the run, the
//! step's `seq`, its `name`, `ok` and `payload`.
//!
//! The form is the one every history signature uses ([`sign_detached`], [`verify_detached`]): a
//! compact JWS (RFC 7515 appendix F) whose payload is detached, its header
//! `{"alg":"EdDSA","kid":"…","typ":"adam-step/v1"}`. Entries are **not** chained: each one is
//! checked on its own when it is replayed ([ADR 0068] decision 4).
//!
//! The payload is the RFC 8785 form of
//!
//! ```json
//! {"typ": "adam-step/v1", "run": "<run id>", "seq": 0, "name": "tool:x", "ok": true, "payload": {}}
//! ```
//!
//! `payload` is signed as the store keeps it: [`storable`]'s rule (a NUL becomes U+FFFD) is applied
//! here, so the bytes signed are the bytes a replay reads whatever wrote the entry.
//! `recorded_at` is not covered: a store may truncate it, and it says when the entry landed, not
//! what adam recorded.
//!
//! [ADR 0068]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0068-adam-signs-its-own-history.md

use serde_json::{Value, json};

use super::form::{SignatureType, TurnSignatureError, sign_detached, verify_detached};
use super::{KeyId, KeySet, TurnKeyError, TurnKeys};
use crate::RunId;
use crate::jcs::canonicalize;
use crate::store::{JournalEntry, storable};

/// The bytes a step signature covers: the RFC 8785 form of
/// `{typ, run, seq, name, ok, payload}`, with `payload` as [`storable`] keeps it.
pub fn step_payload(run: RunId, seq: u64, name: &str, ok: bool, payload: &Value) -> Vec<u8> {
    let payload = storable(payload);
    canonicalize(&json!({
        "typ": SignatureType::Step.typ(),
        "run": run.to_string(),
        "seq": seq,
        "name": name,
        "ok": ok,
        "payload": &*payload,
    }))
}

/// Sign `entry` of `run` with the current key of `keys`: the detached JWS of [`step_payload`].
///
/// # Errors
///
/// [`TurnKeyError`] from the keys.
pub async fn sign_step(
    keys: &(impl TurnKeys + ?Sized),
    run: RunId,
    entry: &JournalEntry,
) -> Result<String, TurnKeyError> {
    sign_detached(
        keys,
        SignatureType::Step,
        &step_payload(run, entry.seq, &entry.name, entry.ok, &entry.payload),
    )
    .await
}

/// Verify that `entry` of `run` carries the signature of its own content, with `set`; the id of the
/// key that made it.
///
/// # Errors
///
/// [`TurnSignatureError::Unsigned`] when it carries none, [`TurnSignatureError::Malformed`] when
/// its signature is empty, and the others of [`verify_detached`]. Every one is class `Corrupt`: the
/// entry was not written by adam, or was changed after it was (ADR 0068 decision 6).
pub fn verify_step(
    set: &KeySet,
    run: RunId,
    entry: &JournalEntry,
) -> Result<KeyId, TurnSignatureError> {
    let jws = entry
        .signature
        .as_deref()
        .ok_or(TurnSignatureError::Unsigned)?;
    verify_detached(
        set,
        SignatureType::Step,
        &step_payload(run, entry.seq, &entry.name, entry.ok, &entry.payload),
        jws,
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::turn::Ed25519TurnKeys;

    fn entry() -> JournalEntry {
        JournalEntry::ok(3, "tool:charge", json!({"receipt": "r-1", "n": 2}))
    }

    /// The payload is the documented members, canonicalized; every member is bound.
    #[test]
    fn the_payload_names_the_run_the_step_and_what_it_recorded() {
        let run = RunId::new();
        let bytes = step_payload(run, 3, "tool:charge", true, &json!({"receipt": "r-1"}));
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(
            text,
            format!(
                r#"{{"name":"tool:charge","ok":true,"payload":{{"receipt":"r-1"}},"run":"{run}","seq":3,"typ":"adam-step/v1"}}"#
            )
        );
        let base = step_payload(run, 3, "tool:charge", true, &json!({"receipt": "r-1"}));
        for other in [
            step_payload(
                RunId::new(),
                3,
                "tool:charge",
                true,
                &json!({"receipt": "r-1"}),
            ),
            step_payload(run, 4, "tool:charge", true, &json!({"receipt": "r-1"})),
            step_payload(run, 3, "tool:refund", true, &json!({"receipt": "r-1"})),
            step_payload(run, 3, "tool:charge", false, &json!({"receipt": "r-1"})),
            step_payload(run, 3, "tool:charge", true, &json!({"receipt": "r-2"})),
        ] {
            assert_ne!(other, base);
        }
    }

    /// A signature verifies with the key set, and an entry changed after it was signed does not:
    /// what a replay refuses (ADR 0068 decision 6).
    #[tokio::test]
    async fn a_signed_entry_verifies_and_an_altered_one_does_not() {
        let keys = Ed25519TurnKeys::ephemeral().unwrap();
        let set = keys.key_set().await.unwrap();
        let run = RunId::new();
        let mut signed = entry();
        signed.signature = Some(sign_step(&keys, run, &signed).await.unwrap());
        assert!(verify_step(&set, run, &signed).is_ok());

        // Another run, and every member of the payload, is bound.
        assert!(verify_step(&set, RunId::new(), &signed).is_err());
        for altered in [
            JournalEntry {
                seq: 4,
                ..signed.clone()
            },
            JournalEntry {
                name: "tool:refund".into(),
                ..signed.clone()
            },
            JournalEntry {
                ok: false,
                ..signed.clone()
            },
            JournalEntry {
                payload: json!({"receipt": "r-2", "n": 2}),
                ..signed.clone()
            },
        ] {
            assert!(
                matches!(
                    verify_step(&set, run, &altered),
                    Err(TurnSignatureError::Invalid { .. })
                ),
                "{altered:?}"
            );
        }

        // No signature at all is the caller's to adopt or refuse, not a verification failure.
        assert_eq!(
            verify_step(&set, run, &entry()),
            Err(TurnSignatureError::Unsigned)
        );
        // Another key's set does not verify it.
        let other = Ed25519TurnKeys::ephemeral().unwrap();
        assert!(verify_step(&other.key_set().await.unwrap(), run, &signed).is_err());
    }

    /// A payload with a NUL is signed as the store keeps it, so the round trip through any adapter
    /// verifies.
    #[tokio::test]
    async fn a_nul_is_signed_as_the_store_keeps_it() {
        let keys = Ed25519TurnKeys::ephemeral().unwrap();
        let run = RunId::new();
        let mut signed = JournalEntry::ok(0, "tool", json!({"out": "a\u{0}b"}));
        signed.signature = Some(sign_step(&keys, run, &signed).await.unwrap());
        let stored = JournalEntry {
            payload: crate::storable_owned(signed.payload.clone()),
            ..signed.clone()
        };
        assert_ne!(stored.payload, signed.payload);
        assert!(
            verify_step(&keys.key_set().await.unwrap(), run, &stored).is_ok(),
            "the store's replacement character is what was signed"
        );
    }
}
