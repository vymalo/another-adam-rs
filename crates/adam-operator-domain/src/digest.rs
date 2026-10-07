//! The digest: sha256 over a canonical serialisation, so the same input always gives the same
//! digest.

use std::collections::BTreeMap;

use adam_operator_ports::{DeletionPolicy, Network, OwnerHandle, RuntimeSpec};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Canonical JSON: object keys sorted, no whitespace, strings and numbers as `serde_json` writes
/// them. Written here rather than trusted to a serde feature: the digest must not change because
/// another crate of the build turned on `preserve_order`.
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write(value, &mut out);
    out
}

fn write(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        // Serialising a string cannot fail; the fallback keeps the function total.
        Value::String(s) => out.push_str(&serde_json::to_string(s).unwrap_or_default()),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let sorted: BTreeMap<&String, &Value> = map.iter().collect();
            out.push('{');
            for (i, (k, v)) in sorted.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(k).unwrap_or_default());
                out.push(':');
                write(v, out);
            }
            out.push('}');
        }
    }
}

/// The same value with every object's members in key order, whether or not another crate of the
/// build turned on `serde_json`'s `preserve_order`: what the operator writes out (a file, an
/// environment variable) is then the same bytes in every build.
pub fn sorted(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let keys: BTreeMap<&String, &Value> = map.iter().collect();
            Value::Object(
                keys.into_iter()
                    .map(|(k, v)| (k.clone(), sorted(v)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

/// `sha256:<hex>` of a JSON value, canonically serialised.
pub fn digest_json(value: &Value) -> String {
    format!(
        "sha256:{:x}",
        Sha256::digest(canonical_json(value).as_bytes())
    )
}

/// The digest of what the pods run: the spec with everything that is applied but does not change
/// what a pod runs set aside.
///
/// Set aside: the owner, the deletion policy, the digest itself, `suspend`, each workload's
/// `replicas` and `min_available`, and who may reach the port. A bigger `scaling.workers`, a
/// suspend, a different `allowFrom` or a new owner must not roll the running pods; a changed image,
/// variable, file, volume, probe or sidecar must (adam reads its files and variables at startup
/// only).
///
/// `RuntimeSpec` serialises without a failing path (no map with non-string keys), so the fallback
/// to `null` is unreachable; it keeps the function total.
pub fn spec_digest(spec: &RuntimeSpec) -> String {
    let mut pods = spec.clone();
    pods.owner = OwnerHandle::none();
    pods.deletion = DeletionPolicy::Retain;
    pods.digest = String::new();
    pods.suspend = false;
    pods.network = Network {
        allow_from: Vec::new(),
        ..pods.network
    };
    for w in &mut pods.workloads {
        w.replicas = 0;
        w.min_available = None;
    }
    digest_json(&serde_json::to_value(&pods).unwrap_or(Value::Null))
}

/// The first eight hex digits of the digest of a folder's files: the suffix of an immutable
/// ConfigMap's name (`<svc>-agent-<hash8>`).
pub fn files_hash8(files: &BTreeMap<String, String>) -> String {
    let value = serde_json::to_value(files).unwrap_or(Value::Null);
    let digest = digest_json(&value);
    digest
        .strip_prefix("sha256:")
        .unwrap_or(&digest)
        .chars()
        .take(8)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keys_are_sorted_and_whitespace_is_gone() {
        let a = json!({"b": 1, "a": [true, null, "x\"y"], "c": {"z": 1, "y": 2}});
        assert_eq!(
            canonical_json(&a),
            r#"{"a":[true,null,"x\"y"],"b":1,"c":{"y":2,"z":1}}"#
        );
    }

    #[test]
    fn the_digest_is_a_sha256_of_the_canonical_form() {
        // sha256 of `{"a":1}`, computed independently.
        assert_eq!(
            digest_json(&json!({"a": 1})),
            "sha256:015abd7f5cc57a2dd94b7590f04ad8084273905ee33ec5cebeae62276a97f862"
        );
    }

    #[test]
    fn the_key_order_of_the_input_does_not_matter() {
        let one: Value = serde_json::from_str(r#"{"a":1,"b":{"c":2,"d":3}}"#).unwrap();
        let two: Value = serde_json::from_str(r#"{"b":{"d":3,"c":2},"a":1}"#).unwrap();
        assert_eq!(digest_json(&one), digest_json(&two));
    }

    #[test]
    fn hash8_is_eight_hex_digits_and_follows_the_content() {
        let mut files = BTreeMap::new();
        files.insert("instructions.md".to_owned(), "a".to_owned());
        let a = files_hash8(&files);
        assert_eq!(a.len(), 8);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        files.insert("instructions.md".to_owned(), "b".to_owned());
        assert_ne!(a, files_hash8(&files));
    }
}
