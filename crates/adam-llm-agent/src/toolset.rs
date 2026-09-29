//! [`ToolSet`] and the [`tools!`](crate::tools) macro.

use crate::tool::{DynTool, Tool};

/// An ordered group of tools, built once and handed to an agent.
///
/// Order is registration order and is what the model is shown. A set does
/// not judge names: two tools with the same name are both kept, and
/// [`LlmAgentBuilder::try_build`](crate::LlmAgentBuilder::try_build) is what
/// rejects them.
///
/// ```
/// use adam_llm_agent::{ToolCtx, ToolError, ToolOutput, ToolSet, Tool, tools};
/// use adam_model::ToolSpec;
/// use async_trait::async_trait;
/// use serde_json::{Value, json};
///
/// struct Clock;
///
/// #[async_trait]
/// impl Tool for Clock {
///     fn spec(&self) -> ToolSpec {
///         ToolSpec {
///             name: "clock".into(),
///             description: "What time is it?".into(),
///             parameters: json!({"type": "object", "properties": {}}),
///         }
///     }
///     async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
///         Ok(ToolOutput::text("12:00"))
///     }
/// }
///
/// let set: ToolSet = tools![Clock];
/// assert_eq!(set.names(), ["clock"]);
/// ```
#[derive(Clone, Default)]
pub struct ToolSet {
    tools: Vec<DynTool>,
}

impl ToolSet {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a tool at the end.
    #[must_use]
    pub fn tool(self, tool: impl Tool) -> Self {
        self.dyn_tool(std::sync::Arc::new(tool))
    }

    /// Add an already shared tool at the end.
    #[must_use]
    pub fn dyn_tool(mut self, tool: DynTool) -> Self {
        self.tools.push(tool);
        self
    }

    /// Add every tool of `other` at the end.
    #[must_use]
    pub fn extend(mut self, other: ToolSet) -> Self {
        self.tools.extend(other.tools);
        self
    }

    /// Replace every tool with what `f` makes of it: the place for middleware
    /// such as redaction or logging that wraps a tool in another tool.
    ///
    /// ```
    /// use adam_llm_agent::{DynTool, ToolSet};
    ///
    /// let set = ToolSet::new().wrap(|tool: DynTool| tool);
    /// assert!(set.is_empty());
    /// ```
    #[must_use]
    pub fn wrap(mut self, f: impl FnMut(DynTool) -> DynTool) -> Self {
        self.tools = self.tools.into_iter().map(f).collect();
        self
    }

    /// The tool called `name`, if any (the first, when a name repeats).
    pub fn get(&self, name: &str) -> Option<&DynTool> {
        self.tools.iter().find(|t| t.spec().name == name)
    }

    /// The tools' names, in order.
    pub fn names(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.spec().name).collect()
    }

    /// The tools, in order.
    pub fn iter(&self) -> std::slice::Iter<'_, DynTool> {
        self.tools.iter()
    }

    /// How many tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether the set has no tools.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

impl std::fmt::Debug for ToolSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.names()).finish()
    }
}

impl IntoIterator for ToolSet {
    type Item = DynTool;
    type IntoIter = std::vec::IntoIter<DynTool>;

    fn into_iter(self) -> Self::IntoIter {
        self.tools.into_iter()
    }
}

impl<'a> IntoIterator for &'a ToolSet {
    type Item = &'a DynTool;
    type IntoIter = std::slice::Iter<'a, DynTool>;

    fn into_iter(self) -> Self::IntoIter {
        self.tools.iter()
    }
}

impl FromIterator<DynTool> for ToolSet {
    fn from_iter<I: IntoIterator<Item = DynTool>>(iter: I) -> Self {
        Self {
            tools: iter.into_iter().collect(),
        }
    }
}

/// Build a [`ToolSet`] from tool values, in order: `tools![Clock, Search::new()]`.
///
/// Each item is an expression whose type implements [`Tool`](crate::Tool)
/// (a unit struct is its own value).
#[macro_export]
macro_rules! tools {
    () => {
        $crate::ToolSet::new()
    };
    ($($tool:expr),+ $(,)?) => {
        $crate::ToolSet::new()$(.tool($tool))+
    };
}
