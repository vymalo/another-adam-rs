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

/// The prompts of an agent's MCP servers, as commands.
#[cfg(feature = "mcp")]
pub(crate) type Prompts = Vec<adam_mcp::PromptCommand>;

/// Without the feature there is no client to ask: no prompts, and a type that says so.
#[cfg(not(feature = "mcp"))]
pub(crate) type Prompts = Vec<std::convert::Infallible>;

/// What was given for one agent.
#[derive(Clone)]
pub(crate) struct AgentMcp {
    /// The `mcp.json` the servers were connected from. `None` for tools supplied by hand
    /// ([`AgentDef::mcp_tools`](crate::AgentDef::mcp_tools)), which were connected from nothing
    /// this crate knows.
    pub(crate) config: Option<McpConfig>,
    pub(crate) tools: ToolSet,
    /// What its servers said about themselves, by server, heading included, ready to append to its
    /// instructions (empty for tools supplied by hand). Read once, when the servers were
    /// connected, so the prompt of the agent never changes during the process. A server with a
    /// `when` contributes its section to the note of its group: the first prompt's for a group the
    /// task loads, the load's result for one the run loads ([`groups`](crate::groups)).
    pub(crate) instructions: Vec<(String, String)>,
    /// The prompts of its servers as commands (the root's are served; a subagent's are not).
    pub(crate) prompts: Prompts,
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
            .field(
                "instructions",
                &self
                    .instructions
                    .iter()
                    .map(|(_, text)| text.len())
                    .sum::<usize>(),
            )
            .field("prompts", &self.prompts.len())
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
        self.agents.insert(
            agent.into(),
            AgentMcp {
                config,
                tools,
                instructions: Vec::new(),
                prompts: Vec::new(),
            },
        );
    }

    /// Tools connected from `config`, with what their servers said and the prompts they serve.
    #[cfg(feature = "mcp")]
    pub(crate) fn set_connected(
        &mut self,
        agent: impl Into<String>,
        config: McpConfig,
        tools: ToolSet,
        instructions: Vec<(String, String)>,
        prompts: Prompts,
    ) {
        self.agents.insert(
            agent.into(),
            AgentMcp {
                config: Some(config),
                tools,
                instructions,
                prompts,
            },
        );
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

/// The most the instructions of an agent's MCP servers may add to its prompt, all together: the
/// bound `adam-mcp` applies to the whole set when it is appended at bind
/// (`adam_mcp::MAX_INSTRUCTIONS_BYTES`, 8 KiB).
pub(crate) const MAX_INSTRUCTIONS_BYTES: usize = 8 * 1024;

/// `sections` (a server's name and the section `adam_mcp::McpServers::instructions_by_server` gives
/// it) as one text: blank-line separated, in order, cut at [`MAX_INSTRUCTIONS_BYTES`] together (the
/// server that would pass it and the ones after are left out, with a warning).
pub(crate) fn join_sections(sections: &[(String, String)]) -> String {
    let mut out = String::new();
    for (server, section) in sections {
        let separator = if out.is_empty() { "" } else { "\n\n" };
        if out.len() + separator.len() + section.len() > MAX_INSTRUCTIONS_BYTES {
            tracing::warn!(
                server = %server,
                limit = MAX_INSTRUCTIONS_BYTES,
                "the MCP servers' instructions are over the limit together: \
                 this server's and the next ones' are left out"
            );
            break;
        }
        out.push_str(separator);
        out.push_str(section);
    }
    out
}
