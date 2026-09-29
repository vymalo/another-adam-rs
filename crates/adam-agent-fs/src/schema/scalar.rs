//! Small serde helpers shared by the schemas: lists written as strings, scalars read as text.

use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;
use serde::de::{self, Deserializer, Visitor};
use serde_json::Value;

/// A YAML scalar read as text: `1.0` and `true` are strings here, so `metadata: { v: 1.0 }`
/// works although the Agent Skills spec says "string to string". A null is the empty string.
struct Scalar(String);

impl<'de> Deserialize<'de> for Scalar {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = Scalar;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a string, number or boolean")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Scalar, E> {
                Ok(Scalar(v.to_owned()))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Scalar, E> {
                Ok(Scalar(v))
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Scalar, E> {
                Ok(Scalar(v.to_string()))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Scalar, E> {
                Ok(Scalar(v.to_string()))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Scalar, E> {
                Ok(Scalar(v.to_string()))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Scalar, E> {
                Ok(Scalar(v.to_string()))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Scalar, E> {
                Ok(Scalar(String::new()))
            }
            fn visit_none<E: de::Error>(self) -> Result<Scalar, E> {
                Ok(Scalar(String::new()))
            }
        }
        d.deserialize_any(V)
    }
}

/// `deserialize_with` for a `BTreeMap<String, String>` whose values may be any scalar. A null
/// (`vars:` with nothing after it) is an empty map.
pub(super) fn scalar_map<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<String, String>, D::Error> {
    let raw = Option::<BTreeMap<String, Scalar>>::deserialize(d)?;
    Ok(raw
        .unwrap_or_default()
        .into_iter()
        .map(|(k, v)| (k, v.0))
        .collect())
}

/// `deserialize_with` for a free-form `metadata` map. The Agent Skills spec says string to
/// string; skills in the wild put lists and maps there (`sources: [..]`), and the client guide
/// asks for leniency, so a value that is not a scalar is kept as its JSON text.
pub(super) fn lenient_map<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<String, String>, D::Error> {
    let raw = Option::<BTreeMap<String, Value>>::deserialize(d)?;
    Ok(raw
        .unwrap_or_default()
        .into_iter()
        .map(|(k, v)| {
            let text = match v {
                Value::String(s) => s,
                Value::Null => String::new(),
                other => other.to_string(),
            };
            (k, text)
        })
        .collect())
}

/// A list written either as a YAML sequence or as one string.
pub(super) enum StringOrSeq {
    Text(String),
    Seq(Vec<String>),
}

impl<'de> Deserialize<'de> for StringOrSeq {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = StringOrSeq;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a string or a list of strings")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<StringOrSeq, E> {
                Ok(StringOrSeq::Text(v.to_owned()))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<StringOrSeq, E> {
                Ok(StringOrSeq::Text(v))
            }
            fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<StringOrSeq, A::Error> {
                let mut out = Vec::new();
                while let Some(Scalar(s)) = seq.next_element()? {
                    out.push(s);
                }
                Ok(StringOrSeq::Seq(out))
            }
        }
        d.deserialize_any(V)
    }
}

impl StringOrSeq {
    /// A string is a comma-separated list (Claude Code and Copilot `tools`); a sequence is
    /// taken item by item. Items are trimmed and empty ones dropped.
    pub(super) fn into_comma_list(self) -> Vec<String> {
        let items: Vec<String> = match self {
            Self::Text(t) => t.split(',').map(str::to_owned).collect(),
            Self::Seq(v) => v,
        };
        items
            .into_iter()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// A string is space-separated (Agent Skills `allowed-tools`), except inside parentheses:
    /// `Bash(git add:*) Read` is two entries. A sequence is taken item by item.
    pub(super) fn into_space_list(self) -> Vec<String> {
        match self {
            Self::Text(t) => split_outside_parens(&t),
            Self::Seq(v) => v
                .into_iter()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect(),
        }
    }
}

/// Split on whitespace that is not inside parentheses.
fn split_outside_parens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0_u32;
    for c in text.chars() {
        match c {
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                cur.push(c);
            }
            c if c.is_whitespace() && depth == 0 => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// `deserialize_with` for `Option<Vec<String>>` written as a space-separated string or a list.
pub(super) fn space_list<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    Ok(Option::<StringOrSeq>::deserialize(d)?
        .map(StringOrSeq::into_space_list)
        .unwrap_or_default())
}
