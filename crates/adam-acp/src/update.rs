//! The crate's own view of what an agent reports during a turn, and of the
//! MCP servers handed to it. No SDK types leak out of this module.

use std::collections::{BTreeMap, HashMap};

use agent_client_protocol::schema::v1::{
    ContentBlock, EnvVariable, HttpHeader, McpServer, McpServerHttp, McpServerStdio, SessionUpdate,
    ToolCallContent,
};

/// One entry of an agent's plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanEntry {
    /// What this step is about.
    pub content: String,
    /// `high`, `medium` or `low`.
    pub priority: String,
    /// `pending`, `in_progress` or `completed`.
    pub status: String,
}

/// Something the agent reported while a turn was running.
///
/// The kind/status strings are the protocol's snake_case names
/// (`kind`: `read`, `edit`, `execute`, ...; `status`: `pending`, `in_progress`,
/// `completed`, `failed`), passed through so new values do not break callers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcpUpdate {
    /// A chunk of the agent's reply.
    AgentText(String),
    /// A chunk of the agent's reasoning.
    Thought(String),
    /// The agent's (replaced-wholesale) plan.
    Plan(Vec<PlanEntry>),
    /// The agent started a tool call.
    ToolCall {
        /// Tool call id, stable across its updates.
        id: String,
        /// Human-readable title.
        title: String,
        /// Tool category, e.g. `edit`.
        kind: String,
        /// Initial status.
        status: String,
    },
    /// Progress of a tool call.
    ToolCallUpdate {
        /// Id of the tool call being updated.
        id: String,
        /// Current status (the last known one when the update carries none).
        status: String,
        /// Text output, if the update carried any.
        output: Option<String>,
    },
    /// The last item of every turn. `stop_reason` is `end_turn`,
    /// `max_tokens`, `max_turn_requests`, `refusal` or `cancelled`.
    TurnEnded {
        /// Why the turn stopped.
        stop_reason: String,
    },
}

/// An MCP server the agent should connect to for a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServerSpec {
    /// A subprocess speaking MCP over stdio, started by the agent.
    Stdio {
        /// Display name.
        name: String,
        /// Executable.
        command: String,
        /// Arguments.
        args: Vec<String>,
        /// Extra environment for the subprocess.
        env: BTreeMap<String, String>,
    },
    /// A remote server over streamable HTTP.
    Http {
        /// Display name.
        name: String,
        /// Endpoint URL.
        url: String,
        /// Extra request headers (e.g. `Authorization`).
        headers: BTreeMap<String, String>,
    },
}

impl McpServerSpec {
    pub(crate) fn to_sdk(&self) -> McpServer {
        match self {
            Self::Stdio {
                name,
                command,
                args,
                env,
            } => McpServer::Stdio(
                McpServerStdio::new(name.clone(), command.clone())
                    .args(args.clone())
                    .env(
                        env.iter()
                            .map(|(k, v)| EnvVariable::new(k.clone(), v.clone()))
                            .collect::<Vec<_>>(),
                    ),
            ),
            Self::Http { name, url, headers } => McpServer::Http(
                McpServerHttp::new(name.clone(), url.clone()).headers(
                    headers
                        .iter()
                        .map(|(k, v)| HttpHeader::new(k.clone(), v.clone()))
                        .collect::<Vec<_>>(),
                ),
            ),
        }
    }
}

/// The protocol's snake_case name of a unit-like enum (`StopReason`,
/// `ToolKind`, ...), via its serde form.
pub(crate) fn wire_str<T: serde::Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(s)) => s,
        Ok(other) => other.to_string(),
        Err(_) => "unknown".to_owned(),
    }
}

/// Last known status of each tool call in the running turn, so an update
/// without a status still reports one.
#[derive(Debug, Default)]
pub(crate) struct ToolStatuses(HashMap<String, String>);

/// Map a protocol update to ours. `None` for updates adam-acp does not
/// surface (usage, available commands, mode changes, user-message echoes).
pub(crate) fn map_update(update: SessionUpdate, tools: &mut ToolStatuses) -> Option<AcpUpdate> {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => {
            text_of(&chunk.content).map(AcpUpdate::AgentText)
        }
        SessionUpdate::AgentThoughtChunk(chunk) => text_of(&chunk.content).map(AcpUpdate::Thought),
        SessionUpdate::Plan(plan) => Some(AcpUpdate::Plan(
            plan.entries
                .iter()
                .map(|e| PlanEntry {
                    content: e.content.clone(),
                    priority: wire_str(&e.priority),
                    status: wire_str(&e.status),
                })
                .collect(),
        )),
        SessionUpdate::ToolCall(call) => {
            let id = call.tool_call_id.to_string();
            let status = wire_str(&call.status);
            tools.0.insert(id.clone(), status.clone());
            Some(AcpUpdate::ToolCall {
                id,
                title: call.title,
                kind: wire_str(&call.kind),
                status,
            })
        }
        SessionUpdate::ToolCallUpdate(update) => {
            let id = update.tool_call_id.to_string();
            let fields = update.fields;
            if let Some(status) = &fields.status {
                tools.0.insert(id.clone(), wire_str(status));
            }
            let status = tools
                .0
                .get(&id)
                .cloned()
                .unwrap_or_else(|| "in_progress".to_owned());
            let output = fields
                .content
                .as_deref()
                .and_then(content_text)
                .or_else(|| fields.raw_output.as_ref().map(value_text));
            Some(AcpUpdate::ToolCallUpdate { id, status, output })
        }
        _ => None,
    }
}

fn text_of(block: &ContentBlock) -> Option<String> {
    match block {
        ContentBlock::Text(t) => Some(t.text.clone()),
        _ => None,
    }
}

fn content_text(content: &[ToolCallContent]) -> Option<String> {
    let parts: Vec<String> = content
        .iter()
        .filter_map(|c| match c {
            ToolCallContent::Content(c) => text_of(&c.content),
            ToolCallContent::Diff(d) => Some(format!("edited {}", d.path.display())),
            _ => None,
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn value_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        ContentChunk, Plan, PlanEntry as SdkPlanEntry, PlanEntryPriority, PlanEntryStatus,
        ToolCall, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
    };

    #[test]
    fn tool_call_update_without_status_keeps_the_last_known_one() {
        let mut tools = ToolStatuses::default();
        let call = SessionUpdate::ToolCall(
            ToolCall::new("t1", "Run tests")
                .kind(ToolKind::Execute)
                .status(ToolCallStatus::InProgress),
        );
        assert_eq!(
            map_update(call, &mut tools),
            Some(AcpUpdate::ToolCall {
                id: "t1".into(),
                title: "Run tests".into(),
                kind: "execute".into(),
                status: "in_progress".into(),
            })
        );
        let upd = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "t1".to_owned(),
            ToolCallUpdateFields::new().raw_output(serde_json::json!({"exit": 0})),
        ));
        assert_eq!(
            map_update(upd, &mut tools),
            Some(AcpUpdate::ToolCallUpdate {
                id: "t1".into(),
                status: "in_progress".into(),
                output: Some(r#"{"exit":0}"#.into()),
            })
        );
    }

    #[test]
    fn text_plan_and_ignored_updates() {
        let mut tools = ToolStatuses::default();
        let chunk = SessionUpdate::AgentMessageChunk(ContentChunk::new("hi".into()));
        assert_eq!(
            map_update(chunk, &mut tools),
            Some(AcpUpdate::AgentText("hi".into()))
        );
        let plan = SessionUpdate::Plan(Plan::new(vec![SdkPlanEntry::new(
            "x",
            PlanEntryPriority::Medium,
            PlanEntryStatus::Completed,
        )]));
        assert_eq!(
            map_update(plan, &mut tools),
            Some(AcpUpdate::Plan(vec![PlanEntry {
                content: "x".into(),
                priority: "medium".into(),
                status: "completed".into(),
            }]))
        );
        let echo = SessionUpdate::UserMessageChunk(ContentChunk::new("q".into()));
        assert_eq!(map_update(echo, &mut tools), None);
    }

    #[test]
    fn mcp_specs_serialise_to_the_wire_shapes() {
        let stdio = McpServerSpec::Stdio {
            name: "fs".into(),
            command: "/bin/mcp-fs".into(),
            args: vec!["--root".into(), "/w".into()],
            env: BTreeMap::from([("K".to_owned(), "V".to_owned())]),
        };
        let json = serde_json::to_value(stdio.to_sdk()).unwrap();
        assert_eq!(json["name"], "fs");
        assert_eq!(json["command"], "/bin/mcp-fs");
        assert_eq!(json["args"], serde_json::json!(["--root", "/w"]));
        assert_eq!(
            json["env"],
            serde_json::json!([{"name": "K", "value": "V"}])
        );

        let http = McpServerSpec::Http {
            name: "remote".into(),
            url: "http://127.0.0.1:9/mcp".into(),
            headers: BTreeMap::from([("Authorization".to_owned(), "Bearer x".to_owned())]),
        };
        let json = serde_json::to_value(http.to_sdk()).unwrap();
        assert_eq!(json["type"], "http");
        assert_eq!(json["url"], "http://127.0.0.1:9/mcp");
        assert_eq!(
            json["headers"],
            serde_json::json!([{"name": "Authorization", "value": "Bearer x"}])
        );
    }
}
