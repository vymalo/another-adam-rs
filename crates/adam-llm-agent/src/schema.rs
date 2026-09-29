//! Tool argument schemas from `schemars` (feature `schema`).

use adam_model::ToolSpec;
use schemars::{JsonSchema, SchemaGenerator, generate::SchemaSettings};
use serde_json::{Map, Value};

/// The [`ToolSpec`] of a tool whose arguments are `A`: its `parameters` are
/// the JSON Schema of `A`.
///
/// `A` must serialize as an object (a struct; a unit struct with braces for a
/// tool without parameters). The schema is draft 2020-12 with subschemas
/// inlined where possible, no `$schema` and no `title` (a title is noise for
/// the model). Doc comments on `A` and on its fields become `description`s,
/// and `Option<T>` fields are not required: that is what `#[derive(JsonSchema)]`
/// does. A recursive type refers back with `$ref`.
///
/// ```
/// use adam_llm_agent::spec_for;
/// use schemars::JsonSchema;
/// use serde::Deserialize;
///
/// #[derive(Deserialize, JsonSchema)]
/// struct Args {
///     /// What you need to know
///     question: String,
///     /// Give up after this many seconds
///     timeout: Option<u32>,
/// }
///
/// let spec = spec_for::<Args>("ask_user", "Ask the user a question.");
/// assert_eq!(spec.name, "ask_user");
/// assert_eq!(spec.parameters["required"], serde_json::json!(["question"]));
/// assert_eq!(
///     spec.parameters["properties"]["question"]["description"],
///     "What you need to know"
/// );
/// assert!(spec.parameters.get("title").is_none());
/// ```
pub fn spec_for<A: JsonSchema>(
    name: impl Into<String>,
    description: impl Into<String>,
) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: description.into(),
        parameters: parameters_schema::<A>(),
    }
}

/// `ToolSpec::for_args::<A>(name, description)`: [`spec_for`] as an associated
/// function. `ToolSpec` belongs to `adam-model`, so this is an extension
/// trait: import it to call it.
///
/// ```
/// use adam_llm_agent::ToolSpecExt as _;
/// use adam_model::ToolSpec;
/// use schemars::JsonSchema;
/// use serde::Deserialize;
///
/// #[derive(Deserialize, JsonSchema)]
/// struct Args { key: String }
///
/// let spec = ToolSpec::for_args::<Args>("lookup", "Look a key up.");
/// assert_eq!(spec.parameters["type"], "object");
/// ```
pub trait ToolSpecExt {
    /// See [`spec_for`].
    fn for_args<A: JsonSchema>(name: impl Into<String>, description: impl Into<String>)
    -> ToolSpec;
}

impl ToolSpecExt for ToolSpec {
    fn for_args<A: JsonSchema>(
        name: impl Into<String>,
        description: impl Into<String>,
    ) -> ToolSpec {
        spec_for::<A>(name, description)
    }
}

fn generator() -> SchemaGenerator {
    SchemaSettings::draft2020_12()
        .with(|s| {
            s.inline_subschemas = true;
            s.meta_schema = None;
        })
        .into_generator()
}

fn parameters_schema<A: JsonSchema>() -> Value {
    let mut schema = Value::from(generator().into_root_schema_for::<A>());
    strip_titles(&mut schema);
    if let Value::Object(root) = &mut schema
        && root.get("type").and_then(Value::as_str) == Some("object")
    {
        // Some providers reject an object schema without `properties`.
        root.entry("properties")
            .or_insert_with(|| Value::Object(Map::new()));
    }
    schema
}

/// Remove the `title` keyword everywhere a schema can carry one, and nothing
/// else: a *property* called `title` (a key of `properties`) stays, and so
/// does data inside `default`, `const`, `enum` and `examples`.
fn strip_titles(schema: &mut Value) {
    let Value::Object(map) = schema else {
        if let Value::Array(items) = schema {
            items.iter_mut().for_each(strip_titles);
        }
        return;
    };
    if map.get("title").is_some_and(Value::is_string) {
        map.remove("title");
    }
    for (key, value) in map.iter_mut() {
        match key.as_str() {
            "default" | "const" | "enum" | "examples" => {}
            // name -> schema maps: the names are not keywords.
            "properties" | "patternProperties" | "$defs" | "definitions" | "dependentSchemas" => {
                if let Value::Object(named) = value {
                    named.values_mut().for_each(strip_titles);
                }
            }
            _ => strip_titles(value),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use schemars::JsonSchema;
    use serde::Deserialize;
    use serde_json::json;

    use super::*;

    /// A struct with everything the tools use.
    #[derive(Deserialize, JsonSchema)]
    #[allow(dead_code)]
    struct Args {
        /// What you need to know
        question: String,
        /// A property that happens to be called title
        title: Option<String>,
        #[serde(default)]
        tags: Vec<String>,
        mode: Mode,
        inner: Inner,
        map: BTreeMap<String, u8>,
    }

    #[derive(Deserialize, JsonSchema)]
    #[allow(dead_code)]
    enum Mode {
        Fast,
        Slow,
    }

    /// The inner struct
    #[derive(Deserialize, JsonSchema)]
    #[allow(dead_code)]
    struct Inner {
        /// A number
        n: u32,
    }

    #[derive(Deserialize, JsonSchema)]
    struct NoArgs {}

    #[derive(Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    struct Strict {
        a: u8,
    }

    #[derive(Deserialize, JsonSchema)]
    #[allow(dead_code)]
    struct Tree {
        value: u8,
        children: Vec<Tree>,
    }

    #[test]
    fn the_schema_is_an_inline_draft_2020_12_object_without_titles() {
        let spec = spec_for::<Args>("t", "d");
        let p = &spec.parameters;
        assert_eq!(p["type"], "object");
        assert!(p.get("$schema").is_none());
        assert!(p.get("title").is_none(), "{p}");
        assert_eq!(
            p["properties"]["question"]["description"],
            "What you need to know"
        );
        // Inlined: no $ref, no $defs, and the nested struct's own title is gone.
        assert!(p.get("$defs").is_none(), "{p}");
        assert_eq!(p["properties"]["inner"]["type"], "object");
        assert_eq!(p["properties"]["inner"]["description"], "The inner struct");
        assert!(p["properties"]["inner"].get("title").is_none());
        assert_eq!(p["properties"]["mode"]["enum"], json!(["Fast", "Slow"]));
        // `Option` is not required, `#[serde(default)]` neither.
        let required: Vec<&str> = p["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(required.contains(&"question"));
        assert!(!required.contains(&"title"));
        assert!(!required.contains(&"tags"));
    }

    #[test]
    fn a_property_called_title_survives() {
        let p = spec_for::<Args>("t", "d").parameters;
        assert_eq!(
            p["properties"]["title"]["description"],
            "A property that happens to be called title"
        );
    }

    #[test]
    fn title_inside_data_is_left_alone() {
        let mut v = json!({
            "title": "gone",
            "properties": {"title": {"title": "gone too", "type": "string", "default": {"title": "kept"}}},
            "enum": [{"title": "kept"}],
            "items": [{"title": "gone"}],
        });
        strip_titles(&mut v);
        assert_eq!(
            v,
            json!({
                "properties": {"title": {"type": "string", "default": {"title": "kept"}}},
                "enum": [{"title": "kept"}],
                "items": [{}],
            })
        );
    }

    #[test]
    fn a_tool_without_parameters_has_an_empty_properties_object() {
        let p = spec_for::<NoArgs>("t", "d").parameters;
        assert_eq!(p["type"], "object");
        assert_eq!(p["properties"], json!({}));
    }

    #[test]
    fn deny_unknown_fields_closes_the_object() {
        let p = spec_for::<Strict>("t", "d").parameters;
        assert_eq!(p["additionalProperties"], json!(false));
    }

    #[test]
    fn a_recursive_type_refers_back_to_the_root() {
        let p = spec_for::<Tree>("t", "d").parameters;
        assert_eq!(p["properties"]["children"]["items"]["$ref"], "#", "{p}");
    }

    #[test]
    fn for_args_is_spec_for() {
        let a = <ToolSpec as ToolSpecExt>::for_args::<Args>("t", "d");
        assert_eq!(a, spec_for::<Args>("t", "d"));
        assert_eq!((a.name.as_str(), a.description.as_str()), ("t", "d"));
    }
}
