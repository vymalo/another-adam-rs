//! The JSON Canonicalization Scheme, RFC 8785 (JCS): one byte string for a JSON value, whatever
//! form it was read from.
//!
//! What a signature covers is the canonical form of a value, so the signer and every verifier must
//! make the same bytes: object members sorted by the UTF-16 code units of their names, no
//! insignificant whitespace, strings escaped minimally, numbers as ECMAScript's `Number::toString`
//! prints them (via `ryu-js`). An agent card's signature (`adam-a2a`, A2A §8.4) and a turn's
//! ([ADR 0068], [`turn`](crate::turn)) are made over it.
//!
//! A number is read as `serde_json` holds it: an integer of at most 2^53 in magnitude prints as an
//! integer, any other number as the double it rounds to, so a store that gives an integer back as a
//! double (or the reverse) gives the same bytes.
//!
//! [ADR 0068]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0068-adam-signs-its-own-history.md

use serde_json::Value;

/// RFC 8785 (JCS) of `value`.
pub fn canonicalize(value: &Value) -> Vec<u8> {
    let mut out = String::new();
    write_value(value, &mut out);
    out.into_bytes()
}

fn write_value(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => write_number(n, out),
        Value::String(s) => write_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut members: Vec<(&String, &Value)> = map.iter().collect();
            // RFC 8785 §3.2.3: sorted by the UTF-16 code units of the names.
            members.sort_by(|(a, _), (b, _)| a.encode_utf16().cmp(b.encode_utf16()));
            out.push('{');
            for (i, (k, v)) in members.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(k, out);
                out.push(':');
                write_value(v, out);
            }
            out.push('}');
        }
    }
}

/// RFC 8785 §3.2.2.2: only `"` and `\` and the control characters are escaped, the control
/// characters with the short forms where JSON has them and `\u00xx` (lowercase) otherwise.
fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{9}' => out.push_str("\\t"),
            '\u{a}' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\u{d}' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// RFC 8785 §3.2.2.3: numbers as ECMAScript's `Number::toString` prints them (via `ryu-js`).
fn write_number(n: &serde_json::Number, out: &mut String) {
    const EXACT: u64 = 1 << 53;
    if let Some(i) = n.as_i64()
        && i.unsigned_abs() <= EXACT
    {
        out.push_str(&i.to_string());
    } else if let Some(u) = n.as_u64()
        && u <= EXACT
    {
        out.push_str(&u.to_string());
    } else if let Some(f) = n.as_f64() {
        if f == 0.0 {
            out.push('0'); // -0 prints as 0
        } else {
            out.push_str(ryu_js::Buffer::new().format_finite(f));
        }
    } else {
        out.push_str(&n.to_string());
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn canon(v: &Value) -> String {
        String::from_utf8(canonicalize(v)).unwrap_or_default()
    }

    /// The ordering rule of RFC 8785 §3.2.3 and number formatting of §3.2.2.3 (the expected
    /// strings are what ECMAScript prints for these numbers).
    #[test]
    fn jcs_orders_members_by_utf16_and_prints_numbers_as_ecmascript_does() {
        // §3.2.3: names sort by UTF-16 code units, so the emoji (a surrogate pair, 0xD83D first)
        // sorts before U+FF5E, where a sort by code point would put it after.
        let v: Value =
            serde_json::from_str("{\"\u{ff5e}\":1,\"😀\":2,\"a\":3,\"\\r\":4,\"1\":5}").unwrap();
        assert_eq!(
            canon(&v),
            "{\"\\r\":4,\"1\":5,\"a\":3,\"😀\":2,\"\u{ff5e}\":1}"
        );
        let v: Value = serde_json::from_str(
            "[1e30, 4.5, 2e-3, 0.000001, 1e-7, 100, -0.0, 1.5e300, 9007199254740993]",
        )
        .unwrap();
        assert_eq!(
            canon(&v),
            "[1e+30,4.5,0.002,0.000001,1e-7,100,0,1.5e+300,9007199254740992]"
        );
        assert_eq!(
            canon(&json!("a\u{7f}\u{1}\"\\/\u{8}")),
            "\"a\u{7f}\\u0001\\\"\\\\/\\b\""
        );
        assert_eq!(
            canon(&json!({"b": [true, null, {"y": 1, "x": 2}], "a": "é"})),
            "{\"a\":\"é\",\"b\":[true,null,{\"x\":2,\"y\":1}]}"
        );
    }

    /// An integer and the double it equals give the same bytes, and so do the integers past 2^53
    /// and the doubles they round to: a store may give back either.
    #[test]
    fn an_integer_and_its_double_canonicalize_alike() {
        assert_eq!(canon(&json!(1)), canon(&json!(1.0)));
        assert_eq!(canon(&json!(-7)), canon(&json!(-7.0)));
        assert_eq!(canon(&json!(i64::MAX)), canon(&json!(i64::MAX as f64)));
        assert_eq!(canon(&json!(i64::MIN)), "-9223372036854776000");
        assert_eq!(canon(&json!(u64::MAX)), "18446744073709552000");
    }
}
