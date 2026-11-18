//! `after_previous`: sync on demand (ADR 0049 decision 12).
//!
//! When the agent runs the calls of a turn at the same time (a limit above 1), every tool's schema
//! gets one more optional boolean property, [`AFTER_PREVIOUS`]. A call that sets it to `true` is a
//! barrier: it starts when every call before it in the reply has finished and been applied, and
//! nothing after it starts before it is done. The argument is the loop's: it is taken out of the
//! call before the tool, [`Tool::access`](crate::Tool::access), the doom-loop key, the hooks and the
//! step's input see it, so a `deny_unknown_fields` tool is unaffected. The history keeps what the
//! model said.
//!
//! A tool whose own schema already has a property of that name keeps it: nothing is injected for it
//! and nothing is stripped from its calls.

use adam_model::ToolSpec;
use serde_json::{Value, json};

/// The reserved argument name.
pub const AFTER_PREVIOUS: &str = "after_previous";

/// What the instructions say about [`AFTER_PREVIOUS`]: the sentence appended to them (after a blank
/// line) when the limit is above 1. Exported as `AFTER_PREVIOUS_INSTRUCTION`, so a test can tell it from
/// the agent's own words.
pub const INSTRUCTION: &str = "The tool calls of one reply run at the same time; give a call \
    `\"after_previous\": true` when it must start only after every call before it in the reply has \
    finished.";

/// The description of the injected property.
const DESCRIPTION: &str =
    "Set true to run this call only after every earlier call of this reply has finished.";

/// Whether the top level of `parameters` is an object schema we can add a property to: `type` is
/// `"object"`, or there is no `type` and `properties` is an object.
fn is_object_schema(parameters: &Value) -> bool {
    match parameters.get("type") {
        Some(Value::String(kind)) => kind == "object",
        Some(_) => false,
        None => parameters.get("properties").is_some_and(Value::is_object),
    }
}

/// Whether the tool's own schema already has a property called [`AFTER_PREVIOUS`].
pub(crate) fn has_own(spec: &ToolSpec) -> bool {
    spec.parameters
        .get("properties")
        .and_then(Value::as_object)
        .is_some_and(|properties| properties.contains_key(AFTER_PREVIOUS))
}

/// Give every tool of `specs` the optional property (when `inject`) and return the names of the tools
/// whose own schema has one, which are left as they are. A schema that is not an object is left alone
/// too: that tool is ordered by its class only.
pub(crate) fn prepare(specs: &mut [ToolSpec], inject: bool) -> Vec<String> {
    let mut own = Vec::new();
    for spec in specs {
        if has_own(spec) {
            own.push(spec.name.clone());
            continue;
        }
        if !inject || !is_object_schema(&spec.parameters) {
            continue;
        }
        if let Some(parameters) = spec.parameters.as_object_mut() {
            let properties = parameters.entry("properties").or_insert_with(|| json!({}));
            if let Some(properties) = properties.as_object_mut() {
                properties.insert(
                    AFTER_PREVIOUS.to_owned(),
                    json!({"type": "boolean", "description": DESCRIPTION}),
                );
            }
        }
    }
    own
}

/// What a call said about its order.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Order {
    /// The call is as the tool takes it: no such argument (or `null`, or `false`), or the tool's own.
    Plain,
    /// `true`: the call starts after every call before it.
    After,
    /// Not a boolean: the call does not run. The value, as text, for the message.
    Bad(String),
}

/// Take [`AFTER_PREVIOUS`] out of `arguments` (an object) and say what it asked. `null` counts as
/// absent. Arguments that are not an object are left alone.
pub(crate) fn take(arguments: &mut Value) -> Order {
    let Some(object) = arguments.as_object_mut() else {
        return Order::Plain;
    };
    match object.remove(AFTER_PREVIOUS) {
        None | Some(Value::Null | Value::Bool(false)) => Order::Plain,
        Some(Value::Bool(true)) => Order::After,
        Some(other) => {
            let mut text = other.to_string();
            if text.chars().count() > 80 {
                text = text.chars().take(80).collect::<String>() + "...";
            }
            Order::Bad(text)
        }
    }
}

/// What the model is told when a call's [`AFTER_PREVIOUS`] is not a boolean.
pub(crate) fn bad_value(tool: &str, value: &str) -> String {
    format!(
        "`{AFTER_PREVIOUS}` of `{tool}` must be true or false, not {value}. The call did not run."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(parameters: Value) -> ToolSpec {
        ToolSpec {
            name: "t".into(),
            description: "d".into(),
            parameters,
        }
    }

    #[test]
    fn an_object_schema_gets_an_optional_boolean_and_keeps_everything_else() {
        let mut specs = [spec(json!({
            "type": "object",
            "properties": {"x": {"type": "integer"}},
            "required": ["x"],
            "additionalProperties": false
        }))];
        assert!(prepare(&mut specs, true).is_empty());
        let p = &specs[0].parameters;
        assert_eq!(p["properties"]["after_previous"]["type"], "boolean");
        assert_eq!(p["properties"]["x"], json!({"type": "integer"}));
        assert_eq!(p["required"], json!(["x"]));
        assert_eq!(p["additionalProperties"], json!(false));
    }

    #[test]
    fn a_schema_without_properties_gets_them_and_other_shapes_are_left_alone() {
        let mut specs = [
            spec(json!({"type": "object"})),
            spec(json!({"properties": {}})),
            spec(json!({"type": "string"})),
            spec(json!({})),
            spec(json!({"type": ["object", "null"]})),
            spec(json!("odd")),
        ];
        assert!(prepare(&mut specs, true).is_empty());
        assert_eq!(
            specs[0].parameters["properties"]["after_previous"]["type"],
            "boolean"
        );
        assert_eq!(
            specs[1].parameters["properties"]["after_previous"]["type"],
            "boolean"
        );
        assert_eq!(specs[2].parameters, json!({"type": "string"}));
        assert_eq!(specs[3].parameters, json!({}));
        assert_eq!(specs[4].parameters, json!({"type": ["object", "null"]}));
        assert_eq!(specs[5].parameters, json!("odd"));
    }

    #[test]
    fn nothing_is_injected_when_asked_not_to_and_own_properties_are_reported() {
        let mut specs = [
            spec(json!({"type": "object"})),
            ToolSpec {
                name: "mine".into(),
                description: "d".into(),
                parameters: json!({"type": "object", "properties": {"after_previous": {"type": "string"}}}),
            },
        ];
        assert_eq!(prepare(&mut specs, false), ["mine"]);
        assert_eq!(specs[0].parameters, json!({"type": "object"}));
        assert_eq!(prepare(&mut specs, true), ["mine"]);
        assert_eq!(
            specs[1].parameters["properties"]["after_previous"],
            json!({"type": "string"})
        );
    }

    #[test]
    fn take_reads_true_false_null_and_refuses_the_rest() {
        let mut a = json!({"k": 1, "after_previous": true});
        assert_eq!(take(&mut a), Order::After);
        assert_eq!(a, json!({"k": 1}));
        for plain in [json!(false), json!(null)] {
            let mut a = json!({"after_previous": plain});
            assert_eq!(take(&mut a), Order::Plain);
            assert_eq!(a, json!({}));
        }
        let mut none = json!({"k": 1});
        assert_eq!(take(&mut none), Order::Plain);
        for bad in [json!("yes"), json!(1), json!([true]), json!({})] {
            let mut a = json!({"after_previous": bad});
            assert!(matches!(take(&mut a), Order::Bad(_)), "{bad}");
            assert_eq!(a, json!({}));
        }
        let mut not_an_object = json!("x");
        assert_eq!(take(&mut not_an_object), Order::Plain);
    }

    #[test]
    fn a_long_bad_value_is_cut_in_the_message() {
        let mut a = json!({"after_previous": "y".repeat(500)});
        let Order::Bad(text) = take(&mut a) else {
            panic!("a string is bad")
        };
        assert!(text.chars().count() < 100, "{}", text.len());
    }
}
