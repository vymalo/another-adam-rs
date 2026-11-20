//! A2UI messages a tool emits: a surface with its components, under the screen's own catalog.

use adam_a2a_runtime::ASK_SURFACE_PREFIX;
use serde_json::{Value, json};

/// The A2UI version these messages are written for.
pub const A2UI_VERSION: &str = "1.0";

/// A surface id made from `raw` (a model's call id, say): the characters of an identifier
/// (`A-Z a-z 0-9 _ . : -`), anything else written as `-`, at most 64 characters.
pub(crate) fn safe_id(raw: &str) -> String {
    let mut id: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-') {
                c
            } else {
                '-'
            }
        })
        .take(64)
        .collect();
    if id.is_empty() {
        id.push('x');
    }
    id
}

/// The id of the surface `ask_user` draws its form on for the tool call `call_id`: the prefix the A2A
/// layer recognises as a form's ([`ASK_SURFACE_PREFIX`]) and the call's id made into an identifier
/// (`A-Z a-z 0-9 _ . : -`, anything else written as `-`, at most 64 characters). An answer comes
/// back naming this surface, so a reader that holds the call can tell which question the answer is
/// for.
#[must_use]
pub fn ask_surface_id(call_id: &str) -> String {
    format!("{ASK_SURFACE_PREFIX}{}", safe_id(call_id))
}

/// The id of the surface a form is asked on again after its answer was refused: the first form's id
/// ([`ask_surface_id`]) and `.r<attempt>`, within the 64 characters an identifier may have after
/// `ask-`. The call's id gives up characters to make room for the suffix, so the id still names the
/// call ([`is_ask_surface_of`]).
#[must_use]
pub fn ask_retry_surface_id(call_id: &str, attempt: u32) -> String {
    let suffix = format!(".r{attempt}");
    let room = 64usize.saturating_sub(suffix.len());
    let head: String = safe_id(call_id).chars().take(room).collect();
    format!("{ASK_SURFACE_PREFIX}{head}{suffix}")
}

/// The number of times the form on `surface` was asked again: `0` for the first form's id and `n`
/// for `.r<n>` after it. `None` for a surface that is neither.
#[must_use]
pub fn ask_attempt(surface: &str) -> Option<u32> {
    if !surface.starts_with(ASK_SURFACE_PREFIX) {
        return None;
    }
    match surface.rsplit_once(".r") {
        Some((head, n))
            if head.len() > ASK_SURFACE_PREFIX.len()
                && !n.is_empty()
                && n.bytes().all(|b| b.is_ascii_digit()) =>
        {
            n.parse().ok()
        }
        _ => Some(0),
    }
}

/// Whether `surface` is a form drawn for the tool call `call_id`: the id [`ask_surface_id`] makes,
/// or the id of a form asked again ([`ask_retry_surface_id`]).
#[must_use]
pub fn is_ask_surface_of(call_id: &str, surface: &str) -> bool {
    let own = ask_surface_id(call_id);
    if surface == own {
        return true;
    }
    let Some((head, n)) = surface.rsplit_once(".r") else {
        return false;
    };
    let numbered = !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit());
    let room = (ASK_SURFACE_PREFIX.len() + 64).saturating_sub(2 + n.len());
    numbered && own.starts_with(head) && head.len() == own.len().min(room)
}

/// The A2UI messages that create surface `surface_id` under `catalog_id` and give it
/// `components` (one of them has the id `root`): `[createSurface, updateComponents]`.
pub(crate) fn surface(surface_id: &str, catalog_id: &str, components: Vec<Value>) -> Value {
    json!([
        {"version": A2UI_VERSION, "createSurface": {"surfaceId": surface_id, "catalogId": catalog_id}},
        {"version": A2UI_VERSION, "updateComponents": {"surfaceId": surface_id, "components": components}}
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_surface_is_a_create_and_an_update_for_the_same_id_and_catalog() {
        let s = surface(
            "ask-1",
            "https://c/",
            vec![json!({"id": "root", "component": "Text", "text": "x"})],
        );
        assert_eq!(
            s[0]["createSurface"],
            json!({"surfaceId": "ask-1", "catalogId": "https://c/"})
        );
        assert_eq!(s[1]["updateComponents"]["surfaceId"], "ask-1");
        assert_eq!(s[1]["updateComponents"]["components"][0]["id"], "root");
        assert_eq!(s[0]["version"], "1.0");
        assert_eq!(s[1]["version"], "1.0");
    }

    #[test]
    fn a_form_asked_again_has_its_own_surface_that_still_names_its_call() {
        assert_eq!(ask_retry_surface_id("call_1", 1), "ask-call_1.r1");
        assert_eq!(ask_attempt("ask-call_1"), Some(0));
        assert_eq!(ask_attempt("ask-call_1.r1"), Some(1));
        assert_eq!(ask_attempt("ask-call_1.r12"), Some(12));
        assert_eq!(ask_attempt("ask-call_1.rx"), Some(0));
        assert_eq!(ask_attempt("chat-1"), None);
        assert!(is_ask_surface_of("call_1", "ask-call_1"));
        assert!(is_ask_surface_of("call_1", "ask-call_1.r3"));
        for other in [
            "ask-call_2.r1",
            "ask-call_1.r",
            "ask-call_1.rx",
            "ask-call_.r1",
            "ask-call_1x.r1",
        ] {
            assert!(!is_ask_surface_of("call_1", other), "{other}");
        }
        // A long call id gives up its last characters, and the surface is still an identifier of
        // at most 64 characters after `ask-` that names the call.
        let long = "c".repeat(100);
        let retry = ask_retry_surface_id(&long, 10);
        assert_eq!(retry.len(), ASK_SURFACE_PREFIX.len() + 64);
        assert!(retry.ends_with(".r10"));
        assert!(is_ask_surface_of(&long, &retry));
        assert!(is_ask_surface_of(&long, &ask_surface_id(&long)));
        assert!(!is_ask_surface_of(&"d".repeat(100), &retry));
    }

    #[test]
    fn an_id_keeps_identifier_characters_and_is_never_empty_or_long() {
        assert_eq!(safe_id("call_abc-1.2:3"), "call_abc-1.2:3");
        assert_eq!(safe_id("a b/c"), "a-b-c");
        assert_eq!(safe_id(""), "x");
        assert_eq!(safe_id(&"é".repeat(100)), "-".repeat(64));
    }
}
