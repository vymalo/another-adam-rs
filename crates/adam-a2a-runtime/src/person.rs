//! What the person wrote in the thread, as the orchestration layer tells an agent that **another
//! agent asked** (`ask_agent` of `thread-tools/v1`), behind which nobody is: the only words of the
//! person such a run has.
//!
//! The orchestrator writes it beside the grant of an ask's message, `person: {said, omitted?}`
//! (`docs/api/thread-tools-v1.md` of `vymalo/another-agentic-system`, "The `person` member"): the
//! text of every message the person wrote in the thread, oldest first, as written (a credential in
//! it redacted), each cut at 4 KiB, the oldest left out whole beyond 64 KiB and counted in
//! `omitted`. The words, not a list of repositories: the rule of what names one is the agent's.
//! [`vymalo_inbound`](crate::vymalo_inbound) reads it into the context key [`CONTEXT_PERSON`] of
//! every message that carries the grant, as it reads [`CONTEXT_CALLER`](crate::CONTEXT_CALLER).
//! What is trusted is the authenticated A2A caller that sent the message, as for the rest of its
//! metadata; a model's text never reaches it.

use serde_json::{Value, json};

use crate::vymalo::whole_number;

/// The context key under which a message that carries `thread-tools/v1` says what the person wrote
/// in the thread: `{"said": ["...", ...]}`, with `"omitted": <n>` when the orchestrator left the
/// oldest out, or `null` when the message carries no such member or one that cannot be read (an
/// earlier value is taken away: the words are this message's). A message without the grant leaves
/// the key as it is. [`person_said`] reads it. Not one of `adam_llm_agent::PROTECTED_CONTEXT_KEYS`
/// (a test keeps it out): a protected value is capped at 1 KiB, far under the words, and losing
/// them is fail closed (nothing the person said, nothing named).
pub const CONTEXT_PERSON: &str = "vymalo.person";

/// The most bytes of one entry of `said` that is read: four times the contract's cut (4 KiB).
const MAX_SAID_ENTRY_BYTES: usize = 4 * 4096;

/// The most bytes of all the entries together: twice the contract's bound (64 KiB), and under the
/// context's own (`adam_llm_agent::MAX_CONTEXT_BYTES`, 256 KiB), so that a well-formed value never
/// costs a message the rest of its context.
const MAX_SAID_BYTES: usize = 2 * 64 * 1024;

/// The most entries read.
const MAX_SAID_ENTRIES: usize = 4096;

/// The `said` strings of a [`CONTEXT_PERSON`] entry, oldest first: what the person wrote in the
/// thread, exactly as carried. Empty for no entry, `null`, or one that is not `{"said": [<strings>]}`
/// within the bounds (fail closed: nothing was said).
pub fn person_said(entry: Option<&Value>) -> Vec<String> {
    entry
        .and_then(|entry| entry.get("said"))
        .and_then(said_of)
        .unwrap_or_default()
}

/// The [`CONTEXT_PERSON`] entry for the thread-tools metadata `entry` of a message: `{"said"}` plus
/// `"omitted"` when `person` is an object whose `said` is an array of strings within the bounds,
/// else `null`. `omitted` is kept only as a whole number above 0 (A2A hands numbers over as
/// doubles); it is information, so a bad one is left out and the words are still read.
pub(crate) fn person_of(entry: &Value) -> Value {
    read(entry).unwrap_or(Value::Null)
}

fn read(entry: &Value) -> Option<Value> {
    let person = entry.get("person")?.as_object()?;
    let said = said_of(person.get("said")?)?;
    let mut value = json!({ "said": said });
    if let Some(omitted) = person
        .get("omitted")
        .and_then(whole_number)
        .filter(|n| *n > 0)
    {
        value["omitted"] = json!(omitted);
    }
    Some(value)
}

/// `value` as the strings of `said`; `None` when it is not an array, holds anything but strings,
/// or is past the bounds.
fn said_of(value: &Value) -> Option<Vec<String>> {
    let items = value.as_array()?;
    if items.len() > MAX_SAID_ENTRIES {
        return None;
    }
    let mut total = 0;
    let mut said = Vec::with_capacity(items.len());
    for item in items {
        let text = item.as_str()?;
        total += text.len();
        if text.len() > MAX_SAID_ENTRY_BYTES || total > MAX_SAID_BYTES {
            return None;
        }
        said.push(text.to_owned());
    }
    Some(said)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(person: Value) -> Value {
        json!({"url": "http://o/thread-tools/t/mcp", "token": "a.b.c",
               "expiresAt": "2999-01-01T00:00:00Z", "person": person})
    }

    #[test]
    fn the_words_are_kept_exactly_and_omitted_is_a_whole_number_above_zero() {
        let said = json!([
            "Set up acme/widgets for me, branch main ",
            "  yes, go ahead\n",
            ""
        ]);
        let whole = person_of(&grant(json!({"said": said, "omitted": 3.0})));
        assert_eq!(whole, json!({"said": said, "omitted": 3}));
        assert_eq!(
            person_said(Some(&whole)),
            [
                "Set up acme/widgets for me, branch main ",
                "  yes, go ahead\n",
                ""
            ]
        );
        assert_eq!(person_of(&grant(json!({"said": []}))), json!({"said": []}));
        // `omitted` that says nothing usable is left out; the words still count.
        for omitted in [json!(0), json!(-1), json!(2.5), json!("3"), json!(null)] {
            assert_eq!(
                person_of(&grant(json!({"said": ["hi"], "omitted": omitted}))),
                json!({"said": ["hi"]}),
                "{omitted}"
            );
        }
    }

    #[test]
    fn a_member_that_is_not_a_list_of_strings_within_the_bounds_is_null_and_says_nothing() {
        let long = "x".repeat(MAX_SAID_ENTRY_BYTES + 1);
        let many: Vec<String> =
            vec!["x".repeat(MAX_SAID_ENTRY_BYTES); MAX_SAID_BYTES / MAX_SAID_ENTRY_BYTES + 1];
        let too_many: Vec<&str> = vec!["a"; MAX_SAID_ENTRIES + 1];
        let cases = [
            grant(json!({"said": "acme/widgets"})),
            grant(json!({"said": ["ok", 7]})),
            grant(json!({"said": ["ok", null]})),
            grant(json!({"said": [["acme/widgets"]]})),
            grant(json!({"said": [long]})),
            grant(json!({"said": many})),
            grant(json!({"said": too_many})),
            grant(json!({"omitted": 2})),
            grant(json!("acme/widgets")),
            grant(json!(["acme/widgets"])),
            grant(json!(null)),
            json!({"url": "http://o", "token": "a.b.c", "expiresAt": "2999-01-01T00:00:00Z"}),
            json!("main"),
            json!(null),
        ];
        for entry in cases {
            let person = person_of(&entry);
            assert_eq!(person, Value::Null, "{entry}");
            assert!(person_said(Some(&person)).is_empty(), "{entry}");
        }
        assert!(person_said(None).is_empty());
        // The bounds, from inside: the largest list that is read.
        let full: Vec<String> =
            vec!["x".repeat(MAX_SAID_ENTRY_BYTES); MAX_SAID_BYTES / MAX_SAID_ENTRY_BYTES];
        assert_eq!(
            person_said(Some(&person_of(&grant(json!({"said": full}))))).len(),
            MAX_SAID_BYTES / MAX_SAID_ENTRY_BYTES
        );
    }

    #[test]
    fn the_key_is_not_a_protected_one_so_the_words_reach_the_run_whole() {
        assert!(!adam_llm_agent::PROTECTED_CONTEXT_KEYS.contains(&CONTEXT_PERSON));
        // Far over `MAX_PROTECTED_VALUE_BYTES`, which a protected key would drop: merged like any key.
        let said: Vec<String> = (0..64)
            .map(|i| format!("message {i}: {}", "x".repeat(200)))
            .collect();
        let value = json!({"said": said});
        assert!(
            serde_json::to_vec(&value).unwrap().len()
                > 8 * adam_llm_agent::MAX_PROTECTED_VALUE_BYTES
        );
        let mut conversation = adam_llm_agent::Conversation::new("t");
        let mut update = serde_json::Map::new();
        update.insert(CONTEXT_PERSON.to_owned(), value.clone());
        assert!(conversation.merge_context(&update));
        assert_eq!(conversation.context.get(CONTEXT_PERSON), Some(&value));
        assert_eq!(
            person_said(conversation.context.get(CONTEXT_PERSON)).len(),
            64
        );
    }
}
