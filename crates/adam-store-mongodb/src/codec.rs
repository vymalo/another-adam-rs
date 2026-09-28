//! JSON <-> BSON conversion for run state and journal payloads.
//!
//! State is stored as a real BSON document (not a string) so it can be
//! queried and indexed from MongoDB. Agent state routinely contains keys that
//! MongoDB treats specially: JSON Schema's `$ref`/`$defs`, dotted keys, empty
//! keys, NUL bytes (illegal in BSON field names). Those keys are escaped:
//!
//! * a key is escaped if it is empty, starts with `%`, or contains `.`, `$` or NUL;
//! * an escaped key is `%` followed by the key with `%`, `.`, `$` and NUL
//!   percent-encoded (`%25`, `%2E`, `%24`, `%00`).
//!
//! Escaped keys always start with `%` and unescaped keys never do, so decoding
//! is unambiguous. Ordinary keys (the vast majority) are stored verbatim, so
//! `state.messages.0.role` is still a valid query path.

use adam_core::StoreError;
use mongodb::bson::{Bson, Document};
use serde_json::{Map, Number, Value};

pub fn json_to_bson(value: &Value) -> Result<Bson, StoreError> {
    Ok(match value {
        Value::Null => Bson::Null,
        Value::Bool(b) => Bson::Boolean(*b),
        Value::Number(n) => number_to_bson(n)?,
        Value::String(s) => Bson::String(s.clone()),
        Value::Array(items) => {
            Bson::Array(items.iter().map(json_to_bson).collect::<Result<_, _>>()?)
        }
        Value::Object(map) => {
            let mut doc = Document::new();
            for (k, v) in map {
                doc.insert(encode_key(k), json_to_bson(v)?);
            }
            Bson::Document(doc)
        }
    })
}

pub fn bson_to_json(value: &Bson) -> Result<Value, StoreError> {
    Ok(match value {
        Bson::Null | Bson::Undefined => Value::Null,
        Bson::Boolean(b) => Value::Bool(*b),
        Bson::Int32(i) => Value::from(*i),
        Bson::Int64(i) => Value::from(*i),
        Bson::Double(f) => Number::from_f64(*f).map(Value::Number).ok_or_else(|| {
            StoreError::InvalidData(format!("non-finite number {f} in stored state"))
        })?,
        Bson::String(s) => Value::String(s.clone()),
        Bson::Array(items) => {
            Value::Array(items.iter().map(bson_to_json).collect::<Result<_, _>>()?)
        }
        Bson::Document(doc) => {
            let mut map = Map::new();
            for (k, v) in doc {
                map.insert(decode_key(k)?, bson_to_json(v)?);
            }
            Value::Object(map)
        }
        // Only JSON-shaped values are ever written; anything else was put
        // there by hand. Keep it readable rather than failing the run.
        other => other.clone().into_relaxed_extjson(),
    })
}

fn number_to_bson(n: &Number) -> Result<Bson, StoreError> {
    if let Some(i) = n.as_i64() {
        Ok(Bson::Int64(i))
    } else if n.is_u64() {
        Err(StoreError::InvalidData(format!(
            "integer {n} exceeds i64::MAX and cannot be stored in BSON"
        )))
    } else {
        Ok(Bson::Double(n.as_f64().expect("finite f64 in serde_json")))
    }
}

fn needs_escape(key: &str) -> bool {
    key.is_empty() || key.starts_with('%') || key.contains(['.', '$', '\0'])
}

pub fn encode_key(key: &str) -> String {
    if !needs_escape(key) {
        return key.to_owned();
    }
    let mut out = String::with_capacity(key.len() + 4);
    out.push('%');
    for c in key.chars() {
        match c {
            '%' => out.push_str("%25"),
            '.' => out.push_str("%2E"),
            '$' => out.push_str("%24"),
            '\0' => out.push_str("%00"),
            c => out.push(c),
        }
    }
    out
}

pub fn decode_key(key: &str) -> Result<String, StoreError> {
    let Some(body) = key.strip_prefix('%') else {
        return Ok(key.to_owned());
    };
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(pos) = rest.find('%') {
        out.push_str(&rest[..pos]);
        let code = rest.get(pos + 1..pos + 3);
        out.push(match code {
            Some("25") => '%',
            Some("2E") => '.',
            Some("24") => '$',
            Some("00") => '\0',
            _ => {
                return Err(StoreError::InvalidData(format!(
                    "malformed escaped key {key:?}"
                )));
            }
        });
        rest = &rest[pos + 3..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keys_roundtrip() {
        for key in [
            "",
            "plain",
            "$ref",
            "$",
            ".",
            "a.b",
            "%",
            "%24",
            "%2E",
            "50%",
            "%%",
            "x\0y",
            "trailing.",
            "ünï.cödé$",
            "%25%24",
        ] {
            let enc = encode_key(key);
            assert!(
                !enc.is_empty() && !enc.starts_with('$') && !enc.contains(['.', '\0']),
                "{enc:?}"
            );
            assert_eq!(
                decode_key(&enc).unwrap(),
                key,
                "roundtrip of {key:?} via {enc:?}"
            );
        }
        assert_eq!(encode_key("role"), "role", "ordinary keys stay queryable");
        assert!(decode_key("%a%zz").is_err());
        assert!(decode_key("%a%2").is_err());
    }

    #[test]
    fn values_roundtrip() {
        let v = json!({"$schema": {"a.b": [1, -2, i64::MIN, 0.5, null, true, {"": "x\0y"}]}});
        assert_eq!(bson_to_json(&json_to_bson(&v).unwrap()).unwrap(), v);
        assert!(json_to_bson(&json!(u64::MAX)).is_err());
    }
}
