//! The request signature (`adam-request/v1`, ADR 0068 amended 2026-11-25) against the shared
//! prompt-signature contract's vector V7 (`tests/fixtures/request-signature/`, test material only):
//! adam's signer gives the exact strings of V7a (a key named by a certificate chain, `x5c`) and
//! V7b (a key named by `kid`), each verifies, and a request that differs in its body, method,
//! path, run or prompt fails, as the contract's cases TN1 to TN4, TN9 and TN14 refuse them.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use serde_json::Value;

use adam_core::turn::testkit::Certified;
use adam_core::turn::{
    CertificateChain, Ed25519TurnKeys, KeyId, KeySet, ModelCall, REQUEST_MAX_AGE_SECS,
    REQUEST_MAX_SKEW_SECS, SignatureType, TurnKeys, TurnSignatureError, sign_detached,
    sign_request, sign_request_at, verify_attached, verify_detached, verify_request,
};
use secrecy::SecretString;

fn vectors() -> Value {
    serde_json::from_str(include_str!(
        "fixtures/request-signature/v7-turn-signature.json"
    ))
    .unwrap()
}

fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}

/// The PKCS#8 v1 PEM of the Ed25519 seed `seed` (hex).
fn pem(seed: &str) -> SecretString {
    let mut der = hex("302e020100300506032b657004220420");
    der.extend(hex(seed));
    SecretString::from(format!(
        "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
        STANDARD.encode(der)
    ))
}

/// What one vector says: the request, the payload's `prompt_jti`, `iat` and nonce, the string.
struct Vector {
    method: String,
    path: String,
    body: String,
    run: String,
    prompt_jti: String,
    iat: i64,
    nonce: [u8; 16],
    header: String,
    signature: String,
}

impl Vector {
    fn read(name: &str) -> Self {
        let all = vectors();
        let v = &all["vectors"][name];
        let payload: Value = serde_json::from_str(v["payload_jcs"].as_str().unwrap()).unwrap();
        let s = |value: &Value| value.as_str().unwrap().to_owned();
        Self {
            method: s(&v["request"]["method"]),
            path: s(&v["request"]["path"]),
            body: s(&v["request"]["body"]),
            run: s(&payload["run"]),
            prompt_jti: s(&payload["prompt_jti"]),
            iat: payload["iat"].as_i64().unwrap(),
            nonce: URL_SAFE_NO_PAD
                .decode(payload["nonce"].as_str().unwrap())
                .unwrap()
                .try_into()
                .unwrap(),
            header: s(&v["protected_header_jcs"]),
            signature: s(&v["turn_signature"]),
        }
    }

    fn call(&self) -> ModelCall<'_> {
        ModelCall {
            run: &self.run,
            method: &self.method,
            path: &self.path,
            body: self.body.as_bytes(),
            prompt_jti: Some(&self.prompt_jti),
        }
    }
}

/// V7a's key: the adam leaf's seed, named by the chain the header's `x5c` carries.
fn v7a_keys(vector: &Vector) -> Certified {
    let all = vectors();
    let header: Value = serde_json::from_str(&vector.header).unwrap();
    let chain = header["x5c"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| STANDARD.decode(c.as_str().unwrap()).unwrap())
        .collect();
    let chain = CertificateChain::new(chain).unwrap();
    assert_eq!(
        chain.kid().as_str(),
        all["keys"]["V7a"]["x5t_s256"].as_str().unwrap(),
        "the chain's kid is its leaf's x5t#S256"
    );
    let inner = Ed25519TurnKeys::from_pkcs8_pem(
        &pem(all["keys"]["V7a"]["seed_hex"].as_str().unwrap()),
        Some(chain.kid().clone()),
        None,
    )
    .unwrap();
    Certified::new(inner, chain)
}

/// V7b's key: the device turn key's seed, named by its registered `kid`.
fn v7b_keys() -> Ed25519TurnKeys {
    let all = vectors();
    Ed25519TurnKeys::from_pkcs8_pem(
        &pem(all["keys"]["V7b"]["seed_hex"].as_str().unwrap()),
        Some(KeyId::new(all["keys"]["V7b"]["kid"].as_str().unwrap()).unwrap()),
        None,
    )
    .unwrap()
}

#[tokio::test]
async fn v7a_a_key_named_by_a_certificate_chain_gives_the_exact_string_with_x5c() {
    let vector = Vector::read("V7a");
    let keys = v7a_keys(&vector);
    let jws = sign_request_at(&keys, &vector.call(), vector.iat, vector.nonce)
        .await
        .unwrap();
    assert_eq!(jws, vector.signature, "the exact bytes of V7a");
    let header = URL_SAFE_NO_PAD
        .decode(jws.split('.').next().unwrap())
        .unwrap();
    assert_eq!(String::from_utf8(header).unwrap(), vector.header);
    let set = keys.key_set().await.unwrap();
    let verified = verify_request(&set, &jws, &vector.call(), vector.iat).unwrap();
    assert_eq!(&verified.kid, keys.current().await.unwrap().kid());
    assert_eq!(verified.iat, vector.iat);
}

#[tokio::test]
async fn v7b_a_key_named_by_its_kid_gives_the_exact_string_without_x5c() {
    let vector = Vector::read("V7b");
    let keys = v7b_keys();
    let jws = sign_request_at(&keys, &vector.call(), vector.iat, vector.nonce)
        .await
        .unwrap();
    assert_eq!(jws, vector.signature, "the exact bytes of V7b");
    assert!(!vector.header.contains("x5c"));
    let set = keys.key_set().await.unwrap();
    verify_request(
        &set,
        &jws,
        &vector.call(),
        vector.iat + REQUEST_MAX_AGE_SECS,
    )
    .unwrap();
    verify_request(
        &set,
        &jws,
        &vector.call(),
        vector.iat - REQUEST_MAX_SKEW_SECS,
    )
    .unwrap();
    // The contract's public key of the registered device key is the one the seed makes.
    let all = vectors();
    let x = URL_SAFE_NO_PAD
        .decode(all["keys"]["V7b"]["jwk"]["x"].as_str().unwrap())
        .unwrap();
    assert_eq!(keys.public_key().to_bytes().to_vec(), x);
}

/// A verifier that holds the key and the vector's own string, given a request that differs in one
/// thing, refuses it: TN1 (the body), TN2 (the method), TN3 (the path), TN4 (`prompt_jti`), TN14
/// (another run), TN9 (a byte of the signature), TN5 (an `iat` 301 s old).
#[test]
fn a_request_that_differs_from_the_one_signed_fails() {
    for name in ["V7a", "V7b"] {
        let vector = Vector::read(name);
        let set = if name == "V7a" {
            let keys = v7a_keys(&vector);
            futures_block(keys.key_set())
        } else {
            futures_block(v7b_keys().key_set())
        };
        let jws = &vector.signature;
        let now = vector.iat;
        let call = vector.call();

        let mut body = vector.body.clone().into_bytes();
        body[10] ^= 1;
        let cases: [(&str, ModelCall<'_>, &str); 5] = [
            (
                "TN1",
                ModelCall {
                    body: &body,
                    ..call
                },
                "body_sha256",
            ),
            (
                "TN2",
                ModelCall {
                    method: "PUT",
                    ..call
                },
                "method",
            ),
            (
                "TN3",
                ModelCall {
                    path: "/v1/completions",
                    ..call
                },
                "path",
            ),
            (
                "TN4",
                ModelCall {
                    prompt_jti: Some("AgICAgICAgICAgICAgICAg"),
                    ..call
                },
                "prompt_jti",
            ),
            (
                "TN14",
                ModelCall {
                    run: "run-test-0002",
                    ..call
                },
                "run",
            ),
        ];
        for (case, changed, member) in cases {
            assert_eq!(
                verify_request(&set, jws, &changed, now),
                Err(TurnSignatureError::Mismatch { member }),
                "{name} {case}"
            );
        }
        assert_eq!(
            verify_request(
                &set,
                jws,
                &ModelCall {
                    prompt_jti: None,
                    ..call
                },
                now
            ),
            Err(TurnSignatureError::Mismatch {
                member: "prompt_jti"
            }),
            "{name}: a signature with a prompt is not one without"
        );

        let mut tampered = jws.clone().into_bytes();
        let last = tampered.len() - 5;
        tampered[last] = if tampered[last] == b'A' { b'B' } else { b'A' };
        let tampered = String::from_utf8(tampered).unwrap();
        assert!(
            matches!(
                verify_request(&set, &tampered, &call, now),
                Err(TurnSignatureError::Invalid { .. })
            ),
            "{name} TN9"
        );
        assert_eq!(
            verify_request(&set, jws, &call, now + REQUEST_MAX_AGE_SECS + 1),
            Err(TurnSignatureError::Expired),
            "{name} TN5"
        );
        assert_eq!(
            verify_request(&set, jws, &call, now - REQUEST_MAX_SKEW_SECS - 1),
            Err(TurnSignatureError::Expired),
            "{name}: an iat ahead of the clock"
        );
        // A key the set does not hold.
        assert!(matches!(
            verify_request(&KeySet::new(), jws, &call, now),
            Err(TurnSignatureError::UnknownKey { .. })
        ));
    }
}

fn futures_block<T>(
    f: impl std::future::Future<Output = Result<T, adam_core::turn::TurnKeyError>>,
) -> T {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(f)
        .unwrap()
}

/// A request signature never verifies as a history signature, nor one the other way round; the
/// attached form refuses a detached string and the detached form an attached one.
#[tokio::test]
async fn a_request_signature_is_its_own_type_and_form() {
    let keys = v7b_keys();
    let set = keys.key_set().await.unwrap();
    let vector = Vector::read("V7b");
    assert_eq!(
        verify_attached(&set, SignatureType::Turn, &vector.signature),
        Err(TurnSignatureError::WrongType)
    );
    let detached = sign_detached(&keys, SignatureType::Request, b"x")
        .await
        .unwrap();
    assert_eq!(
        verify_attached(&set, SignatureType::Request, &detached),
        Err(TurnSignatureError::Malformed)
    );
    assert_eq!(
        verify_detached(&set, SignatureType::Request, b"x", &vector.signature),
        Err(TurnSignatureError::Malformed)
    );
    // A payload segment written with padding is not adam's form.
    let mut parts: Vec<String> = vector.signature.split('.').map(str::to_owned).collect();
    parts[1].push('=');
    assert_eq!(
        verify_attached(&set, SignatureType::Request, &parts.join(".")),
        Err(TurnSignatureError::Malformed)
    );
}

/// With no prompt the payload's `prompt_jti` is `null`; each signature has its own nonce and the
/// time of signing, and none of it shows in `Debug`.
#[tokio::test]
async fn a_request_without_a_prompt_signs_null_and_each_signature_is_new() {
    let keys = <dyn TurnKeys>::ephemeral().unwrap();
    let call = ModelCall {
        run: "0190f0c2-6a1b-7c3d-8e4f-0123456789ab",
        method: "POST",
        path: "/v1/chat/completions?x=1",
        body: b"{\"secret prompt\":1}",
        prompt_jti: None,
    };
    let one = sign_request(&*keys, &call).await.unwrap();
    let two = sign_request(&*keys, &call).await.unwrap();
    assert_ne!(one, two, "a new nonce each time");
    let payload: Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(one.split('.').nth(1).unwrap())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(payload["prompt_jti"], Value::Null);
    assert_eq!(payload["path"], "/v1/chat/completions?x=1");
    let now = jiff::Timestamp::now().as_second();
    let set = keys.key_set().await.unwrap();
    let a = verify_request(&set, &one, &call, now).unwrap();
    let b = verify_request(&set, &two, &call, now).unwrap();
    assert_ne!(a.nonce, b.nonce);
    let shown = format!("{call:?}");
    assert!(!shown.contains("secret prompt"), "{shown}");
}
