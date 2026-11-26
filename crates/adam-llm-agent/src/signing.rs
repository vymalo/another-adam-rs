//! Signing the turns a step commits ([ADR 0068], slice T3a-1).
//!
//! The one seam is [`sign_messages`], called by [`LlmAgent::sign_transition`](crate::LlmAgent)
//! over the state a step hands the runtime, so every site of the loop that pushes a message is
//! covered without knowing about signatures. Each assistant and tool message that carries no turn
//! signature is signed at its position (`idx`, and `prv`, the chain over every message before it,
//! a person's included), and the chain is folded once, in order, over the messages **as stored**
//! (with the signature just set). A person's message is chained, never signed.
//!
//! [ADR 0068]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0068-adam-signs-its-own-history.md

use std::collections::BTreeMap;

use adam_core::RunId;
use adam_core::turn::{
    ChainHash, ConversationHash, TurnKeyError, TurnKeys, TurnPosition, sign_turn,
};
use adam_model::{ADAM_MAKER, Message};
use serde_json::{Map, Value};

/// Whether `message` is one adam signs (an assistant or a tool message) and carries no signature.
fn needs_signature(message: &Message) -> bool {
    let extra = match message {
        Message::Assistant { extra_content, .. } | Message::Tool { extra_content, .. } => {
            extra_content
        }
        Message::User { .. } => return false,
    };
    !extra
        .as_ref()
        .and_then(|makers| makers.get(ADAM_MAKER))
        .and_then(|adam| adam.get("turn_signature"))
        .is_some_and(Value::is_string)
}

/// Put `signature` in the message's `extra_content.adam.turn_signature`, beside any other member.
fn attach(message: &mut Message, signature: String) {
    let (Message::Assistant { extra_content, .. } | Message::Tool { extra_content, .. }) = message
    else {
        return;
    };
    let makers = extra_content.get_or_insert_with(BTreeMap::new);
    let adam = makers
        .entry(ADAM_MAKER.to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    if !adam.is_object() {
        *adam = Value::Object(Map::new());
    }
    if let Some(adam) = adam.as_object_mut() {
        adam.insert("turn_signature".into(), Value::String(signature));
    }
}

/// A message as the transcript stores it (`Conversation::split` does the same).
fn row(message: &Message) -> Value {
    serde_json::to_value(message).unwrap_or(Value::Null)
}

/// Sign every unsigned assistant and tool message of `messages`, in order, with `keys`, and give
/// how many were signed. Messages after the last one that needs a signature are not read.
///
/// # Errors
///
/// [`TurnKeyError`] from the keys; `messages` may then be partly signed, and the caller discards
/// them (the step is retried).
pub(crate) async fn sign_messages(
    keys: &(impl TurnKeys + ?Sized),
    run: RunId,
    conversation: Option<&str>,
    messages: &mut [Message],
) -> Result<usize, TurnKeyError> {
    let Some(last) = messages.iter().rposition(needs_signature) else {
        return Ok(0);
    };
    let cnv = ConversationHash::of(conversation, run);
    let mut prv = ChainHash::seed(run, &cnv);
    let mut signed = 0;
    for (idx, message) in messages.iter_mut().enumerate().take(last + 1) {
        if needs_signature(message) {
            let position = TurnPosition {
                run,
                cnv,
                idx: u32::try_from(idx).unwrap_or(u32::MAX),
                prv,
            };
            let signature = sign_turn(keys, &position, &row(message)).await?;
            attach(message, signature);
            signed += 1;
        }
        prv = prv.next(&row(message));
    }
    Ok(signed)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use adam_core::turn::{Ed25519TurnKeys, KeyId, turn_signature};
    use adam_model::ToolCall;
    use base64::Engine as _;
    use serde_json::json;

    /// RFC 8032 §7.1 test 1, the key of the golden vector of `adam-core`'s T1 tests.
    const SECRET: &str = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";

    fn keys() -> Ed25519TurnKeys {
        let mut der: Vec<u8> = (0..32)
            .step_by(2)
            .map(|i| u8::from_str_radix(&"302e020100300506032b657004220420"[i..i + 2], 16).unwrap())
            .collect();
        der.extend(
            (0..SECRET.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&SECRET[i..i + 2], 16).unwrap()),
        );
        let pem = secrecy::SecretString::from(format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
            base64::engine::general_purpose::STANDARD.encode(der)
        ));
        Ed25519TurnKeys::from_pkcs8_pem(&pem, Some(KeyId::new("vault:turn-key#v1").unwrap()), None)
            .unwrap()
    }

    fn run() -> RunId {
        RunId(uuid::Uuid::parse_str("0190f0c2-6a1b-7c3d-8e4f-0123456789ab").unwrap())
    }

    fn transcript() -> Vec<Message> {
        vec![
            Message::user_text("Bonjour, ça va ? 😀"),
            Message::Assistant {
                content: Vec::new(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "run".into(),
                    arguments: json!({"cmd": "cargo test", "n": 1.5}),
                    ..Default::default()
                }],
                reasoning: None,
                provider: None,
                extra_content: None,
            },
            Message::tool_result("c1", "12 passed"),
        ]
    }

    /// The vector was made outside this crate (2026-11-26) by Python: `hashlib`,
    /// `json.dumps(sort_keys=True, separators=(",", ":"), ensure_ascii=False)` (RFC 8785 for these
    /// values) and `cryptography`'s Ed25519 over the key above; the same chain and payload as
    /// `adam-core`'s T1 vector, over a three-message transcript signed in order.
    #[tokio::test]
    async fn the_chain_and_the_signatures_match_the_golden_vector() {
        let mut messages = transcript();
        let signed = sign_messages(&keys(), run(), Some("ctx-1"), &mut messages)
            .await
            .unwrap();
        assert_eq!(
            signed, 2,
            "the assistant and the tool message, not the person's"
        );
        let rows: Vec<Value> = messages.iter().map(row).collect();
        assert!(turn_signature(&rows[0]).is_none());
        assert_eq!(turn_signature(&rows[1]), Some(GOLDEN_ASSISTANT_JWS));
        assert_eq!(turn_signature(&rows[2]), Some(GOLDEN_TOOL_JWS));
        let cnv = ConversationHash::of(Some("ctx-1"), run());
        assert_eq!(
            ChainHash::over(run(), &cnv, &rows[..1]).to_string(),
            GOLDEN_PRV_ASSISTANT
        );
        assert_eq!(
            ChainHash::over(run(), &cnv, &rows[..2]).to_string(),
            GOLDEN_PRV_TOOL
        );
        assert_eq!(ChainHash::over(run(), &cnv, &rows).to_string(), GOLDEN_HEAD);
    }

    const GOLDEN_PRV_ASSISTANT: &str = "2qtpHpcR_5NiTZUlWWYgKdhVxToZNDE85nYdN2pzFW4";
    const GOLDEN_PRV_TOOL: &str = "H6HLKS8pJZZacJfws4YFshpJ3Wj2tw-mfJh_PTt_LYU";
    const GOLDEN_HEAD: &str = "p0hMS--V5LWn6liiCWvxqVVYB6p-HM35zS1GKnHuj0s";
    const GOLDEN_ASSISTANT_JWS: &str = "eyJhbGciOiJFZERTQSIsImtpZCI6InZhdWx0OnR1cm4ta2V5I3YxIiwidHlwIjoiYWRhbS10dXJuL3YxIn0..xHMiOorjazqzO6Ye_5AWmXoTuZL92vbgLyObdaXyNUCdFQC1qXN_T5wmQ6vHkoM1MIfyDf0Fz_p8ZxZQ21d4AA";
    const GOLDEN_TOOL_JWS: &str = "eyJhbGciOiJFZERTQSIsImtpZCI6InZhdWx0OnR1cm4ta2V5I3YxIiwidHlwIjoiYWRhbS10dXJuL3YxIn0..aQqcMUS30DBTtCzSyDZY1VIwneydHOfhSE1ViH-n6TEM_Doas5mvI_LuZpNC4Lcdtq4fOyeHdfMpHuwG0zdsCQ";

    #[tokio::test]
    async fn signed_messages_are_left_alone_and_a_providers_members_are_kept() {
        let mut messages = transcript();
        if let Message::Assistant { extra_content, .. } = &mut messages[1] {
            *extra_content = Some(BTreeMap::from([(
                "google".to_owned(),
                json!({"thought_signature": "G"}),
            )]));
        }
        let k = keys();
        assert_eq!(
            sign_messages(&k, run(), None, &mut messages).await.unwrap(),
            2
        );
        let first = messages.clone();
        assert_eq!(
            sign_messages(&k, run(), None, &mut messages).await.unwrap(),
            0,
            "a second pass signs nothing"
        );
        assert_eq!(messages, first);
        let Message::Assistant { extra_content, .. } = &messages[1] else {
            unreachable!()
        };
        let members = extra_content.as_ref().unwrap();
        assert_eq!(members["google"], json!({"thought_signature": "G"}));
        assert!(members["adam"]["turn_signature"].is_string());
    }
}
