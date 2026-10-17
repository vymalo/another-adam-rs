//! Who a message was sent for: the thread's addressed agent, which a person drives (from their screen,
//! or a program holding their token), or an agent that another agent asked (`ask_agent` of
//! `thread-tools/v1`), behind which nobody is.
//!
//! The orchestration layer says it in the token of the thread-tools grant, a JWS whose claims hold
//! `caller` (`"main"` or `"ask:<n>"`) and `depth` (`docs/api/thread-tools-v1.md` of
//! `vymalo/another-agentic-system`, "The token"). [`vymalo_inbound`](crate::vymalo_inbound) reads the
//! claims of every message that carries the grant into the context key [`CONTEXT_CALLER`]. The
//! signature is not checked here: the key is the orchestrator's and the agent has none. What is trusted
//! is the authenticated A2A caller that sent the message, as for the rest of its metadata (a form's
//! answer, the access tier); a model's text never reaches the metadata.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};

/// The context key under which a message that carries `thread-tools/v1` says who it was sent for:
/// `{"caller": "main"}` (the thread's addressed agent), `{"caller": "ask:<n>", "depth": <d>}` (the
/// n-th agent asked in the job, `depth` when the claim is a number from 1 to 255), or
/// `{"caller": "unreadable"}` when the message carries the grant and its token's claims cannot be read.
/// A message without the grant leaves the key as it is. [`asked_by_an_agent`] reads it. It is one of
/// `adam_llm_agent::PROTECTED_CONTEXT_KEYS` (a test keeps the two equal): a large context does not
/// drop it, and a child run starts with it.
pub const CONTEXT_CALLER: &str = "vymalo.caller";

/// The `caller` of [`CONTEXT_CALLER`] for a grant whose token's claims cannot be read.
pub const UNREADABLE_CALLER: &str = "unreadable";

/// The most bytes of a token whose claims are read (the contract's tokens are at most 2 KiB).
const MAX_TOKEN_BYTES: usize = 4096;

/// Whether a run whose context holds `entry` under [`CONTEXT_CALLER`] was sent its newest message
/// that carried a thread-tools grant for an agent **another agent asked**, so that no person is
/// behind it: any entry but `{"caller": "main"}`, an unreadable one included (fail closed). No entry
/// (no message of the run carried a grant, as from a plain A2A client) is `false`.
pub fn asked_by_an_agent(entry: Option<&Value>) -> bool {
    entry.is_some_and(|entry| entry.get("caller").and_then(Value::as_str) != Some("main"))
}

/// The [`CONTEXT_CALLER`] entry for the thread-tools metadata `entry` of a message, from the
/// `caller` and `depth` claims of its token. `main` counts only with `depth` 0; `ask:<n>` is `ask:`
/// and a number from 1 with no leading zero. Anything else, a token that is not three base64url
/// segments with a JSON object in the middle one included, is [`UNREADABLE_CALLER`].
pub(crate) fn caller_of(entry: &Value) -> Value {
    read(entry).unwrap_or_else(|| json!({ "caller": UNREADABLE_CALLER }))
}

fn read(entry: &Value) -> Option<Value> {
    let token = entry.get("token")?.as_str()?;
    if token.len() > MAX_TOKEN_BYTES {
        return None;
    }
    let mut segments = token.split('.');
    let (Some(_), Some(payload), Some(_), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        return None;
    };
    let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
    let caller = claims.get("caller")?.as_str()?;
    let depth = claims.get("depth").and_then(Value::as_u64);
    if caller == "main" {
        return (depth == Some(0)).then(|| json!({ "caller": "main" }));
    }
    let n = caller.strip_prefix("ask:")?;
    if n.is_empty() || n.len() > 10 || n.starts_with('0') || !n.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    Some(match depth.filter(|d| (1..=255).contains(d)) {
        Some(depth) => json!({ "caller": caller, "depth": depth }),
        None => json!({ "caller": caller }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A token with `claims` in its middle segment, as the orchestrator mints it (the signature is
    /// not read).
    fn token(claims: &Value) -> String {
        let segment = |v: &Value| URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap());
        format!(
            "{}.{}.c2lnbmF0dXJl",
            segment(&json!({"alg": "HS256", "typ": "JWT", "kid": "0123456789abcdef"})),
            segment(claims)
        )
    }

    fn grant(token: &str) -> Value {
        json!({"url": "http://o/thread-tools/t/mcp", "token": token, "expiresAt": "2999-01-01T00:00:00Z"})
    }

    fn claims(caller: &str, depth: Value) -> Value {
        json!({"iss": "orch", "aud": "thread-tools", "sub": "01927a4e-3b00-7000-8000-000000000001",
               "job": 3, "agt": "coder", "caller": caller, "depth": depth, "jti": "m-1",
               "iat": 1_790_856_000, "exp": 1_790_863_200})
    }

    #[test]
    fn the_claims_of_the_token_say_who_the_message_was_sent_for() {
        let main = caller_of(&grant(&token(&claims("main", json!(0)))));
        assert_eq!(main, json!({"caller": "main"}));
        assert!(!asked_by_an_agent(Some(&main)));
        let asked = caller_of(&grant(&token(&claims("ask:2", json!(1)))));
        assert_eq!(asked, json!({"caller": "ask:2", "depth": 1}));
        assert!(asked_by_an_agent(Some(&asked)));
        // A depth out of range is left out; the caller is still an asked agent.
        let deep = caller_of(&grant(&token(&claims("ask:12", json!(300)))));
        assert_eq!(deep, json!({"caller": "ask:12"}));
    }

    #[test]
    fn a_token_whose_claims_cannot_be_read_is_nobodys_and_not_a_person() {
        let unreadable = json!({"caller": UNREADABLE_CALLER});
        let three = |middle: &str| format!("e30.{middle}.c2ln");
        let cases = [
            grant("aaa.bbb.ccc"),
            grant("thread-token"),
            grant(""),
            grant("a.b"),
            grant(&three("e30")),
            grant(&format!("{}.x", token(&claims("main", json!(0))))),
            grant(&three(&URL_SAFE_NO_PAD.encode(b"[\"main\"]"))),
            grant(&three(&URL_SAFE_NO_PAD.encode(b"not json"))),
            grant(&token(&claims("main", json!(1)))),
            grant(&token(&claims("main", json!(null)))),
            grant(&token(&json!({"caller": "main"}))),
            grant(&token(&claims("Main", json!(0)))),
            grant(&token(&claims("ask:", json!(1)))),
            grant(&token(&claims("ask:0", json!(1)))),
            grant(&token(&claims("ask:01", json!(1)))),
            grant(&token(&claims("ask:-1", json!(1)))),
            grant(&token(&claims("ask:1x", json!(1)))),
            grant(&token(&claims("person", json!(0)))),
            grant(&token(&json!({"caller": 1, "depth": 0}))),
            // Padding is not base64url without padding.
            grant(&three(&format!(
                "{}=",
                URL_SAFE_NO_PAD.encode(br#"{"caller":"main","depth":0}"#)
            ))),
            // A token past the bound is not read, whatever it says.
            grant(&token(
                &json!({"caller": "main", "depth": 0, "pad": "x".repeat(MAX_TOKEN_BYTES)}),
            )),
            json!({"url": "http://o", "expiresAt": "2999-01-01T00:00:00Z"}),
            json!({"token": 7}),
            json!("main"),
            json!(null),
        ];
        for entry in cases {
            let caller = caller_of(&entry);
            assert_eq!(caller, unreadable, "{entry}");
            assert!(asked_by_an_agent(Some(&caller)), "{entry}");
        }
    }

    #[test]
    fn the_key_is_one_the_run_never_loses_to_a_large_context() {
        assert_eq!(CONTEXT_CALLER, adam_llm_agent::CALLER_CONTEXT_KEY);
        assert!(adam_llm_agent::PROTECTED_CONTEXT_KEYS.contains(&CONTEXT_CALLER));
    }

    #[test]
    fn no_entry_is_no_grant_and_any_entry_but_main_is_an_asked_agent() {
        assert!(!asked_by_an_agent(None));
        assert!(!asked_by_an_agent(Some(&json!({"caller": "main"}))));
        for entry in [
            json!({"caller": "ask:1"}),
            json!({"caller": UNREADABLE_CALLER}),
            json!({"caller": null}),
            json!({}),
            json!("main"),
            json!(null),
        ] {
            assert!(asked_by_an_agent(Some(&entry)), "{entry}");
        }
    }
}
