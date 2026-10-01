//! A2UI messages a tool emits: a surface with its components, under the screen's own catalog.

use serde_json::{Value, json};

/// The A2UI version these messages are written for.
pub const A2UI_VERSION: &str = "v0.9.1";

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
        assert_eq!(s[0]["version"], "v0.9.1");
    }

    #[test]
    fn an_id_keeps_identifier_characters_and_is_never_empty_or_long() {
        assert_eq!(safe_id("call_abc-1.2:3"), "call_abc-1.2:3");
        assert_eq!(safe_id("a b/c"), "a-b-c");
        assert_eq!(safe_id(""), "x");
        assert_eq!(safe_id(&"é".repeat(100)), "-".repeat(64));
    }
}
