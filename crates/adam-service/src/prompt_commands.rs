//! The prompts of an agent's MCP servers as `commands/v1` commands (feature `mcp`).
//!
//! [`Assembly::prompt_commands`](https://docs.rs/adam-assembly) gives the prompts the root's servers
//! serve; [`with_prompt_commands`] adds them to the commands the backend expands, and
//! [`card_with_commands`] lists them on the card. A prompt that has the name of a command of the
//! folder loses, with one warning, in both places (the backend and the card are made from the same
//! answer, so they cannot list different commands), and the folder's commands and the prompts
//! together are at most 50. A process that connects no server ([`refuse_prompt_commands`]) refuses
//! the names of the prompts its folder's servers could serve.
//!
//! ```mermaid
//! sequenceDiagram
//!     participant S as serve (role all)
//!     participant A as Assembly (MCP servers connected)
//!     participant C as Commands (the backend)
//!     participant K as the card
//!     S->>A: prompt_commands()
//!     S->>C: with_prompt_commands(folder commands, prompts)
//!     Note over C: a name the folder has is kept as the folder wrote it
//!     C-->>S: commands, the infos of the prompts added
//!     S->>K: card_with_commands(card, infos)
//! ```

use std::sync::Arc;

use adam_a2a::{AgentCardConfig, COMMANDS_EXTENSION, CommandInfo, ExtensionConfig};
use adam_a2a_runtime::{CommandRefusal, Commands, RemoteCommand};
use adam_agent_fs::{MAX_COMMANDS, McpConfig, is_agent_name};
use adam_mcp::{PromptCommand, PromptError};
use async_trait::async_trait;

/// A prompt of an MCP server, asked when a message names it.
struct Prompt(PromptCommand);

#[async_trait]
impl RemoteCommand for Prompt {
    async fn expand(&self, arguments: &str) -> Result<String, CommandRefusal> {
        self.0
            .expand_text(arguments)
            .await
            .map_err(|error| match error {
                PromptError::Invalid(why) => CommandRefusal::Invalid(why),
                PromptError::Unavailable(why) => CommandRefusal::Unavailable(why),
            })
    }
}

/// `commands` with each of `prompts` added as a remote command, and what the card says of the ones
/// that were added.
///
/// The folder's commands and the prompts together are at most [`MAX_COMMANDS`] (the limit of an
/// agent's folder): the folder's come first and the prompts follow in the order given, so what does
/// not fit is the last prompts, dropped with one warning that gives how many. A prompt whose name a
/// command already has is left out, with a warning.
pub fn with_prompt_commands(
    mut commands: Commands,
    prompts: &[PromptCommand],
) -> (Commands, Vec<CommandInfo>) {
    let room = MAX_COMMANDS.saturating_sub(commands.served());
    let mut infos = Vec::with_capacity(prompts.len().min(room));
    let mut dropped = 0_usize;
    for prompt in prompts {
        if infos.len() >= room {
            dropped += 1;
            continue;
        }
        if !commands.add_remote(prompt.name(), Arc::new(Prompt(prompt.clone()))) {
            tracing::warn!(
                command = prompt.name(),
                server = prompt.server(),
                "an MCP prompt has the name of a command of the agent's folder: the folder's \
                 command is served and the prompt is not"
            );
            continue;
        }
        infos.push(CommandInfo {
            name: prompt.name().to_owned(),
            description: prompt.description().to_owned(),
            argument_hint: prompt.argument_hint().map(str::to_owned),
        });
    }
    if dropped > 0 {
        tracing::warn!(
            dropped,
            limit = MAX_COMMANDS,
            "the folder's commands and the MCP prompts are over the limit of commands an agent \
             serves: the last prompts are left out"
        );
    }
    (commands, infos)
}

/// `commands` for a process that connects no MCP server (a control plane): it cannot ask a server
/// for a prompt, so a message that names `/<server>__<anything>` for a server of `mcp` that serves
/// prompts is **refused** (`invalid params`) and never reaches the model as plain text. The names
/// come from the files, so nothing is connected. One warning names the servers.
///
/// A server with `prompts: false`, or whose name cannot start a command, is left alone.
pub fn refuse_prompt_commands(mut commands: Commands, mcp: Option<&McpConfig>) -> Commands {
    let Some(mcp) = mcp else {
        return commands;
    };
    let mut named = Vec::new();
    for (name, server) in &mcp.servers {
        if !server.settings().prompts || !is_agent_name(&format!("{name}__x")) {
            continue;
        }
        commands.refuse_prefix(
            format!("{name}__"),
            format!(
                "`/{name}__...` is the prompt of an MCP server, and this deployment's front \
                 (a control plane) connects no MCP server, so it does not serve the prompts of \
                 MCP servers"
            ),
        );
        named.push(name.as_str());
    }
    if !named.is_empty() {
        tracing::warn!(
            servers = named.join(", "),
            "this process connects no MCP server (a control plane): it serves none of their \
             prompts as commands, and refuses a message that names one"
        );
    }
    commands
}

/// `card` with `extra` added to its `commands/v1` entry (made when the card has none). Every entry
/// the card already has is kept as it is; the entries are sorted by name.
#[must_use]
pub fn card_with_commands(mut card: AgentCardConfig, extra: &[CommandInfo]) -> AgentCardConfig {
    let Some(added) = ExtensionConfig::commands(extra) else {
        return card;
    };
    let Some(existing) = card
        .extensions
        .iter_mut()
        .find(|extension| extension.uri == COMMANDS_EXTENSION)
    else {
        card.extensions.push(added);
        return card;
    };
    let entries = |extension: &ExtensionConfig| {
        extension
            .params
            .get("commands")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let mut listed = entries(existing);
    listed.extend(entries(&added));
    listed.sort_by(|a, b| {
        let name = |entry: &serde_json::Value| {
            entry
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        name(a).cmp(&name(b))
    });
    existing
        .params
        .insert("commands".to_owned(), serde_json::Value::Array(listed));
    card
}

#[cfg(test)]
mod tests {
    use super::*;
    use url::Url;

    fn plain_card(commands: &[CommandInfo]) -> AgentCardConfig {
        let url = Url::parse("http://agent.test/").unwrap();
        let mut card = AgentCardConfig::new("a", "an agent", url, "1");
        if let Some(extension) = ExtensionConfig::commands(commands) {
            card = card.with_extension(extension);
        }
        card
    }

    fn info(name: &str, hint: Option<&str>) -> CommandInfo {
        CommandInfo {
            name: name.to_owned(),
            description: format!("The {name} command."),
            argument_hint: hint.map(str::to_owned),
        }
    }

    fn listed(card: &AgentCardConfig) -> Vec<String> {
        card.extensions
            .iter()
            .filter(|e| e.uri == COMMANDS_EXTENSION)
            .flat_map(|e| e.params["commands"].as_array().unwrap().clone())
            .map(|c| c["name"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn the_card_lists_the_folders_commands_and_the_added_ones_in_one_entry() {
        let folder = [info("review", Some("<file>"))];
        let added = [info("docs__summarize", None)];
        let card = card_with_commands(plain_card(&folder), &added);
        assert_eq!(
            listed(&card),
            ["docs__summarize", "review"],
            "sorted by name"
        );
        let entry = card
            .extensions
            .iter()
            .find(|e| e.uri == COMMANDS_EXTENSION)
            .unwrap();
        assert_eq!(entry.params["commands"][1]["argumentHint"], "<file>");
        assert_eq!(
            card.extensions
                .iter()
                .filter(|e| e.uri == COMMANDS_EXTENSION)
                .count(),
            1
        );
    }

    #[test]
    fn a_card_without_commands_gets_the_extension_when_there_are_prompts() {
        let card = card_with_commands(plain_card(&[]), &[info("docs__plain", None)]);
        assert_eq!(listed(&card), ["docs__plain"]);
        // Nothing to add: the card is as it was.
        let same = card_with_commands(plain_card(&[]), &[]);
        assert!(listed(&same).is_empty());
    }

    #[test]
    fn a_card_entry_that_cannot_be_read_as_a_command_is_kept_as_it_is() {
        let mut card = plain_card(&[info("review", None)]);
        let entry = card
            .extensions
            .iter_mut()
            .find(|e| e.uri == COMMANDS_EXTENSION)
            .unwrap();
        entry
            .params
            .get_mut("commands")
            .unwrap()
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"name": "odd", "note": "no description"}));
        let card = card_with_commands(card, &[info("docs__a", None)]);
        let entries = card.extensions[0].params["commands"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(entries.len(), 3, "{entries:?}");
        assert!(
            entries
                .iter()
                .any(|e| e["name"] == "odd" && e["note"] == "no description")
        );
    }

    fn mcp(json: &str) -> McpConfig {
        let mut diagnostics = Vec::new();
        adam_agent_fs::parse_mcp(std::path::Path::new("mcp.json"), json, &mut diagnostics)
            .expect("an mcp.json")
    }

    #[tokio::test]
    async fn a_control_plane_refuses_the_prompts_of_the_servers_that_could_serve_them() {
        let config = mcp(r#"{"mcpServers": {
                "docs": {"type": "http", "url": "https://a.example.com/mcp"},
                "quiet": {"type": "http", "url": "https://b.example.com/mcp", "prompts": false},
                "Loud": {"type": "http", "url": "https://c.example.com/mcp"}
            }}"#);
        let commands =
            refuse_prompt_commands(Commands::new([("review", "Review $1")]), Some(&config));
        let refusal = |name: &'static str| {
            let commands = commands.clone();
            async move { commands.expand_any(name, "").await.unwrap_err().to_string() }
        };
        let refused = refusal("docs__summarize").await;
        assert!(refused.contains("connects no MCP server"), "{refused}");
        assert!(
            refusal("quiet__x").await.starts_with("unknown command"),
            "prompts: false"
        );
        assert!(refusal("loud__x").await.starts_with("unknown command"));
        assert_eq!(
            commands.expand_any("review", "a").await.unwrap(),
            "Review a"
        );
        // No servers, nothing refused.
        let none = refuse_prompt_commands(Commands::default(), None);
        assert!(none.is_empty());
    }
}
