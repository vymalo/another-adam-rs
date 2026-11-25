//! The decisive tests of ADR 0068 slice T1: the turn signature's form, payload and chain, the key
//! set, and the keys.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers: a failed unwrap is a failed test"
)]

use std::sync::Arc;

use adam_core::RunId;
use adam_core::turn::{
    CertificateChain, ChainHash, ConversationHash, CurrentKey, DynTurnKeys, Ed25519TurnKeys, KeyId,
    KeyIdentity, KeySet, KeySetError, KeyStatus, MAX_SIGNATURE_LEN, PublicKey, Signature,
    SignatureType, TurnKeyConfigError, TurnKeyError, TurnKeys, TurnPosition, TurnSignatureError,
    set_turn_signature, sign_detached, sign_turn, turn_payload, turn_signature, verify_detached,
    verify_turn,
};
use adam_error::{Classify, ErrorClass};
use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use secrecy::SecretString;
use serde_json::{Value, json};
use uuid::Uuid;

/// RFC 8032 §7.1, test 1: the secret key, its public key, and the signature of the empty message.
const RFC8032_SECRET: &str = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";
const RFC8032_PUBLIC: &str = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";
const RFC8032_EMPTY_SIG: &str = "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b";

/// A second fixed key (RFC 8032 §7.1, test 2).
const RFC8032_SECRET_2: &str = "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb";

fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}

/// The PKCS#8 v1 PEM of the seed `secret` (hex), as `openssl genpkey -algorithm ed25519` writes it.
fn pem(secret: &str) -> SecretString {
    let mut der = hex("302e020100300506032b657004220420");
    der.extend(hex(secret));
    SecretString::from(format!(
        "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
        STANDARD.encode(der)
    ))
}

fn kid(text: &str) -> KeyId {
    KeyId::new(text).unwrap()
}

fn keys(secret: &str, id: &str, verify: Option<KeySet>) -> Ed25519TurnKeys {
    Ed25519TurnKeys::from_pkcs8_pem(&pem(secret), Some(kid(id)), verify).unwrap()
}

fn run() -> RunId {
    RunId(Uuid::parse_str("0190f0c2-6a1b-7c3d-8e4f-0123456789ab").unwrap())
}

fn history() -> Vec<Value> {
    vec![
        json!({"role": "system", "content": "You are adam."}),
        json!({"role": "user", "content": "Bonjour, ça va ? 😀"}),
    ]
}

fn assistant() -> Value {
    json!({
        "role": "assistant",
        "content": [{"type": "text", "text": "I ran the tests: 12 passed."}],
        "tool_calls": [{"id": "c1", "name": "run", "arguments": {"cmd": "cargo test", "n": 1.5}}],
        "provider": {"model": "gw", "endpoint": "openai:0011",
                     "members": {"extra_content": {"google": {"thought_signature": "opaque"}}}},
    })
}

fn position() -> TurnPosition {
    let run = run();
    let cnv = ConversationHash::of(Some("ctx-1"), run);
    TurnPosition {
        run,
        cnv,
        idx: 2,
        prv: ChainHash::over(run, &cnv, &history()),
    }
}

/// The golden vector: Ed25519 is deterministic, so a fixed key and a fixed message give a fixed
/// signature. The values below were made outside this crate (2026-11-25): `cnv`, the chain and the
/// payload by Python's `hashlib` and `json.dumps(sort_keys=True, separators=(",", ":"),
/// ensure_ascii=False)` (RFC 8785 for these values), and the signature by OpenSSL 3.0.13,
/// `openssl pkeyutl -sign -rawin` with the PEM `pem(RFC8032_SECRET)` over the signing input.
#[tokio::test]
async fn golden_vector() {
    // The primitive: RFC 8032's own vector, through the port.
    let k = keys(RFC8032_SECRET, "vault:turn-key#v1", None);
    assert_eq!(hex(RFC8032_PUBLIC), k.public_key().to_bytes());
    // RFC 8037 appendix A.3: the RFC 7638 thumbprint of this very key, the default `kid`.
    assert_eq!(
        k.public_key().thumbprint(),
        "kPrK_qmxVWaYVA9wwBF6Iuo3vVzz7TxHCTwXBygrS4k"
    );
    let empty = k.sign(&kid("vault:turn-key#v1"), b"").await.unwrap();
    assert_eq!(empty.to_bytes().to_vec(), hex(RFC8032_EMPTY_SIG));

    let p = position();
    assert_eq!(
        p.cnv.to_string(),
        "ttRMvr5ey69pn6UHwsJo13SN6hROid4Hp2fxjLaGF20",
        "cnv: base64url SHA-256 of the conversation id"
    );
    assert_eq!(
        ChainHash::seed(p.run, &p.cnv).to_string(),
        GOLDEN_SEED,
        "h(-1)"
    );
    assert_eq!(p.prv.to_string(), GOLDEN_PRV, "h(1)");
    let payload = turn_payload(&p, &assistant());
    assert_eq!(String::from_utf8(payload).unwrap(), GOLDEN_PAYLOAD);
    let jws = sign_turn(&k, &p, &assistant()).await.unwrap();
    assert_eq!(jws, GOLDEN_JWS);
    assert_eq!(jws.len(), 171, "the member's value with a 17-character kid");
}

const GOLDEN_SEED: &str = "adWzymFrXDilRmOzix6RLn0jYZZLgaGCPEfeumS6kfA";
const GOLDEN_PRV: &str = "XBH2PlyX2icAEAC1dcXd0IAvHxVLEcv_LxNy1kKejTA";
const GOLDEN_PAYLOAD: &str = r#"{"cnv":"ttRMvr5ey69pn6UHwsJo13SN6hROid4Hp2fxjLaGF20","idx":2,"msg":{"content":[{"text":"I ran the tests: 12 passed.","type":"text"}],"provider":{"endpoint":"openai:0011","members":{"extra_content":{"google":{"thought_signature":"opaque"}}},"model":"gw"},"role":"assistant","tool_calls":[{"arguments":{"cmd":"cargo test","n":1.5},"id":"c1","name":"run"}]},"prv":"XBH2PlyX2icAEAC1dcXd0IAvHxVLEcv_LxNy1kKejTA","run":"0190f0c2-6a1b-7c3d-8e4f-0123456789ab","typ":"adam-turn/v1"}"#;
const GOLDEN_JWS: &str = "eyJhbGciOiJFZERTQSIsImtpZCI6InZhdWx0OnR1cm4ta2V5I3YxIiwidHlwIjoiYWRhbS10dXJuL3YxIn0..WMt8J41GOAYebVzKium2zW5jxqRxziWLjGA-M0VkKXKgpCTjwOT3onoIlGBoXzdJ-RsEkoX-5gFsqwxecacwDg";

fn signed(keys_: &Ed25519TurnKeys, p: &TurnPosition, message: &Value) -> Value {
    let jws = futures_block(sign_turn(keys_, p, message)).unwrap();
    let mut message = message.clone();
    assert!(set_turn_signature(&mut message, jws));
    message
}

fn futures_block<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(f)
}

/// The signature rides in the message, and does not sign itself: adding it changes no payload.
#[test]
fn the_signature_rides_in_extra_content_adam_and_is_not_signed() {
    let k = keys(RFC8032_SECRET, "vault:turn-key#v1", None);
    let p = position();
    let message = signed(&k, &p, &assistant());
    assert!(turn_signature(&message).is_some());
    assert_eq!(turn_payload(&p, &message), turn_payload(&p, &assistant()));
    assert_eq!(
        message["provider"]["members"]["extra_content"]["google"]["thought_signature"],
        "opaque"
    );
    assert_eq!(
        verify_turn(k.keys(), &p, &message).unwrap(),
        kid("vault:turn-key#v1")
    );
    // An `extra_content` a provider left empty, and one whose only member is adam's, sign alike.
    let mut empty = assistant();
    empty["extra_content"] = json!({});
    assert_eq!(turn_payload(&p, &empty), turn_payload(&p, &assistant()));
    // Another maker's member at the message's top level is signed.
    let mut other = message.clone();
    other["extra_content"]["google"] = json!({"x": 1});
    assert!(matches!(
        verify_turn(k.keys(), &p, &other),
        Err(TurnSignatureError::Invalid { .. })
    ));
}

/// Each field of the payload, changed, fails: the run, the conversation, the index, the chain
/// before it, the message (its text, a provider's member, a tool call's argument, a number), and
/// the type.
#[test]
fn each_payload_field_changed_fails() {
    let k = keys(RFC8032_SECRET, "vault:turn-key#v1", None);
    let p = position();
    let message = signed(&k, &p, &assistant());
    assert!(verify_turn(k.keys(), &p, &message).is_ok());

    let other_run = RunId(Uuid::parse_str("0190f0c2-6a1b-7c3d-8e4f-0123456789ac").unwrap());
    let positions = [
        TurnPosition {
            run: other_run,
            ..p
        },
        TurnPosition {
            cnv: ConversationHash::of(Some("ctx-2"), p.run),
            ..p
        },
        TurnPosition {
            cnv: ConversationHash::of(None, p.run),
            ..p
        },
        TurnPosition { idx: 3, ..p },
        TurnPosition { idx: 1, ..p },
        TurnPosition {
            prv: ChainHash::over(p.run, &p.cnv, &history()[..1]),
            ..p
        },
        TurnPosition {
            prv: ChainHash::over(p.run, &p.cnv, &[history()[1].clone(), history()[0].clone()]),
            ..p
        },
        TurnPosition {
            prv: ChainHash::over(
                p.run,
                &ConversationHash::of(Some("ctx-2"), p.run),
                &history(),
            ),
            ..p
        },
    ];
    for (i, changed) in positions.iter().enumerate() {
        assert!(
            matches!(
                verify_turn(k.keys(), changed, &message),
                Err(TurnSignatureError::Invalid { .. })
            ),
            "position {i} verified"
        );
    }
    let edits: [Edit; 5] = [
        ("text", |m| {
            m["content"][0]["text"] = json!("I ran the tests: 13 passed.")
        }),
        ("provider member", |m| {
            m["provider"]["members"]["extra_content"]["google"]["thought_signature"] =
                json!("other")
        }),
        ("argument", |m| {
            m["tool_calls"][0]["arguments"]["cmd"] = json!("rm -rf /")
        }),
        ("number", |m| {
            m["tool_calls"][0]["arguments"]["n"] = json!(1.25)
        }),
        ("role", |m| m["role"] = json!("user")),
    ];
    for (what, edit) in edits {
        let mut changed = message.clone();
        edit(&mut changed);
        assert!(
            matches!(
                verify_turn(k.keys(), &p, &changed),
                Err(TurnSignatureError::Invalid { .. })
            ),
            "{what} changed and verified"
        );
    }
    // The type: a turn's signature is not a head's.
    let jws = turn_signature(&message).unwrap();
    assert_eq!(
        verify_detached(
            k.keys(),
            SignatureType::Head,
            &turn_payload(&p, &assistant()),
            jws
        ),
        Err(TurnSignatureError::WrongType)
    );
    // A message with no signature is unsigned.
    assert_eq!(
        verify_turn(k.keys(), &p, &assistant()),
        Err(TurnSignatureError::Unsigned)
    );
}

type Edit = (&'static str, fn(&mut Value));

/// The chain: every earlier message, a person's included and with its own signature member,
/// changes every later link.
#[test]
fn the_chain_covers_every_earlier_message() {
    let p = position();
    let base = ChainHash::over(p.run, &p.cnv, &history());
    let mut edited = history();
    edited[0]["content"] = json!("You are someone else.");
    assert_ne!(ChainHash::over(p.run, &p.cnv, &edited), base);
    let mut member = history();
    member[1]["extra_content"] = json!({"adam": {"turn_signature": "x"}});
    assert_ne!(ChainHash::over(p.run, &p.cnv, &member), base);
    assert_eq!(
        ChainHash::seed(p.run, &p.cnv)
            .next(&history()[0])
            .next(&history()[1]),
        base
    );
}

/// A `kid` outside the set fails, whatever the signature.
#[tokio::test]
async fn a_kid_outside_the_set_fails() {
    let signer = keys(RFC8032_SECRET, "vault:turn-key#v1", None);
    let verifier = keys(RFC8032_SECRET_2, "vault:turn-key#v2", None);
    let jws = sign_detached(&signer, SignatureType::Turn, b"p")
        .await
        .unwrap();
    assert_eq!(
        verify_detached(verifier.keys(), SignatureType::Turn, b"p", &jws),
        Err(TurnSignatureError::UnknownKey {
            kid: kid("vault:turn-key#v1")
        })
    );
    // The right key under another name is still another kid.
    let renamed = keys(RFC8032_SECRET, "vault:turn-key#v9", None);
    assert!(matches!(
        verify_detached(renamed.keys(), SignatureType::Turn, b"p", &jws),
        Err(TurnSignatureError::UnknownKey { .. })
    ));
}

/// A rotation: version 2 signs, version 1's public key stays in the set, and both versions'
/// signatures verify, during the overlap (a worker still on v1) and after it (v1's private key gone).
#[tokio::test]
async fn both_versions_of_an_overlap_pass() {
    let v1 = keys(RFC8032_SECRET, "vault:turn-key#v1", None);
    let old = sign_detached(&v1, SignatureType::Turn, b"p").await.unwrap();
    let set = KeySet::new()
        .with_key(kid("vault:turn-key#v1"), v1.public_key())
        .unwrap();
    // The overlap: the new worker signs with v2 and verifies with both; the old one still signs v1.
    let v2 = keys(RFC8032_SECRET_2, "vault:turn-key#v2", Some(set.clone()));
    let v1_late = keys(
        RFC8032_SECRET,
        "vault:turn-key#v1",
        Some(
            KeySet::new()
                .with_key(kid("vault:turn-key#v2"), v2.public_key())
                .unwrap(),
        ),
    );
    let new = sign_detached(&v2, SignatureType::Turn, b"p").await.unwrap();
    let late = sign_detached(&v1_late, SignatureType::Turn, b"p")
        .await
        .unwrap();
    for verifier in [v2.keys(), v1_late.keys()] {
        for jws in [&old, &new, &late] {
            assert!(verify_detached(verifier, SignatureType::Turn, b"p", jws).is_ok());
        }
    }
    // After it: the set as a JWKS, as the verify file holds it, with v1 public only.
    let jwks = v2.keys().to_jwks();
    let after = KeySet::from_jwks(&jwks).unwrap();
    assert_eq!(
        verify_detached(&after, SignatureType::Turn, b"p", &old).unwrap(),
        kid("vault:turn-key#v1")
    );
    // And the server's key keeps v1 for verifying while it signs only with v2.
    assert!(matches!(
        v2.sign(&kid("vault:turn-key#v1"), b"p").await,
        Err(TurnKeyError::NotSigning { .. })
    ));
}

/// A public-only key verifies and cannot sign: a key set read from a JWKS verifies, a JWKS with a
/// private member is refused, a public PEM is not a signing key, and a source asked for a key whose
/// public half alone it holds does not sign.
#[tokio::test]
async fn a_public_only_key_verifies_and_cannot_sign() {
    let k = keys(RFC8032_SECRET, "vault:turn-key#v1", None);
    let jws = sign_detached(&k, SignatureType::Step, b"p").await.unwrap();
    let public_only = KeySet::from_jwks(&k.keys().to_jwks()).unwrap();
    assert!(verify_detached(&public_only, SignatureType::Step, b"p", &jws).is_ok());

    let mut private = k.keys().to_jwks();
    private["keys"][0]["d"] = json!(URL_SAFE_NO_PAD.encode(hex(RFC8032_SECRET)));
    assert_eq!(
        KeySet::from_jwks(&private),
        Err(KeySetError::PrivateMember { index: 0 })
    );

    let public_pem = SecretString::from(
        "-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEA11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=\n-----END PUBLIC KEY-----\n",
    );
    assert!(matches!(
        Ed25519TurnKeys::from_pkcs8_pem(&public_pem, None, None),
        Err(TurnKeyConfigError::NotPrivate)
    ));

    let other = keys(RFC8032_SECRET_2, "vault:turn-key#v2", Some(public_only));
    assert!(other.keys().key(&kid("vault:turn-key#v1")).is_some());
    assert!(matches!(
        other.sign(&kid("vault:turn-key#v1"), b"p").await,
        Err(TurnKeyError::NotSigning { .. })
    ));
}

/// A revoked key fails wherever it signed, its public key listed or not, and cannot be the signing
/// key; revocation survives the JWKS.
#[tokio::test]
async fn a_revoked_key_fails() {
    let v1 = keys(RFC8032_SECRET, "vault:turn-key#v1", None);
    let jws = sign_detached(&v1, SignatureType::Turn, b"p").await.unwrap();
    let mut set = v1.keys().clone();
    set.revoke(kid("vault:turn-key#v1"));
    assert_eq!(
        verify_detached(&set, SignatureType::Turn, b"p", &jws),
        Err(TurnSignatureError::Revoked {
            kid: kid("vault:turn-key#v1")
        })
    );
    let jwks = set.to_jwks();
    assert_eq!(jwks["revoked"], json!(["vault:turn-key#v1"]));
    let reread = KeySet::from_jwks(&jwks).unwrap();
    assert!(matches!(
        verify_detached(&reread, SignatureType::Turn, b"p", &jws),
        Err(TurnSignatureError::Revoked { .. })
    ));
    // Removed and named under `revoked`: the same.
    let only_revoked =
        KeySet::from_jwks(&json!({"keys": [], "revoked": ["vault:turn-key#v1"]})).unwrap();
    assert!(matches!(
        verify_detached(&only_revoked, SignatureType::Turn, b"p", &jws),
        Err(TurnSignatureError::Revoked { .. })
    ));
    assert!(matches!(
        Ed25519TurnKeys::from_pkcs8_pem(
            &pem(RFC8032_SECRET),
            Some(kid("vault:turn-key#v1")),
            Some(only_revoked)
        ),
        Err(TurnKeyConfigError::SigningKeyRevoked { .. })
    ));
}

/// No `Debug` and no error holds a key: the keys, a key set, and every error of a bad PEM or JWKS
/// show neither the seed (hex, base64, bytes) nor the PEM's body.
#[tokio::test]
async fn no_debug_or_error_holds_a_key() {
    let seed = hex(RFC8032_SECRET);
    let pem = pem(RFC8032_SECRET);
    let body = {
        use secrecy::ExposeSecret as _;
        pem.expose_secret().lines().nth(1).unwrap().to_owned()
    };
    let forbidden = [
        RFC8032_SECRET.to_owned(),
        STANDARD.encode(&seed),
        URL_SAFE_NO_PAD.encode(&seed),
        format!("{:?}", &seed[..4])
            .trim_matches(['[', ']'])
            .to_owned(),
        body.clone(),
        body[..24].to_owned(),
    ];
    let check = |what: &str, text: String| {
        for f in &forbidden {
            assert!(
                !text.contains(f.as_str()),
                "{what} holds key material: {text}"
            );
        }
    };
    let k = Ed25519TurnKeys::from_pkcs8_pem(&pem, None, None).unwrap();
    check("Debug of the keys", format!("{k:?}"));
    let dyn_keys: DynTurnKeys = Arc::new(k);
    check("Debug of dyn TurnKeys", format!("{dyn_keys:?}"));
    check(
        "Debug of the key set",
        format!("{:?}", dyn_keys.key_set().await.unwrap()),
    );
    check(
        "Debug of the current key",
        format!("{:?}", dyn_keys.current().await.unwrap()),
    );

    // Damaged PEMs: a mismatched v2 public key, a cut body, a wrong algorithm, a wrong label.
    let mut v2 = hex("3053020101300506032b657004220420");
    v2.extend(&seed);
    v2.extend(hex("a123032100"));
    v2.extend([7_u8; 32]);
    let mut wrong_alg = hex("302e020100300506032b656e04220420");
    wrong_alg.extend(&seed);
    let wrap = |label: &str, der: &[u8]| {
        SecretString::from(format!(
            "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
            STANDARD.encode(der)
        ))
    };
    let mut cut = hex("302e020100300506032b657004220420");
    cut.extend(&seed[..31]);
    for bad in [
        wrap("PRIVATE KEY", &v2),
        wrap("PRIVATE KEY", &wrong_alg),
        wrap("PRIVATE KEY", &cut),
        wrap("EC PRIVATE KEY", &wrong_alg),
        SecretString::from(format!("-----BEGIN PRIVATE KEY-----\n{body}\n")),
        SecretString::from(body.clone()),
    ] {
        let err = Ed25519TurnKeys::from_pkcs8_pem(&bad, None, None).unwrap_err();
        assert_eq!(err.class(), ErrorClass::Invalid);
        check("an error", format!("{err} / {err:?}"));
    }
    let mut private = KeySet::new()
        .with_key(
            kid("k"),
            PublicKey::from_bytes(&hex(RFC8032_PUBLIC).try_into().unwrap()).unwrap(),
        )
        .unwrap()
        .to_jwks();
    private["keys"][0]["d"] = json!(URL_SAFE_NO_PAD.encode(&seed));
    let err = KeySet::from_jwks(&private).unwrap_err();
    check("a key set error", format!("{err} / {err:?}"));
}

/// The size bound: the usual member is small, the largest header allowed fits, and a longer
/// value is refused before it is decoded.
#[tokio::test]
async fn the_size_bound() {
    let k = keys(RFC8032_SECRET, "vault:turn-keys#v12", None);
    let jws = sign_detached(&k, SignatureType::Turn, b"p").await.unwrap();
    // A 19-character kid: the ADR's estimate was about 220 bytes for the whole member.
    let member = json!({"extra_content": {"adam": {"turn_signature": jws}}}).to_string();
    assert_eq!("vault:turn-keys#v12".len(), 19);
    assert_eq!(jws.len(), 174);
    assert_eq!(member.len(), 222);

    // The largest identity: a kid of the longest length and a full chain of the largest certificates.
    let chain = CertificateChain::new(vec![
        vec![0x30; CertificateChain::MAX_CERTIFICATE_LEN];
        CertificateChain::MAX_CERTIFICATES
    ])
    .unwrap()
    .with_kid(kid(&"k".repeat(KeyId::MAX_LEN)));
    let big = WithChain {
        inner: keys(RFC8032_SECRET, &"k".repeat(KeyId::MAX_LEN), None),
        chain,
        status: KeyStatus::Active,
    };
    let jws = sign_detached(&big, SignatureType::Turn, b"p")
        .await
        .unwrap();
    assert!(jws.len() <= MAX_SIGNATURE_LEN, "{}", jws.len());
    assert!(verify_detached(big.inner.keys(), SignatureType::Turn, b"p", &jws).is_ok());

    let long = format!("{}..{}", "A".repeat(MAX_SIGNATURE_LEN), "A".repeat(86));
    assert_eq!(
        verify_detached(k.keys(), SignatureType::Turn, b"p", &long),
        Err(TurnSignatureError::TooLong)
    );
}

/// Only the form adam writes verifies: not an attached payload, a header written another way, an
/// `alg` other than EdDSA, a member more, or a signature of another length.
#[tokio::test]
async fn only_the_form_adam_writes_verifies() {
    let k = keys(RFC8032_SECRET, "vault:turn-key#v1", None);
    let jws = sign_detached(&k, SignatureType::Turn, b"p").await.unwrap();
    let (protected, signature) = jws.split_once("..").unwrap();
    let header = |h: &str| URL_SAFE_NO_PAD.encode(h);
    let forms = [
        format!("{protected}.{}.{signature}", URL_SAFE_NO_PAD.encode(b"p")),
        format!(
            "{}..{signature}",
            header(r#"{"kid":"vault:turn-key#v1","alg":"EdDSA","typ":"adam-turn/v1"}"#)
        ),
        format!(
            "{}..{signature}",
            header(r#"{"alg": "EdDSA","kid":"vault:turn-key#v1","typ":"adam-turn/v1"}"#)
        ),
        format!(
            "{}..{signature}",
            header(r#"{"alg":"none","kid":"vault:turn-key#v1","typ":"adam-turn/v1"}"#)
        ),
        format!(
            "{}..{signature}",
            header(
                r#"{"alg":"EdDSA","crit":["b64"],"kid":"vault:turn-key#v1","typ":"adam-turn/v1"}"#
            )
        ),
        format!("{protected}..{}", &signature[..signature.len() - 2]),
        format!("{protected}.."),
        format!("{protected}..{signature}."),
        "not a jws".to_owned(),
    ];
    for (i, form) in forms.iter().enumerate() {
        assert_eq!(
            verify_detached(k.keys(), SignatureType::Turn, b"p", form),
            Err(TurnSignatureError::Malformed),
            "form {i}"
        );
    }
    let err = TurnSignatureError::Malformed;
    assert_eq!(err.class(), ErrorClass::Corrupt);
}

/// A key a PKI issues fits the port as it is: named by a certificate chain, renewing, signing with
/// a key held elsewhere. The header carries the chain (`x5c`), and the key set still verifies it
/// by its `kid`. The port's conformance suite passes it.
#[derive(Debug)]
struct WithChain {
    inner: Ed25519TurnKeys,
    chain: CertificateChain,
    status: KeyStatus,
}

#[async_trait]
impl TurnKeys for WithChain {
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

async fn with_chain() -> DynTurnKeys {
    let chain =
        CertificateChain::new(vec![b"leaf certificate".to_vec(), b"issuer".to_vec()]).unwrap();
    Arc::new(WithChain {
        inner: keys(RFC8032_SECRET, "inner", None),
        chain,
        status: KeyStatus::Renewing,
    })
}

#[tokio::test]
async fn a_certificate_chain_rides_in_the_header() {
    let keys_ = with_chain().await;
    let current = keys_.current().await.unwrap();
    assert_eq!(current.status, KeyStatus::Renewing);
    let jws = sign_detached(&*keys_, SignatureType::Turn, b"p")
        .await
        .unwrap();
    let header: Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(jws.split('.').next().unwrap())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        header["x5c"],
        json!([
            STANDARD.encode("leaf certificate"),
            STANDARD.encode("issuer")
        ])
    );
    assert_eq!(header["kid"], current.kid().as_str());
    let set = keys_.key_set().await.unwrap();
    assert_eq!(
        &verify_detached(&set, SignatureType::Turn, b"p", &jws).unwrap(),
        current.kid()
    );
}

mod certificate_conformance {
    adam_core::turn_keys_conformance!(super::with_chain);
}

async fn ephemeral() -> DynTurnKeys {
    <dyn TurnKeys>::ephemeral().unwrap()
}

mod ephemeral_conformance {
    adam_core::turn_keys_conformance!(super::ephemeral);
}

async fn from_pem() -> DynTurnKeys {
    Arc::new(keys(RFC8032_SECRET, "vault:turn-key#v1", None))
}

mod pem_conformance {
    adam_core::turn_keys_conformance!(super::from_pem);
}

/// Two ephemeral keys are two keys: neither verifies the other's signature.
#[tokio::test]
async fn an_ephemeral_key_is_its_own() {
    let a = ephemeral().await;
    let b = ephemeral().await;
    assert_ne!(
        a.current().await.unwrap().kid(),
        b.current().await.unwrap().kid()
    );
    let jws = sign_detached(&*a, SignatureType::Turn, b"p").await.unwrap();
    assert!(matches!(
        verify_detached(&b.key_set().await.unwrap(), SignatureType::Turn, b"p", &jws),
        Err(TurnSignatureError::UnknownKey { .. })
    ));
    assert_eq!(a.kind(), "ephemeral");
}

/// The files a server mounts: the PEM and the verify JWKS, with the default `kid` the key's
/// thumbprint (RFC 7638, the same as the card's signer gives).
#[tokio::test]
async fn the_files_a_server_mounts() {
    let dir = std::env::temp_dir().join(format!("adam-turn-keys-{}", Uuid::new_v4()));
    tokio::fs::create_dir_all(&dir).await.unwrap();
    let key = dir.join("turn.pem");
    let verify = dir.join("verify.json");
    {
        use secrecy::ExposeSecret as _;
        tokio::fs::write(&key, pem(RFC8032_SECRET).expose_secret())
            .await
            .unwrap();
    }
    let older = keys(RFC8032_SECRET_2, "vault:turn-key#v1", None);
    tokio::fs::write(&verify, older.keys().to_jwks().to_string())
        .await
        .unwrap();
    let k =
        Ed25519TurnKeys::from_files(&key, Some(kid("vault:turn-key#v2")), Some(&verify)).unwrap();
    assert_eq!(k.kind(), "file");
    let ids: Vec<_> = k.keys().ids().map(KeyId::as_str).collect();
    assert_eq!(ids, ["vault:turn-key#v1", "vault:turn-key#v2"]);
    let default = Ed25519TurnKeys::from_files(&key, None, None).unwrap();
    assert_eq!(
        default.kid().as_str(),
        default.public_key().thumbprint().as_str()
    );
    assert!(matches!(
        Ed25519TurnKeys::from_files(&dir.join("missing.pem"), None, None),
        Err(TurnKeyConfigError::Unreadable {
            which: "signing key",
            ..
        })
    ));
    // The signing key under an id the verify set gives another key: refused.
    assert!(matches!(
        Ed25519TurnKeys::from_files(&key, Some(kid("vault:turn-key#v1")), Some(&verify)),
        Err(TurnKeyConfigError::KeySet(KeySetError::Conflict { .. }))
    ));
    tokio::fs::remove_dir_all(&dir).await.unwrap();
}

/// A JWKS that is not one of Ed25519 signature keys is refused, naming the key by position.
#[test]
fn a_bad_key_set_is_refused() {
    let x = URL_SAFE_NO_PAD.encode(hex(RFC8032_PUBLIC));
    let cases = [
        (json!({}), KeySetError::NotAKeySet),
        (json!({"keys": {}}), KeySetError::NotAKeySet),
        (json!({"keys": [], "revoked": "k"}), KeySetError::NotAKeySet),
        (
            json!({"keys": [{"kty": "EC", "crv": "P-256", "x": x, "kid": "k"}]}),
            KeySetError::NotEd25519 { index: 0 },
        ),
        (
            json!({"keys": [{"kty": "OKP", "crv": "Ed25519", "x": x, "kid": "k", "alg": "ES256"}]}),
            KeySetError::NotEd25519 { index: 0 },
        ),
        (
            json!({"keys": [{"kty": "OKP", "crv": "Ed25519", "x": x, "kid": "k", "use": "enc"}]}),
            KeySetError::NotEd25519 { index: 0 },
        ),
        (
            json!({"keys": [{"kty": "OKP", "crv": "Ed25519", "x": x}]}),
            KeySetError::NoKid { index: 0 },
        ),
        (
            json!({"keys": [{"kty": "OKP", "crv": "Ed25519", "x": "AAAA", "kid": "k"}]}),
            KeySetError::BadKey { index: 0 },
        ),
        // The identity point: a weak key, of small order.
        (
            json!({"keys": [{"kty": "OKP", "crv": "Ed25519", "kid": "k",
                          "x": URL_SAFE_NO_PAD.encode([1_u8].into_iter().chain([0; 31]).collect::<Vec<_>>())}]}),
            KeySetError::BadKey { index: 0 },
        ),
        (
            json!({"keys": [], "revoked": [""]}),
            KeySetError::BadRevoked { index: 0 },
        ),
        (
            json!({"keys": [{"kty": "OKP", "crv": "Ed25519", "x": x, "kid": "k"},
                         {"kty": "OKP", "crv": "Ed25519", "kid": "k",
                          "x": URL_SAFE_NO_PAD.encode(keys(RFC8032_SECRET_2, "k", None).public_key().to_bytes())}]}),
            KeySetError::Conflict { kid: kid("k") },
        ),
    ];
    for (jwks, want) in cases {
        assert_eq!(KeySet::from_jwks(&jwks), Err(want), "{jwks}");
    }
    assert_eq!(KeySet::from_jwks_slice(b"{"), Err(KeySetError::NotJson));
    // Members a JWKS may carry are ignored.
    let ok = json!({"keys": [{"kty": "OKP", "crv": "Ed25519", "x": x, "kid": "k", "alg": "EdDSA",
                              "use": "sig", "key_ops": ["verify"]}]});
    assert!(KeySet::from_jwks(&ok).is_ok());
}
