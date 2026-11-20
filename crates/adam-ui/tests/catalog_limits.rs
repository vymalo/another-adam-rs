//! The limits of a catalog read from a message, at their edges: the contract allows 64 KiB and 64
//! components, canonical JSON nests 32 levels and holds whole numbers up to 2^53 - 1. The numbers
//! are written here, not read from the constants, so moving a limit is a change this file sees.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use adam_ui::{Catalog, CatalogError, Claimed, canonical_json, catalog_digest};
use serde_json::{Value, json};

const ID: &str = "https://example.test/limits";

fn claimed(document: &Value) -> Claimed {
    Claimed {
        catalog_id: ID.to_owned(),
        version: 1,
        digest: catalog_digest(document).unwrap(),
    }
}

/// A catalog with one component whose description is `padding` bytes long.
fn padded(padding: usize) -> Value {
    json!({
        "catalogId": ID,
        "protocolVersion": "1.0",
        "components": {"Note": {"type": "object", "description": "x".repeat(padding)}}
    })
}

/// A catalog with `count` components.
fn with_components(count: usize) -> Value {
    let components: serde_json::Map<String, Value> = (0..count)
        .map(|n| (format!("C{n:03}"), json!({"type": "object"})))
        .collect();
    json!({"catalogId": ID, "protocolVersion": "1.0", "components": components})
}

#[test]
fn a_catalog_of_exactly_64_kib_is_taken_and_one_byte_more_is_too_large() {
    let base = canonical_json(&padded(0)).unwrap().len();
    let exact = padded(65_536 - base);
    assert_eq!(canonical_json(&exact).unwrap().len(), 65_536);
    assert!(Catalog::from_document(exact.clone(), &claimed(&exact)).is_ok());

    let over = padded(65_536 - base + 1);
    assert_eq!(canonical_json(&over).unwrap().len(), 65_537);
    assert_eq!(
        Catalog::from_document(over.clone(), &claimed(&over)).unwrap_err(),
        CatalogError::TooLarge
    );
}

#[test]
fn a_catalog_of_64_components_is_taken_and_65_are_refused() {
    let sixty_four = with_components(64);
    assert!(Catalog::from_document(sixty_four.clone(), &claimed(&sixty_four)).is_ok());

    let sixty_five = with_components(65);
    let error = Catalog::from_document(sixty_five.clone(), &claimed(&sixty_five)).unwrap_err();
    assert!(
        matches!(&error, CatalogError::Shape(m) if m.contains("1 to 64 components, not 65")),
        "{error}"
    );
}

#[test]
fn canonical_json_nests_32_levels_and_refuses_the_33rd() {
    let nested = |levels: usize| {
        let mut value = json!(1);
        for _ in 0..levels {
            value = json!([value]);
        }
        value
    };
    assert_eq!(
        canonical_json(&nested(32)).unwrap(),
        format!("{}1{}", "[".repeat(32), "]".repeat(32))
    );
    assert!(matches!(
        canonical_json(&nested(33)),
        Err(CatalogError::NotCanonical(m)) if m.contains("deeper than 32")
    ));
}

#[test]
fn the_largest_safe_integer_is_whole_and_the_next_is_refused() {
    assert_eq!(
        canonical_json(&json!({"a": 9_007_199_254_740_991_i64})).unwrap(),
        r#"{"a":9007199254740991}"#
    );
    assert_eq!(
        canonical_json(&json!({"a": -9_007_199_254_740_991_i64})).unwrap(),
        r#"{"a":-9007199254740991}"#
    );
    // As a double, which is how a message's metadata arrives.
    assert_eq!(
        canonical_json(&json!({"a": 9_007_199_254_740_991.0})).unwrap(),
        r#"{"a":9007199254740991}"#
    );
    assert!(canonical_json(&json!({"a": 9_007_199_254_740_992_i64})).is_err());
    assert!(canonical_json(&json!({"a": -9_007_199_254_740_992_i64})).is_err());
}
