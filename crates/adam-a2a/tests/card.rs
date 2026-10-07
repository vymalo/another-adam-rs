//! The extended agent card (authenticated callers only) and agent card signatures, through a real
//! listener and the official client.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod support;

use a2a::{AgentCard, GetExtendedAgentCardRequest, error_code};
use adam_a2a::{
    AuthConfig, CardSigner, ExtendedCardConfig, ExtensionConfig, SkillConfig, VerifyError,
    VerifyingKey, generate_signing_key_pem,
};
use base64::Engine as _;
use support::*;

async fn http_get(server: &TestServer, path: &str) -> (u16, String) {
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("{}{path}", server.base()))
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.text().await.unwrap())
}

async fn public_card(server: &TestServer) -> AgentCard {
    let (status, body) = http_get(server, "/.well-known/agent-card.json").await;
    assert_eq!(status, 200);
    serde_json::from_str(&body).unwrap()
}

fn extended() -> ExtendedCardConfig {
    ExtendedCardConfig::new()
        .with_description("The longer story, for those who signed in")
        .with_skill(SkillConfig::new(
            "audit",
            "Audit",
            "Only for authenticated callers",
        ))
        .with_extension(ExtensionConfig::new("https://example.com/ext/internal/v1"))
}

fn extended_request() -> GetExtendedAgentCardRequest {
    GetExtendedAgentCardRequest { tenant: None }
}

// -------------------------------------------------------------- extended card

#[tokio::test]
async fn without_an_extended_card_the_card_says_so_and_the_method_is_unsupported() {
    let server = TestServer::start(Setup::default()).await;
    assert_eq!(
        public_card(&server).await.capabilities.extended_agent_card,
        Some(false)
    );
    let client = server.client(Some(TOKEN)).await;
    let err = client
        .get_extended_agent_card(&extended_request())
        .await
        .unwrap_err();
    assert_eq!(err.code, error_code::UNSUPPORTED_OPERATION);
}

#[tokio::test]
async fn the_extended_card_needs_authentication_and_shows_the_extra_entries() {
    let server = TestServer::start(Setup {
        extended: Some(extended()),
        ..Setup::default()
    })
    .await;
    let public = public_card(&server).await;
    assert_eq!(public.capabilities.extended_agent_card, Some(true));
    assert_eq!(
        public
            .skills
            .iter()
            .map(|s| s.id.as_str())
            .collect::<Vec<_>>(),
        ["echo"]
    );
    assert_eq!(public.description, "Echoes messages");
    assert!(
        public.capabilities.extensions.is_none(),
        "the public card does not show it"
    );

    // Anonymous: refused before the method is reached.
    let (status, body) = {
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(server.base())
            .header("content-type", "application/json")
            .body(r#"{"jsonrpc":"2.0","id":1,"method":"GetExtendedAgentCard","params":{}}"#)
            .send()
            .await
            .unwrap();
        (response.status().as_u16(), response.text().await.unwrap())
    };
    assert_eq!(status, 401, "{body}");
    assert!(!body.contains("audit"), "{body}");

    let client = server.client(Some(TOKEN)).await;
    let card = client
        .get_extended_agent_card(&extended_request())
        .await
        .unwrap();
    assert_eq!(
        card.description,
        "The longer story, for those who signed in"
    );
    let ids: Vec<_> = card.skills.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(
        ids,
        ["echo", "audit"],
        "the public skills plus the extra ones"
    );
    let extensions = card.capabilities.extensions.unwrap();
    assert_eq!(extensions[0].uri, "https://example.com/ext/internal/v1");
    assert_eq!(card.capabilities.extended_agent_card, Some(true));
    assert_eq!(card.name, public.name);
    assert_eq!(card.supported_interfaces, public.supported_interfaces);
    // A wrong token is refused too.
    let bad = server.client(Some("wrong")).await;
    assert!(
        bad.get_extended_agent_card(&extended_request())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_server_that_does_not_authenticate_has_no_extended_card() {
    let server = TestServer::start(Setup {
        auth: AuthConfig::AllowAnonymous,
        extended: Some(extended()),
        ..Setup::default()
    })
    .await;
    assert_eq!(
        public_card(&server).await.capabilities.extended_agent_card,
        Some(false)
    );
    let client = server.client(None).await;
    let err = client
        .get_extended_agent_card(&extended_request())
        .await
        .unwrap_err();
    assert_eq!(err.code, error_code::UNSUPPORTED_OPERATION);
}

// ----------------------------------------------------------------- signatures

fn signer(ed25519: bool) -> CardSigner {
    CardSigner::from_pem(&generate_signing_key_pem(ed25519), Some("test-key")).unwrap()
}

#[tokio::test]
async fn an_unsigned_server_serves_unsigned_cards_and_no_key_set() {
    let server = TestServer::start(Setup::default()).await;
    assert!(public_card(&server).await.signatures.is_none());
    // The key-set route does not exist (and, like every route that does not, it is not public).
    let (status, _) = http_get(&server, "/.well-known/jwks.json").await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn the_signature_verifies_and_fails_when_the_card_is_altered() {
    for ed25519 in [false, true] {
        let signer = signer(ed25519);
        let server = TestServer::start(Setup {
            signer: Some(signer.clone()),
            ..Setup::default()
        })
        .await;
        let card = public_card(&server).await;
        let signatures = card.signatures.clone().expect("the card is signed");
        assert_eq!(signatures.len(), 1);

        // A client fetches the key set from the server, as `jku` would say, and verifies.
        let (status, jwks) = http_get(&server, "/.well-known/jwks.json").await;
        assert_eq!(status, 200, "the key set is public");
        let jwks: serde_json::Value = serde_json::from_str(&jwks).unwrap();
        assert_eq!(jwks["keys"][0]["kid"], "test-key");
        assert!(jwks["keys"][0].get("d").is_none());
        let key = VerifyingKey::from_jwk(&jwks, Some("test-key")).unwrap();
        assert_eq!(key.verify_card(&card), Ok(()), "ed25519={ed25519}");
        // The header says how it was signed.
        let header: serde_json::Value = serde_json::from_slice(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(&signatures[0].protected)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(header["alg"], if ed25519 { "EdDSA" } else { "ES256" });
        assert_eq!(header["typ"], "JOSE");
        assert_eq!(header["kid"], "test-key");

        // Altering anything the card says breaks it: a skill, the URL, a capability, the name.
        let mut altered = card.clone();
        altered.skills[0].description = "Does what an attacker wants".into();
        assert_eq!(key.verify_card(&altered), Err(VerifyError::Invalid));
        let mut altered = card.clone();
        altered.supported_interfaces[0].url = "https://evil.example.com/".into();
        assert_eq!(key.verify_card(&altered), Err(VerifyError::Invalid));
        let mut altered = card.clone();
        altered.capabilities.push_notifications = Some(true);
        assert_eq!(key.verify_card(&altered), Err(VerifyError::Invalid));
        let mut altered = card.clone();
        altered.name = "someone else".into();
        assert_eq!(key.verify_card(&altered), Err(VerifyError::Invalid));
        // Stripping the signature is not "valid" either.
        let mut stripped = card.clone();
        stripped.signatures = None;
        assert_eq!(key.verify_card(&stripped), Err(VerifyError::Unsigned));
        // Another key does not verify it.
        assert_eq!(
            signer_other(ed25519).verify_card(&card),
            Err(VerifyError::Invalid)
        );
    }
}

fn signer_other(ed25519: bool) -> VerifyingKey {
    signer(ed25519).verifying_key()
}

#[tokio::test]
async fn the_extended_card_is_signed_too_and_the_jku_is_in_the_header_when_configured() {
    let signer = CardSigner::from_pem(&generate_signing_key_pem(false), None)
        .unwrap()
        .with_jku("https://agent.example.com/.well-known/jwks.json")
        .unwrap();
    let server = TestServer::start(Setup {
        signer: Some(signer.clone()),
        extended: Some(extended()),
        ..Setup::default()
    })
    .await;
    let key = signer.verifying_key();
    let public = public_card(&server).await;
    assert_eq!(key.verify_card(&public), Ok(()));
    let client = server.client(Some(TOKEN)).await;
    let card = client
        .get_extended_agent_card(&extended_request())
        .await
        .unwrap();
    assert!(card.skills.iter().any(|s| s.id == "audit"));
    assert_eq!(
        key.verify_card(&card),
        Ok(()),
        "the extended card carries its own signature"
    );
    let mut altered = card.clone();
    altered.skills.retain(|s| s.id != "audit");
    assert_eq!(key.verify_card(&altered), Err(VerifyError::Invalid));
    // The public card's signature is not the extended card's (they say different things).
    let mut swapped = card;
    swapped.signatures = public.signatures.clone();
    assert_eq!(key.verify_card(&swapped), Err(VerifyError::Invalid));
    let header: serde_json::Value = serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&public.signatures.unwrap()[0].protected)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        header["jku"],
        "https://agent.example.com/.well-known/jwks.json"
    );
    assert_eq!(header["kid"], signer.kid());
}
