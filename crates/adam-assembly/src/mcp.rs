//! The MCP tools of an agent: what `mcp.json` declares, what was connected for it, and the rules
//! that bind the two.
//!
//! An agent's MCP tools come **only** from its own directory's `mcp.json` (the root's for the root,
//! a subagent directory's for that subagent). They are not added to the shared tool set that
//! [`AgentDef::bind`](crate::AgentDef::bind) takes, so a subagent inherits none of its parent's,
//! and two directories may both have a server called `linear`, each with its own connection and
//! credentials.
//!
//! ```mermaid
//! sequenceDiagram
//!     participant C as composition root
//!     participant D as AgentDef
//!     participant M as adam-mcp
//!     participant B as AgentDef::bind
//!     C->>D: from_manifest, env(..)
//!     C->>D: connect_mcp(policy)
//!     loop root and each local subagent with a non-empty mcp.json
//!         D->>M: McpServers::connect(config, Env, policy)
//!         M-->>D: tools (server__tool)
//!         Note over D: kept per agent, with the config they were connected from
//!     end
//!     C->>B: bind(registered tools)
//!     Note over B: per agent: connected? same config? names fit the servers?<br/>no clash with a registered tool? then tools: selects among both
//!     B-->>C: BoundDef
//! ```
//!
//! ```mermaid
//! stateDiagram-v2
//!     [*] --> Declared: mcp.json in the manifest
//!     Declared --> Connected: connect_mcp
//!     Declared --> Supplied: mcp_tools (a client of your own, or a test)
//!     Declared --> Refused: bind without either (McpNotConnected)
//!     Connected --> Bound: bind, same mcp.json
//!     Connected --> Refused: bind, mcp.json differs (McpChanged)
//!     Supplied --> Bound: bind, names fit the servers
//!     Supplied --> Refused: a tool of no server of the file (McpForeignTool)
//!     Bound --> [*]
//!     Refused --> [*]
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;

use adam_agent_fs::{AgentManifest, McpConfig, Subagent};
use adam_llm_agent::ToolSet;

/// What was given for one agent.
#[derive(Clone)]
pub(crate) struct AgentMcp {
    /// The `mcp.json` the servers were connected from. `None` for tools supplied by hand
    /// ([`AgentDef::mcp_tools`](crate::AgentDef::mcp_tools)), which were connected from nothing
    /// this crate knows.
    pub(crate) config: Option<McpConfig>,
    pub(crate) tools: ToolSet,
}

impl std::fmt::Debug for AgentMcp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentMcp")
            .field(
                "servers",
                &self
                    .config
                    .as_ref()
                    .map(|c| c.servers.keys().collect::<Vec<_>>()),
            )
            .field("tools", &self.tools)
            .finish()
    }
}

/// The MCP tools of the agents of a definition, by the name each is registered under.
#[derive(Debug, Clone, Default)]
pub(crate) struct McpBinding {
    agents: BTreeMap<String, AgentMcp>,
}

impl McpBinding {
    pub(crate) fn set(
        &mut self,
        agent: impl Into<String>,
        config: Option<McpConfig>,
        tools: ToolSet,
    ) {
        self.agents.insert(agent.into(), AgentMcp { config, tools });
    }

    pub(crate) fn get(&self, agent: &str) -> Option<&AgentMcp> {
        self.agents.get(agent)
    }

    pub(crate) fn agents(&self) -> impl Iterator<Item = &String> {
        self.agents.keys()
    }
}

/// The servers `manifest` declares: its `mcp.json`, unless it has no servers (an empty file is no
/// file).
pub(crate) fn declared(manifest: &AgentManifest) -> Option<&McpConfig> {
    manifest.mcp.as_ref().filter(|c| !c.servers.is_empty())
}

/// The `mcp.json` of an agent: next to its instructions file.
pub(crate) fn file_of(manifest: &AgentManifest) -> PathBuf {
    manifest.path.with_file_name("mcp.json")
}

/// The local agents of a tree, depth first, root first: the registration name and the manifest.
pub(crate) fn local_agents<'a>(
    manifest: &'a AgentManifest,
    name: &str,
    out: &mut Vec<(String, &'a AgentManifest)>,
) {
    out.push((name.to_owned(), manifest));
    for sub in &manifest.subagents {
        if let Subagent::Local(child) = sub {
            local_agents(child, &format!("{name}/{}", child.name), out);
        }
    }
}
