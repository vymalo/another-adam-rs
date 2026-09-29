//! `mcp.json`: the `mcpServers` shape of Claude Code's `.mcp.json` and the draft SEP-2633.
//!
//! `${VAR}` and `${VAR:-default}` are **never** expanded here: expansion happens at run time,
//! from the process environment, so that a secret never passes through the build. This module
//! only finds the references ([`McpConfig::env_references`]) so a build can record their names.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use serde_json::Value;

use super::names::is_env_name;

/// The transport of a remote MCP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteKind {
    /// `type: http`.
    Http,
    /// `type: streamable-http`.
    StreamableHttp,
    /// `type: sse`.
    Sse,
}

/// One MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServer {
    /// A process the runtime spawns (`command`; `type` is `stdio` or absent).
    Stdio {
        /// The executable.
        command: String,
        /// Its arguments.
        args: Vec<String>,
        /// Its environment; values are unexpanded text.
        env: BTreeMap<String, String>,
        /// The allow-list of tools (an adam extension); `None` allows all.
        tools: Option<Vec<String>>,
    },
    /// A server reached over the network (`type` and `url`).
    Remote {
        /// The transport.
        kind: RemoteKind,
        /// The endpoint; unexpanded text.
        url: String,
        /// Request headers; values are unexpanded text.
        headers: BTreeMap<String, String>,
        /// The allow-list of tools (an adam extension); `None` allows all.
        tools: Option<Vec<String>>,
    },
}

impl McpServer {
    /// The allow-list of tools, when there is one.
    pub fn tools(&self) -> Option<&[String]> {
        match self {
            Self::Stdio { tools, .. } | Self::Remote { tools, .. } => tools.as_deref(),
        }
    }

    fn texts(&self) -> Vec<&str> {
        match self {
            Self::Stdio {
                command, args, env, ..
            } => std::iter::once(command.as_str())
                .chain(args.iter().map(String::as_str))
                .chain(env.values().map(String::as_str))
                .collect(),
            Self::Remote { url, headers, .. } => std::iter::once(url.as_str())
                .chain(headers.values().map(String::as_str))
                .collect(),
        }
    }
}

/// A parsed `mcp.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpConfig {
    /// The servers by name. The model sees their tools as `<server>__<tool>`.
    pub servers: BTreeMap<String, McpServer>,
}

impl McpConfig {
    /// The names of every environment variable the config refers to with `${NAME}` or
    /// `${NAME:-default}`, sorted. Names only, never values.
    pub fn env_references(&self) -> BTreeSet<String> {
        self.servers
            .values()
            .flat_map(McpServer::texts)
            .flat_map(|t| scan_references(t).refs)
            .map(|r| r.name)
            .collect()
    }
}

/// One `${NAME}` or `${NAME:-default}` in a text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvRef {
    /// The variable.
    pub name: String,
    /// The text after `:-`, when there is one.
    pub default: Option<String>,
}

/// The references found in a text, and the `${` that do not form one.
#[derive(Debug, Default)]
pub(crate) struct Scan {
    pub(crate) refs: Vec<EnvRef>,
    /// The malformed spans, as written.
    pub(crate) malformed: Vec<String>,
}

/// Find `${NAME}` and `${NAME:-default}`. A `${` with no `}`, or with a name that is not an
/// environment variable name, is reported as malformed and left alone.
pub(crate) fn scan_references(text: &str) -> Scan {
    let mut scan = Scan::default();
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            scan.malformed.push(rest[start..].to_owned());
            break;
        };
        let inner = &after[..end];
        let (name, default) = match inner.split_once(":-") {
            Some((n, d)) => (n, Some(d.to_owned())),
            None => (inner, None),
        };
        if is_env_name(name) {
            scan.refs.push(EnvRef {
                name: name.to_owned(),
                default,
            });
        } else {
            scan.malformed.push(format!("${{{inner}}}"));
        }
        rest = &after[end + 1..];
    }
    scan
}

/// The file as serde reads it, before the checks that turn it into an [`McpConfig`].
#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawMcp {
    #[serde(default, rename = "mcpServers")]
    pub(crate) servers: BTreeMap<String, RawServer>,
    #[serde(flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

/// One server entry, every key optional.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct RawServer {
    #[serde(rename = "type")]
    pub(crate) kind: Option<String>,
    pub(crate) command: Option<String>,
    pub(crate) args: Vec<String>,
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) url: Option<String>,
    pub(crate) headers: BTreeMap<String, String>,
    pub(crate) tools: Option<Vec<String>>,
    #[serde(flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_references_and_defaults() {
        let s = scan_references("a ${A} b ${B:-x y} c ${ } ${1X} ${open");
        let names: Vec<_> = s.refs.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["A", "B"]);
        assert_eq!(s.refs[1].default.as_deref(), Some("x y"));
        assert_eq!(s.malformed.len(), 3, "{:?}", s.malformed);
    }

    #[test]
    fn plain_text_has_none() {
        let s = scan_references("no refs $HOME {x} $");
        assert!(s.refs.is_empty() && s.malformed.is_empty());
    }
}
