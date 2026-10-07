//! A secret is never a value: the specs `resolve` makes hold references and nothing else.
//!
//! Two sentinels are put into the Secret names and keys of the examples. The name sentinel must
//! appear only as the `name` of a `SecretRef`; the key sentinel only as the `key` of one, as the
//! name of the variable a header's secret is carried in (named by the Secret key), and
//! inside a `${…}` reference of the extra MCP file. Anywhere else would be a Secret's identity
//! leaking into a place that holds values: a literal variable, a command, a file's text.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use adam_operator_ports::memory::plain_text;
use adam_operator_ports::{EnvValue, StoreKind};
use common::{example, typed};
use serde_json::Value;

const NAME: &str = "SENTINEL-secret-name";

/// A variable name: the key sentinel is a valid one, because a header's key becomes one.
const KEY: &str = "SENTINEL_SECRET_KEY";

/// Both examples with the sentinel in every Secret reference they hold. Returns the number of
/// references changed, so the walk below cannot pass by finding nothing.
fn with_sentinels(service: &mut Value, config: &mut Value) -> usize {
    fn walk(v: &mut Value, n: &mut usize, counter: &mut usize) {
        match v {
            Value::Object(m) => {
                let is_ref = m.len() == 2
                    && m.contains_key("name")
                    && m.contains_key("key")
                    && m["name"].is_string();
                if is_ref {
                    *counter += 1;
                    m.insert("name".into(), Value::String(format!("{NAME}-{counter}")));
                    m.insert("key".into(), Value::String(format!("{KEY}_{counter}")));
                    *n += 1;
                } else {
                    for child in m.values_mut() {
                        walk(child, n, counter);
                    }
                }
            }
            Value::Array(a) => a.iter_mut().for_each(|c| walk(c, n, counter)),
            _ => {}
        }
    }
    let mut n = 0;
    let mut counter = 0;
    // `configRef` is `{name}` only, so it is not mistaken for a reference.
    walk(service, &mut n, &mut counter);
    walk(config, &mut n, &mut counter);
    n
}

/// Every place in a serialised spec where a sentinel is, and whether it may be there.
fn leaks(value: &Value, path: &str, out: &mut Vec<String>, found: &mut usize) {
    match value {
        Value::Object(m) => {
            let is_ref = m.len() == 2 && m.contains_key("name") && m.contains_key("key");
            let is_env = m.len() == 2 && m.contains_key("name") && m.contains_key("value");
            for (k, v) in m {
                let here = format!("{path}.{k}");
                match v {
                    Value::String(s) if is_ref && (k == "name" || k == "key") => {
                        if s.contains(NAME) || s.contains(KEY) {
                            *found += 1;
                        }
                    }
                    Value::String(s) if is_env && k == "name" => {
                        // A header's variable is named by its Secret key.
                        if s.contains(NAME) {
                            out.push(format!(
                                "{here}: the name sentinel in a variable's name: {s}"
                            ));
                        }
                        if s.contains(KEY) {
                            *found += 1;
                        }
                    }
                    _ => leaks(v, &here, out, found),
                }
            }
        }
        Value::Array(a) => a
            .iter()
            .enumerate()
            .for_each(|(i, v)| leaks(v, &format!("{path}[{i}]"), out, found)),
        Value::String(s) => {
            if s.contains(NAME) {
                out.push(format!("{path}: the name sentinel in plain text: {s}"));
            }
            if s.contains(KEY) {
                // Only as `${KEY…}` in a file's text.
                let mut rest = s.as_str();
                while let Some(at) = rest.find(KEY) {
                    let before = &rest[..at];
                    let open = before.rfind("${");
                    let ok = open.is_some_and(|o| !before[o..].contains('}'));
                    if !ok {
                        out.push(format!(
                            "{path}: the key sentinel outside a ${{…}} reference: {s}"
                        ));
                        break;
                    }
                    *found += 1;
                    rest = &rest[at + KEY.len()..];
                }
            }
        }
        _ => {}
    }
}

fn resolved_with_sentinels(name: &str) -> (adam_operator_domain::ResolvedAgent, usize) {
    let (mut s, mut c) = example(name);
    let changed = with_sentinels(&mut s, &mut c);
    let (s, c) = typed(&s, &c);
    let r = adam_operator_domain::resolve(&s, &c, adam_operator_ports::OwnerHandle::none())
        .unwrap_or_else(|i| {
            panic!("{i:?}");
        });
    (r, changed)
}

#[test]
fn the_runtime_spec_of_both_examples_holds_no_secret_value() {
    for (name, min_refs) in [("coder", 7), ("chat", 3)] {
        let (r, changed) = resolved_with_sentinels(name);
        assert!(
            changed >= min_refs,
            "{name}: the sentinels went into {changed} references"
        );
        let mut out = Vec::new();
        let mut found = 0;
        leaks(
            &serde_json::to_value(&r.runtime).unwrap(),
            "runtime",
            &mut out,
            &mut found,
        );
        leaks(
            &serde_json::to_value(&r.store).unwrap(),
            "store",
            &mut out,
            &mut found,
        );
        assert!(out.is_empty(), "{name}:\n{}", out.join("\n"));
        assert!(
            found >= changed,
            "{name}: the walk found {found} sentinels for {changed} references: it looks at nothing"
        );
    }
}

#[test]
fn the_plain_text_a_provider_writes_has_no_secret_identity_but_the_files_references() {
    for name in ["coder", "chat"] {
        let (r, _) = resolved_with_sentinels(name);
        for text in plain_text(&r.runtime) {
            assert!(!text.contains(NAME), "{name}: {text}");
            if text.contains(KEY) {
                assert!(
                    text.contains("${"),
                    "{name}: the key sentinel outside the MCP file: {text}"
                );
            }
        }
    }
}

#[test]
fn every_secret_is_a_reference_in_the_places_a_secret_goes() {
    let (r, _) = resolved_with_sentinels("coder");
    let w = &r.runtime.workloads[0];
    let secrets: Vec<&str> = w
        .container
        .env
        .iter()
        .filter(|e| matches!(e.value, EnvValue::Secret(_)))
        .map(|e| e.name.as_str())
        .collect();
    for expected in [
        "A2A_BEARER_TOKENS",
        "DATABASE_URL",
        "MODEL_API_KEY",
        "MODEL_BASE_URL",
    ] {
        assert!(
            secrets.contains(&expected),
            "{expected} is a reference: {secrets:?}"
        );
    }
    // The ones named by their Secret key are the headers'.
    assert_eq!(secrets.iter().filter(|n| n.starts_with(KEY)).count(), 2);
    // A literal is never one of the sentinels.
    for e in &w.container.env {
        if let EnvValue::Literal(v) = &e.value {
            assert!(!v.contains("SENTINEL"), "{}={v}", e.name);
        }
    }
    let StoreKind::Secret(db) = &r.store.kind else {
        panic!()
    };
    assert!(db.name.contains(NAME));
}

#[test]
fn the_walk_fails_what_it_should() {
    // A positive control: a literal that carries a Secret's name is found.
    let (mut r, _) = resolved_with_sentinels("coder");
    r.runtime.workloads[0].container.env[0].value = EnvValue::Literal(format!("{NAME}-1"));
    let mut out = Vec::new();
    leaks(
        &serde_json::to_value(&r.runtime).unwrap(),
        "runtime",
        &mut out,
        &mut 0,
    );
    assert!(!out.is_empty());
    // And so is a key in a command.
    let (mut r, _) = resolved_with_sentinels("coder");
    r.runtime.workloads[0]
        .container
        .args
        .push(format!("--token={KEY}_1"));
    let mut out = Vec::new();
    leaks(
        &serde_json::to_value(&r.runtime).unwrap(),
        "runtime",
        &mut out,
        &mut 0,
    );
    assert!(!out.is_empty());
}

#[test]
fn no_type_of_the_specs_can_hold_a_secret_value() {
    // The structure says it: the only secret-shaped field is `SecretRef { name, key }`. A new field
    // named like a value of a secret in a spec type would fail this list and be looked at.
    let (r, _) = resolved_with_sentinels("coder");
    let json = serde_json::to_string(&r.runtime).unwrap().to_lowercase();
    for forbidden in [
        "\"password\"",
        "\"token\":",
        "\"secretvalue\"",
        "\"value\":{\"secret\":\"",
    ] {
        assert!(!json.contains(forbidden), "{forbidden}");
    }
}
