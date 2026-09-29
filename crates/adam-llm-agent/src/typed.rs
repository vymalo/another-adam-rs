//! Typed tool helpers: what a tool function may return, and how its
//! arguments are read.
//!
//! [`IntoToolOutput`] and [`IntoToolResult`] let a tool return a `String`, a
//! [`Json`] value or a `Result` of either instead of building a
//! [`ToolOutput`] by hand; [`parse_args`] reads the model's argument object
//! and turns a mistake into a [`ToolOutput::error`] the model can act on.
//! The `#[tool]` macro of the `adam` crate generates calls to these; they are
//! useful by hand too.

use std::borrow::Cow;
use std::ops::{Deref, DerefMut};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::tool::{ToolError, ToolOutput};

/// A value a tool can return as its output.
///
/// | Type | The model sees |
/// |---|---|
/// | [`ToolOutput`] | itself (text, error flag, artifacts) |
/// | `String`, `&'static str`, `Cow<'static, str>` | that text |
/// | [`serde_json::Value`] | its compact JSON text (a string is quoted) |
/// | [`Json<T>`] | `T` as compact JSON text |
///
/// ```
/// use adam_llm_agent::{IntoToolOutput, Json, ToolOutput};
///
/// assert_eq!("12:00".into_tool_output(), ToolOutput::text("12:00"));
/// assert_eq!(Json([1, 2]).into_tool_output(), ToolOutput::text("[1,2]"));
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be returned from a tool",
    label = "not a tool output",
    note = "return `ToolOutput`, `String`, `&'static str`, `serde_json::Value` or `Json<T>`, or a `Result` of one of them with an error that is `Into<ToolError>`"
)]
pub trait IntoToolOutput {
    /// The output the model sees.
    fn into_tool_output(self) -> ToolOutput;
}

impl IntoToolOutput for ToolOutput {
    fn into_tool_output(self) -> ToolOutput {
        self
    }
}

impl IntoToolOutput for String {
    fn into_tool_output(self) -> ToolOutput {
        ToolOutput::text(self)
    }
}

impl IntoToolOutput for &'static str {
    fn into_tool_output(self) -> ToolOutput {
        ToolOutput::text(self)
    }
}

impl IntoToolOutput for Cow<'static, str> {
    fn into_tool_output(self) -> ToolOutput {
        ToolOutput::text(self)
    }
}

impl IntoToolOutput for Value {
    fn into_tool_output(self) -> ToolOutput {
        ToolOutput::text(self.to_string())
    }
}

/// Returns `T` to the model as compact JSON text.
///
/// A value that cannot be serialized (a map with non-string keys, say) becomes
/// a [`ToolOutput::error`] saying so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Json<T>(pub T);

impl<T> Deref for Json<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> DerefMut for Json<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

impl<T: Serialize> IntoToolOutput for Json<T> {
    fn into_tool_output(self) -> ToolOutput {
        match serde_json::to_string(&self.0) {
            Ok(text) => ToolOutput::text(text),
            Err(e) => ToolOutput::error(format!("the result could not be serialized: {e}")),
        }
    }
}

/// What a tool function returns, as the [`Tool::call`](crate::Tool::call)
/// result: any [`IntoToolOutput`], or a `Result` of one with an error that is
/// `Into<ToolError>`.
///
/// ```
/// use adam_llm_agent::{IntoToolResult, ToolError, ToolOutput};
///
/// assert_eq!("ok".into_tool_result(), Ok(ToolOutput::text("ok")));
/// let failed: Result<String, ToolError> = Err(ToolError::Permanent("no".into()));
/// assert_eq!(failed.into_tool_result(), Err(ToolError::Permanent("no".into())));
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be returned from a tool",
    label = "not a tool result",
    note = "return `Result<T, E>` where `T: IntoToolOutput` and `E: Into<ToolError>`, or a bare `T: IntoToolOutput`"
)]
pub trait IntoToolResult {
    /// The result handed to the agent loop.
    fn into_tool_result(self) -> Result<ToolOutput, ToolError>;
}

impl<T: IntoToolOutput> IntoToolResult for T {
    fn into_tool_result(self) -> Result<ToolOutput, ToolError> {
        Ok(self.into_tool_output())
    }
}

impl<T: IntoToolOutput, E: Into<ToolError>> IntoToolResult for Result<T, E> {
    fn into_tool_result(self) -> Result<ToolOutput, ToolError> {
        self.map(IntoToolOutput::into_tool_output)
            .map_err(Into::into)
    }
}

/// Read the model's argument object into `A`.
///
/// The model's JSON is not trusted to match the schema, so a mismatch is not
/// an error of the run: it is a [`ToolOutput::error`] naming the tool and the
/// problem (`invalid arguments for `ask_user`: missing field `question``),
/// which the model reads and corrects. Return it as the tool's output.
///
/// `null` is read as an empty object, because models often send it for a tool
/// without parameters. Never panics.
///
/// ```
/// use adam_llm_agent::parse_args;
/// use serde::Deserialize;
/// use serde_json::json;
///
/// #[derive(Debug, Deserialize)]
/// struct Args { question: String }
///
/// let args: Args = parse_args("ask_user", json!({"question": "why?"})).unwrap();
/// assert_eq!(args.question, "why?");
///
/// let refusal = parse_args::<Args>("ask_user", json!({})).unwrap_err();
/// assert!(refusal.is_error);
/// assert!(refusal.content.contains("missing field `question`"));
/// ```
pub fn parse_args<A: DeserializeOwned>(tool: &str, args: Value) -> Result<A, ToolOutput> {
    let args = if args.is_null() {
        Value::Object(serde_json::Map::new())
    } else {
        args
    };
    serde_json::from_value(args)
        .map_err(|e| ToolOutput::error(format!("invalid arguments for `{tool}`: {e}")))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use proptest::prelude::*;
    use serde::Deserialize;
    use serde_json::json;

    use super::*;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Args {
        name: String,
        count: Option<u32>,
        #[serde(default)]
        tags: Vec<String>,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct NoArgs {}

    #[test]
    fn valid_arguments_parse() {
        let args: Args = parse_args("t", json!({"name": "a", "count": 2, "tags": ["x"]})).unwrap();
        assert_eq!(
            args,
            Args {
                name: "a".into(),
                count: Some(2),
                tags: vec!["x".into()]
            }
        );
        let args: Args = parse_args("t", json!({"name": "a"})).unwrap();
        assert_eq!(args.count, None);
    }

    #[test]
    fn a_mistake_is_an_error_output_naming_the_tool_and_the_problem() {
        let out = parse_args::<Args>("greet", json!({"count": "many"})).unwrap_err();
        assert!(out.is_error);
        assert!(out.content.starts_with("invalid arguments for `greet`: "));
        let out = parse_args::<Args>("greet", json!({"name": 3})).unwrap_err();
        assert!(
            out.content.contains("invalid type: integer `3`"),
            "{}",
            out.content
        );
        let out = parse_args::<Args>("greet", json!("nope")).unwrap_err();
        assert!(out.is_error);
    }

    #[test]
    fn null_means_no_arguments() {
        assert_eq!(parse_args::<NoArgs>("t", Value::Null).unwrap(), NoArgs {});
        // A tool that needs arguments still says what is missing.
        let out = parse_args::<Args>("t", Value::Null).unwrap_err();
        assert!(out.content.contains("missing field `name`"));
    }

    #[test]
    fn outputs_convert() {
        assert_eq!("a".into_tool_output(), ToolOutput::text("a"));
        assert_eq!(String::from("b").into_tool_output(), ToolOutput::text("b"));
        assert_eq!(Cow::Borrowed("c").into_tool_output(), ToolOutput::text("c"));
        assert_eq!(
            ToolOutput::error("d").into_tool_output(),
            ToolOutput::error("d")
        );
        assert_eq!(
            json!({"a": 1}).into_tool_output(),
            ToolOutput::text(r#"{"a":1}"#)
        );
        assert_eq!(json!("s").into_tool_output(), ToolOutput::text(r#""s""#));
        let mut wrapped = Json(vec![1, 2]);
        wrapped.push(3);
        assert_eq!(wrapped.len(), 3);
        assert_eq!(wrapped.into_tool_output(), ToolOutput::text("[1,2,3]"));
    }

    #[test]
    fn json_that_cannot_be_serialized_is_an_error_output() {
        let mut map = BTreeMap::new();
        map.insert((1, 2), "tuple keys are not JSON keys");
        let out = Json(map).into_tool_output();
        assert!(out.is_error);
        assert!(
            out.content
                .starts_with("the result could not be serialized")
        );
    }

    #[test]
    fn results_convert_and_errors_pass_through() {
        assert_eq!(
            "x".into_tool_result(),
            Ok::<_, ToolError>(ToolOutput::text("x"))
        );
        let ok: Result<String, ToolError> = Ok("y".into());
        assert_eq!(ok.into_tool_result(), Ok(ToolOutput::text("y")));
        let needs: Result<&'static str, ToolError> = Err(ToolError::NeedsInput {
            question: "q".into(),
        });
        assert_eq!(
            needs.into_tool_result(),
            Err(ToolError::NeedsInput {
                question: "q".into()
            })
        );
        // Any error with `Into<ToolError>` works.
        struct MyErr;
        impl From<MyErr> for ToolError {
            fn from(_: MyErr) -> Self {
                ToolError::Transient("mine".into())
            }
        }
        let mine: Result<String, MyErr> = Err(MyErr);
        assert_eq!(
            mine.into_tool_result(),
            Err(ToolError::Transient("mine".into()))
        );
    }

    fn json_value() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::from),
            any::<i64>().prop_map(Value::from),
            any::<u64>().prop_map(Value::from),
            any::<f64>().prop_map(Value::from),
            ".{0,12}".prop_map(Value::from),
        ];
        leaf.prop_recursive(4, 48, 6, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
                prop::collection::btree_map("name|count|tags|extra|.{0,6}", inner, 0..6)
                    .prop_map(|m| Value::Object(m.into_iter().collect())),
            ]
        })
    }

    proptest! {
        /// Whatever the model sends, `parse_args` returns a value or an error
        /// output for the model, and never panics.
        #[test]
        fn arbitrary_json_never_panics_and_fails_only_as_a_tool_error(value in json_value()) {
            match parse_args::<Args>("t", value) {
                Ok(_) => {}
                Err(out) => {
                    prop_assert!(out.is_error);
                    prop_assert!(out.content.starts_with("invalid arguments for `t`: "));
                    prop_assert!(out.artifacts.is_empty());
                }
            }
        }

        /// Well-formed arguments always parse back to what was sent.
        #[test]
        fn well_formed_arguments_round_trip(
            name in ".{0,20}",
            count in proptest::option::of(any::<u32>()),
            tags in prop::collection::vec(".{0,5}", 0..4),
        ) {
            let sent = json!({"name": name, "count": count, "tags": tags});
            let args: Args = parse_args("t", sent).unwrap();
            prop_assert_eq!(args, Args { name, count, tags });
        }
    }
}
