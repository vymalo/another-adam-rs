//! The members of the seven components a question may show, as the web's catalog of version 9 defines
//! them: required members, types and limits. A block is checked against this table on every screen,
//! including a screen that cannot draw the component and a thread with no catalog, so a block the
//! web would refuse is refused the same way everywhere, naming the member.
//!
//! A test holds the table equal to the vendored catalog of version 9 (`tests/fixtures/catalog-v9.json`),
//! member by member, so the two cannot drift.

use serde_json::{Map, Value};

/// What a member holds.
pub(super) enum Shape {
    /// Text of `min` to `max` characters (code points).
    Text { min: usize, max: usize },
    /// Text of at most `max` characters that starts with `http://` or `https://`.
    Url { max: usize },
    /// The sha256 of a file: exactly 64 lower-case hexadecimal digits.
    Hash,
    /// One of these words.
    OneOf(&'static [&'static str]),
    /// Any number: how much room a component takes.
    Weight,
    /// A number from `min` to `max`.
    Number { min: i64, max: i64 },
    /// `min` to `max` items of one shape.
    List {
        min: usize,
        max: usize,
        item: &'static Shape,
    },
    /// An object that has these members and no other.
    Object(&'static [Member]),
}

/// A member of a component or of an object inside it.
pub(super) struct Member {
    pub(super) name: &'static str,
    pub(super) shape: Shape,
    pub(super) required: bool,
}

const fn must(name: &'static str, shape: Shape) -> Member {
    Member {
        name,
        shape,
        required: true,
    }
}

const fn may(name: &'static str, shape: Shape) -> Member {
    Member {
        name,
        shape,
        required: false,
    }
}

const fn text(min: usize, max: usize) -> Shape {
    Shape::Text { min, max }
}

const NAME: Shape = text(1, 40);

const TEXT: [Member; 3] = [
    may("weight", Shape::Weight),
    must("text", text(1, 4000)),
    may(
        "variant",
        Shape::OneOf(&["h1", "h2", "h3", "h4", "h5", "caption", "body"]),
    ),
];
const IMAGE: [Member; 4] = [
    may("weight", Shape::Weight),
    must("artifact", Shape::Hash),
    must("alt", text(1, 300)),
    may("caption", text(0, 500)),
];
const MERMAID: [Member; 4] = [
    may("weight", Shape::Weight),
    may("title", text(0, 120)),
    must("code", text(1, 20000)),
    may("caption", text(0, 500)),
];
const CODE: [Member; 4] = [
    may("title", text(0, 120)),
    may("filename", text(0, 200)),
    may("language", text(0, 40)),
    must("code", text(1, 20000)),
];
const CARD: [Member; 5] = [
    must("title", text(1, 200)),
    may("subtitle", text(0, 200)),
    may("body", text(0, 2000)),
    may("url", Shape::Url { max: 2048 }),
    may(
        "tags",
        Shape::List {
            min: 0,
            max: 8,
            item: &NAME,
        },
    ),
];
const CARDS: [Member; 4] = [
    may("weight", Shape::Weight),
    may("title", text(0, 120)),
    may("layout", Shape::OneOf(&["list", "grid"])),
    must(
        "cards",
        Shape::List {
            min: 1,
            max: 24,
            item: &Shape::Object(&CARD),
        },
    ),
];
const CALLOUT: [Member; 3] = [
    may(
        "tone",
        Shape::OneOf(&["info", "success", "warning", "error"]),
    ),
    must("title", text(1, 120)),
    must("text", text(1, 2000)),
];
const SERIES: [Member; 2] = [
    must("name", text(1, 60)),
    must(
        "values",
        Shape::List {
            min: 1,
            max: 24,
            item: &Shape::Number {
                min: -1_000_000_000_000,
                max: 1_000_000_000_000,
            },
        },
    ),
];
const CHART: [Member; 6] = [
    may("title", text(0, 120)),
    may("caption", text(0, 500)),
    must("type", Shape::OneOf(&["bar", "line", "pie"])),
    may("unit", text(0, 20)),
    must(
        "labels",
        Shape::List {
            min: 1,
            max: 24,
            item: &text(1, 40),
        },
    ),
    must(
        "series",
        Shape::List {
            min: 1,
            max: 4,
            item: &Shape::Object(&SERIES),
        },
    ),
];

/// The members of `component`, or `None` for a name a question cannot show.
pub(super) fn members(component: &str) -> Option<&'static [Member]> {
    Some(match component {
        "Text" => &TEXT,
        "Image" => &IMAGE,
        "Mermaid" => &MERMAID,
        "Code" => &CODE,
        "Cards" => &CARDS,
        "Callout" => &CALLOUT,
        "Chart" => &CHART,
        _ => return None,
    })
}

/// `hash` is 64 lower-case hexadecimal digits.
pub(super) fn is_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `props` (the members the model wrote, `component` and `id` apart) are the members of `component`:
/// the problem is said with the member's path in it.
pub(super) fn check(component: &str, props: &Map<String, Value>) -> Result<(), String> {
    let Some(members) = members(component) else {
        return Ok(());
    };
    object("", members, props, &["component", "id"])?;
    if component == "Chart" {
        series_lengths(props)?;
    }
    Ok(())
}

/// Each series has one value for each label: the schema cannot say it.
fn series_lengths(props: &Map<String, Value>) -> Result<(), String> {
    let labels = props
        .get("labels")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    for (n, series) in props
        .get("series")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let values = series
            .get("values")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        if values != labels {
            let name = series.get("name").and_then(Value::as_str).unwrap_or("");
            return Err(format!(
                "`series[{}]` (\"{name}\") has {values} values for {labels} labels: give exactly one value for each label",
                n + 1
            ));
        }
    }
    Ok(())
}

fn at(path: &str, name: &str) -> String {
    if path.is_empty() {
        name.to_owned()
    } else {
        format!("{path}.{name}")
    }
}

fn object(
    path: &str,
    members: &[Member],
    map: &Map<String, Value>,
    skip: &[&str],
) -> Result<(), String> {
    for key in map.keys() {
        if !skip.contains(&key.as_str()) && !members.iter().any(|m| m.name == key) {
            return Err(format!(
                "`{}` is not a member it has (it has {})",
                at(path, key),
                members
                    .iter()
                    .map(|m| format!("`{}`", m.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    for member in members {
        match map.get(member.name) {
            Some(value) => value_of(&at(path, member.name), &member.shape, value)?,
            None if member.required => {
                return Err(format!("`{}` is missing", at(path, member.name)));
            }
            None => {}
        }
    }
    Ok(())
}

fn value_of(path: &str, shape: &Shape, value: &Value) -> Result<(), String> {
    match shape {
        Shape::Text { min, max } => {
            let text = value
                .as_str()
                .ok_or_else(|| format!("`{path}` is not text"))?;
            let n = text.chars().count();
            if n < *min || n > *max {
                let range = if *min == 0 {
                    format!("at most {max}")
                } else {
                    format!("{min} to {max}")
                };
                return Err(format!(
                    "`{path}` has {n} characters; it needs {range} characters"
                ));
            }
            Ok(())
        }
        Shape::Url { max } => {
            let text = value
                .as_str()
                .ok_or_else(|| format!("`{path}` is not text"))?;
            if !(text.starts_with("http://") || text.starts_with("https://")) {
                return Err(format!("`{path}` must start with http:// or https://"));
            }
            if text.chars().count() > *max {
                return Err(format!("`{path}` is longer than {max} characters"));
            }
            Ok(())
        }
        Shape::Hash => match value.as_str() {
            Some(hash) if is_hash(hash) => Ok(()),
            _ => Err(format!(
                "`{path}` is not the sha256 (64 hexadecimal digits) of an image file you shared in this \
                 conversation: never a URL or a path"
            )),
        },
        Shape::OneOf(words) => match value.as_str() {
            Some(word) if words.contains(&word) => Ok(()),
            _ => Err(format!("`{path}` is not one of {}", words.join(", "))),
        },
        Shape::Weight => {
            if value.is_number() {
                Ok(())
            } else {
                Err(format!("`{path}` is not a number"))
            }
        }
        Shape::Number { min, max } => {
            let number = value
                .as_f64()
                .ok_or_else(|| format!("`{path}` is not a number"))?;
            #[allow(clippy::cast_precision_loss)] // the bounds are 10^12, exact in a double
            if number < *min as f64 || number > *max as f64 {
                return Err(format!("`{path}` is outside {min} to {max}"));
            }
            Ok(())
        }
        Shape::List { min, max, item } => {
            let items = value
                .as_array()
                .ok_or_else(|| format!("`{path}` is not a list"))?;
            if items.len() < *min || items.len() > *max {
                return Err(format!(
                    "`{path}` has {} items; it needs {min} to {max}",
                    items.len()
                ));
            }
            items
                .iter()
                .enumerate()
                .try_for_each(|(n, entry)| value_of(&format!("{path}[{}]", n + 1), item, entry))
        }
        Shape::Object(members) => {
            let map = value
                .as_object()
                .ok_or_else(|| format!("`{path}` is not an object"))?;
            object(path, members, map, &[])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The schema a shape stands for, in the form the web's catalog writes it.
    fn schema_of(shape: &Shape) -> Value {
        match shape {
            Shape::Text { min, max } => {
                let mut s = json!({"type": "string", "maxLength": max});
                if *min > 0 {
                    s["minLength"] = json!(min);
                }
                s
            }
            Shape::Url { max } => {
                json!({"type": "string", "maxLength": max, "pattern": "^https?://"})
            }
            Shape::Hash => json!({"type": "string", "pattern": "^[0-9a-f]{64}$"}),
            Shape::OneOf(words) => json!({"enum": words}),
            Shape::Weight => json!({"type": "number"}),
            Shape::Number { min, max } => {
                json!({"type": "number", "minimum": min, "maximum": max})
            }
            Shape::List { min, max, item } => {
                let mut s = json!({"type": "array", "maxItems": max, "items": schema_of(item)});
                if *min > 0 {
                    s["minItems"] = json!(min);
                }
                s
            }
            Shape::Object(members) => object_schema(members),
        }
    }

    fn object_schema(members: &[Member]) -> Value {
        let mut required: Vec<&str> = members
            .iter()
            .filter(|m| m.required)
            .map(|m| m.name)
            .collect();
        required.sort_unstable();
        let properties: Map<String, Value> = members
            .iter()
            .map(|m| (m.name.to_owned(), schema_of(&m.shape)))
            .collect();
        json!({
            "type": "object", "properties": properties, "required": required,
            "additionalProperties": false
        })
    }

    /// The vendored schema of a component without what the table does not hold: the `id` and
    /// `component` members and the descriptions.
    fn vendored(component: &Value) -> Value {
        let mut schema = component.clone();
        strip(&mut schema);
        let properties = schema["properties"].as_object_mut().unwrap();
        properties.remove("id");
        properties.remove("component");
        let required = schema["required"].as_array_mut().unwrap();
        required.retain(|n| n != "id" && n != "component");
        schema
    }

    /// Remove the descriptions at every depth and sort every `required`.
    fn strip(value: &mut Value) {
        match value {
            Value::Object(map) => {
                map.remove("description");
                if let Some(Value::Array(required)) = map.get_mut("required") {
                    required.sort_by_key(|n| n.as_str().unwrap_or("").to_owned());
                }
                map.values_mut().for_each(strip);
            }
            Value::Array(items) => items.iter_mut().for_each(strip),
            _ => {}
        }
    }

    #[test]
    fn the_table_is_the_catalog_of_version_9_member_by_member() {
        let catalog: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/catalog-v9.json")).unwrap();
        for name in super::super::shown::SHOWABLE {
            assert_eq!(
                object_schema(members(name).unwrap()),
                vendored(&catalog["components"][name]),
                "{name} drifted from the catalog of version 9"
            );
        }
    }

    #[test]
    fn a_list_of_components_the_table_holds_is_the_closed_list() {
        for name in super::super::shown::SHOWABLE {
            assert!(members(name).is_some(), "{name}");
        }
        assert!(members("Choices").is_none());
        assert!(members("Table").is_none());
    }

    #[test]
    fn a_hash_is_64_lower_case_hexadecimal_digits() {
        let ok = "a1".repeat(32);
        assert!(is_hash(&ok));
        assert!(!is_hash(&ok[1..]));
        assert!(!is_hash(&format!("{ok}a")));
        assert!(!is_hash(&format!("g{}", &ok[1..])));
        assert!(!is_hash(&ok.to_uppercase()));
    }
}
