//! The tools a session's `config` narrows ([ADR 0054], slice 3): `tools`, an allow-list, and
//! `deny_tools`, a deny-list applied after it.
//!
//! [`SessionTools`](crate::SessionTools) starts and continues sessions; this is the other end. A
//! round carries the lists in its context ([`SESSION_TOOLS_CONTEXT_KEY`]), and [`SessionToolFilter`]
//! is a [`ToolSource`] that offers no tool and says nothing: it only holds back, through
//! [`withhold`](ToolSource::withhold), every tool the lists leave out. The agent applies that at
//! both points a source's holds apply, so a hidden tool is **not in the request** and a call to it
//! anyway is **refused without running**.
//!
//! **It only narrows**, like a mode ([`MODE_CONTEXT_KEY`](crate::MODE_CONTEXT_KEY)): the tier and
//! the mode still hold back what they hold back, and a list cannot offer a tool the agent does not
//! have. Entries are the folder's own syntax: exact names and `*` patterns (`github__*` selects an
//! MCP server). `read_output` is never held back: a round that cannot read a result it was handed
//! could not finish. A round whose context has no such key, or an empty object, is the agent as it
//! was.
//!
//! [ADR 0054]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md

use adam_model::ToolSpec;
use adam_runtime::glob;
use async_trait::async_trait;
use serde_json::{Map, Value};

use crate::conversation::SESSION_TOOLS_CONTEXT_KEY;
use crate::kept::READ_OUTPUT;
use crate::source::{SourceCtx, ToolSource, Withheld};
use crate::tool::{ToolCtx, ToolError, ToolOutput};

/// The [`ToolSource`] that applies a session's `tools` and `deny_tools` to the round it runs in.
///
/// Give it to every agent: it does nothing for a run whose context carries no
/// [`SESSION_TOOLS_CONTEXT_KEY`], which is every run that is not a round of a configured session.
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionToolFilter;

impl SessionToolFilter {
    /// The filter.
    pub fn new() -> Self {
        Self
    }
}

/// The lists a round's context carries: `None` for a list it does not set.
///
/// A value of the wrong shape (only a bug or a hand-made context can write one) **fails closed**:
/// an allow-list that is not a list of names allows nothing, and a deny-list that is not one denies
/// everything.
struct Lists<'a> {
    allow: Option<Vec<&'a str>>,
    deny: Option<Vec<&'a str>>,
}

fn names<'a>(value: Option<&'a Value>, whole: &'a str) -> Option<Vec<&'a str>> {
    match value {
        None | Some(Value::Null) => None,
        Some(Value::Array(entries)) => Some(entries.iter().filter_map(Value::as_str).collect()),
        Some(_) => Some(vec![whole]),
    }
}

impl<'a> Lists<'a> {
    fn of(context: &'a Map<String, Value>) -> Option<Self> {
        let entry = context.get(SESSION_TOOLS_CONTEXT_KEY)?;
        let Some(entry) = entry.as_object() else {
            return Some(Self {
                allow: Some(Vec::new()),
                deny: None,
            });
        };
        let allow = match entry.get("tools") {
            Some(value) if !value.is_array() && !value.is_null() => Some(Vec::new()),
            value => names(value, "*"),
        };
        Some(Self {
            allow,
            deny: names(entry.get("denyTools"), "*"),
        })
    }

    fn holds_back(&self, tool: &str) -> bool {
        if tool == READ_OUTPUT {
            return false;
        }
        let matched = |entries: &[&str]| entries.iter().any(|entry| glob(entry, tool));
        self.allow.as_deref().is_some_and(|allow| !matched(allow))
            || self.deny.as_deref().is_some_and(matched)
    }
}

/// What a withheld tool's call answers: a result for the model, not for the person.
fn refusal(tool: &str) -> String {
    format!(
        "`{tool}` is not offered to this session: the settings it was started with leave it out. \
         Do not look for another way to the same end. Say in plain words what you did not do and \
         that the session's settings do not offer `{tool}`; whoever started the session can start \
         another with different settings. Then do the rest of the work."
    )
}

#[async_trait]
impl ToolSource for SessionToolFilter {
    async fn specs(&self, _ctx: &SourceCtx) -> Vec<ToolSpec> {
        Vec::new()
    }

    async fn withhold(&self, ctx: &SourceCtx, tools: &[&str]) -> Vec<Withheld> {
        let Some(lists) = Lists::of(ctx.context_map()) else {
            return Vec::new();
        };
        tools
            .iter()
            .filter(|tool| lists.holds_back(tool))
            .map(|tool| Withheld::new(*tool, refusal(tool)))
            .collect()
    }

    async fn call(
        &self,
        _ctx: &ToolCtx,
        _name: &str,
        _args: Value,
    ) -> Option<Result<ToolOutput, ToolError>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const TOOLS: [&str; 5] = [
        "read_file",
        "write_file",
        "linear__list_issues",
        READ_OUTPUT,
        "subagent_start",
    ];

    async fn held(entry: Option<Value>) -> Vec<String> {
        let context = entry
            .map(|entry| Map::from_iter([(SESSION_TOOLS_CONTEXT_KEY.to_owned(), entry)]))
            .unwrap_or_default();
        SessionToolFilter
            .withhold(&SourceCtx::detached(context), &TOOLS)
            .await
            .into_iter()
            .map(|h| h.tool)
            .collect()
    }

    #[tokio::test]
    async fn no_key_and_an_empty_value_hold_nothing_back() {
        assert!(held(None).await.is_empty());
        assert!(held(Some(json!({}))).await.is_empty());
    }

    #[tokio::test]
    async fn an_allow_list_holds_back_what_it_does_not_match_but_read_output() {
        assert_eq!(
            held(Some(json!({"tools": ["read_file"]}))).await,
            ["write_file", "linear__list_issues", "subagent_start"]
        );
        assert_eq!(
            held(Some(json!({"tools": ["read_*", "linear__*"]}))).await,
            ["write_file", "subagent_start"]
        );
        // An empty allow-list offers nothing but `read_output`.
        assert_eq!(
            held(Some(json!({"tools": []}))).await,
            [
                "read_file",
                "write_file",
                "linear__list_issues",
                "subagent_start"
            ]
        );
    }

    #[tokio::test]
    async fn a_deny_list_wins_over_the_allow_list_and_never_takes_read_output() {
        assert_eq!(
            held(Some(
                json!({"tools": ["read_file", "write_file"], "denyTools": ["write_*"]})
            ))
            .await,
            ["write_file", "linear__list_issues", "subagent_start"]
        );
        assert_eq!(
            held(Some(json!({"denyTools": ["*"]}))).await,
            [
                "read_file",
                "write_file",
                "linear__list_issues",
                "subagent_start"
            ]
        );
    }

    #[tokio::test]
    async fn a_value_of_the_wrong_shape_fails_closed() {
        for bad in [json!("read_file"), json!({"tools": "read_file"})] {
            assert_eq!(
                held(Some(bad.clone())).await,
                [
                    "read_file",
                    "write_file",
                    "linear__list_issues",
                    "subagent_start"
                ],
                "{bad}"
            );
        }
        assert_eq!(
            held(Some(json!({"denyTools": "write_file"}))).await,
            [
                "read_file",
                "write_file",
                "linear__list_issues",
                "subagent_start"
            ]
        );
    }

    #[tokio::test]
    async fn the_refusal_names_the_tool_and_tells_the_model_to_say_so() {
        let context = Map::from_iter([(
            SESSION_TOOLS_CONTEXT_KEY.to_owned(),
            json!({"tools": ["read_file"]}),
        )]);
        let held = SessionToolFilter
            .withhold(&SourceCtx::detached(context), &["write_file"])
            .await;
        assert_eq!(held.len(), 1);
        assert!(held[0].reason.contains("`write_file` is not offered"));
        assert!(held[0].reason.contains("Do not look for another way"));
        assert!(held[0].reason.contains("Say in plain words"));
    }
}
