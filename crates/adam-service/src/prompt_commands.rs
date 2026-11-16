//! The prompts of an agent's MCP servers as `commands/v1` commands (feature `mcp`).
//!
//! [`Assembly::prompt_commands`](https://docs.rs/adam-assembly) gives the prompts the root's servers
//! serve; [`with_prompt_commands`] adds them to the commands the backend expands, and
//! [`card_with_commands`] lists them on the card. A prompt that has the name of a command of the
//! folder loses, with one warning, in both places (the backend and the card are made from the same
//! answer, so they cannot list different commands).
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
use adam_mcp::{PromptCommand, PromptError};
use async_trait::async_trait;

/// A prompt of an MCP server, asked when a message names it.
struct Prompt(PromptCommand);

#[async_trait]
impl RemoteCommand for Prompt {
    async fn expand(&self, arguments: &[String]) -> Result<String, CommandRefusal> {
        self.0.expand(arguments).await.map_err(|error| match error {
            PromptError::Invalid(why) => CommandRefusal::Invalid(why),
            PromptError::Unavailable(why) => CommandRefusal::Unavailable(why),
        })
    }
}

/// `commands` with each of `prompts` added as a remote command, and what the card says of the ones
/// that were added. A prompt whose name a command already has is left out, with a warning.
pub fn with_prompt_commands(
    mut commands: Commands,
    prompts: &[PromptCommand],
) -> (Commands, Vec<CommandInfo>) {
    let mut infos = Vec::with_capacity(prompts.len());
    for prompt in prompts {
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
    (commands, infos)
}

/// `card` with `extra` added to its `commands/v1` entry (made when the card has none).
#[must_use]
pub fn card_with_commands(mut card: AgentCardConfig, extra: &[CommandInfo]) -> AgentCardConfig {
    if extra.is_empty() {
        return card;
    }
    let mut infos: Vec<CommandInfo> = card
        .extensions
        .iter()
        .find(|extension| extension.uri == COMMANDS_EXTENSION)
        .and_then(|extension| extension.params.get("commands"))
        .and_then(serde_json::Value::as_array)
        .map(|listed| {
            listed
                .iter()
                .filter_map(|entry| {
                    Some(CommandInfo {
                        name: entry.get("name")?.as_str()?.to_owned(),
                        description: entry.get("description")?.as_str()?.to_owned(),
                        argument_hint: entry
                            .get("argumentHint")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    infos.extend(extra.iter().cloned());
    card.extensions
        .retain(|extension| extension.uri != COMMANDS_EXTENSION);
    if let Some(extension) = ExtensionConfig::commands(&infos) {
        card.extensions.push(extension);
    }
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
}
