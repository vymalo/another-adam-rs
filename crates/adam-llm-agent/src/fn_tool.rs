//! [`FnTool`]: a tool defined at run time from a closure.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use adam_model::ToolSpec;
use async_trait::async_trait;
use serde_json::Value;

use crate::tool::{Tool, ToolCtx, ToolError, ToolOutput};
use crate::typed::IntoToolResult;

type Handler = dyn Fn(ToolCtx, Value) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send>>
    + Send
    + Sync;

/// A [`Tool`] made from a name, a description, an argument schema and a
/// closure: for tools whose shape is known only at run time (a tool discovered
/// from another service, say) or that are too small to deserve a type.
///
/// The closure gets its own [`ToolCtx`] (cheap to clone) and returns anything
/// that is [`IntoToolResult`].
///
/// ```
/// use adam_llm_agent::{FnTool, ToolError};
/// use serde_json::json;
///
/// let lookup = FnTool::raw(
///     "lookup",
///     "Look a key up.",
///     json!({"type": "object", "properties": {"key": {"type": "string"}}, "required": ["key"]}),
///     |_ctx, args| async move {
///         Ok::<_, ToolError>(format!("no value for {}", args["key"]))
///     },
/// );
/// ```
///
/// With the `schema` feature the arguments can be a typed struct instead, see
/// [`FnTool::builder`].
#[derive(Clone)]
pub struct FnTool {
    spec: ToolSpec,
    handler: Arc<Handler>,
    asks_user: bool,
}

impl FnTool {
    /// A tool whose `parameters` schema is given as JSON and whose closure
    /// receives the model's argument object unvalidated (like
    /// [`Tool::call`]).
    pub fn raw<F, Fut, R>(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        handler: F,
    ) -> Self
    where
        F: Fn(ToolCtx, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + Send + 'static,
        R: IntoToolResult,
    {
        Self {
            spec: ToolSpec {
                name: name.into(),
                description: description.into(),
                parameters,
            },
            handler: Arc::new(move |ctx, args| {
                let fut = handler(ctx, args);
                Box::pin(async move { fut.await.into_tool_result() })
            }),
            asks_user: false,
        }
    }

    /// Declare that the tool can end a call with [`ToolError::NeedsInput`], as
    /// [`Tool::asks_user`] says.
    #[must_use]
    pub fn asking_user(mut self) -> Self {
        self.asks_user = true;
        self
    }

    /// Start a tool whose arguments are a typed struct; needs the `schema`
    /// feature. See [`FnToolBuilder`].
    #[cfg(feature = "schema")]
    pub fn builder(name: impl Into<String>) -> FnToolBuilder {
        FnToolBuilder {
            name: name.into(),
            description: String::new(),
        }
    }
}

impl fmt::Debug for FnTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FnTool")
            .field("name", &self.spec.name)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Tool for FnTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn asks_user(&self) -> bool {
        self.asks_user
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        (self.handler)(ctx.clone(), args).await
    }
}

/// Builds a typed [`FnTool`]: `FnTool::builder("echo").description("...")
/// .args::<EchoArgs>().handler(|ctx, args| async move { ... })`.
#[cfg(feature = "schema")]
#[derive(Debug, Clone)]
pub struct FnToolBuilder {
    name: String,
    description: String,
}

#[cfg(feature = "schema")]
impl FnToolBuilder {
    /// What the tool does, for the model.
    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// The arguments are `A`: the schema comes from its `JsonSchema` (see
    /// [`spec_for`](crate::spec_for)) and the model's JSON is read into it
    /// with [`parse_args`](crate::parse_args), so a mistake reaches the model
    /// as an error output and never reaches the closure.
    pub fn args<A>(self) -> TypedFnToolBuilder<A>
    where
        A: serde::de::DeserializeOwned + schemars::JsonSchema + Send + 'static,
    {
        TypedFnToolBuilder {
            spec: crate::schema::spec_for::<A>(self.name, self.description),
            args: std::marker::PhantomData,
        }
    }
}

/// The second half of [`FnToolBuilder`]: the arguments are known, the closure
/// is not.
#[cfg(feature = "schema")]
pub struct TypedFnToolBuilder<A> {
    spec: ToolSpec,
    args: std::marker::PhantomData<fn() -> A>,
}

#[cfg(feature = "schema")]
impl<A> TypedFnToolBuilder<A>
where
    A: serde::de::DeserializeOwned + Send + 'static,
{
    /// The tool's closure.
    ///
    /// ```
    /// use adam_llm_agent::{FnTool, ToolError};
    /// use schemars::JsonSchema;
    /// use serde::Deserialize;
    ///
    /// #[derive(Deserialize, JsonSchema)]
    /// struct EchoArgs {
    ///     /// The text to echo
    ///     text: String,
    /// }
    ///
    /// let echo = FnTool::builder("echo")
    ///     .description("Echo the text back.")
    ///     .args::<EchoArgs>()
    ///     .handler(|_ctx, a| async move { Ok::<_, ToolError>(a.text) });
    /// ```
    pub fn handler<F, Fut, R>(self, handler: F) -> FnTool
    where
        F: Fn(ToolCtx, A) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + Send + 'static,
        R: IntoToolResult,
    {
        let name = self.spec.name.clone();
        let handler = Arc::new(handler);
        FnTool {
            spec: self.spec,
            handler: Arc::new(
                move |ctx, args| match crate::typed::parse_args::<A>(&name, args) {
                    Ok(args) => {
                        let fut = handler(ctx, args);
                        Box::pin(async move { fut.await.into_tool_result() })
                    }
                    Err(refusal) => Box::pin(async move { Ok(refusal) }),
                },
            ),
            asks_user: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn tool() -> FnTool {
        FnTool::raw(
            "ask",
            "Ask.",
            json!({"type": "object"}),
            |_ctx, _args| async move { Ok::<_, ToolError>("answer") },
        )
    }

    #[test]
    fn a_tool_does_not_ask_the_user_unless_it_says_so() {
        assert!(!tool().asks_user());
        assert!(tool().asking_user().asks_user());
    }
}
