//! The root agent's A2A card (feature `a2a`).

use adam_a2a::{AgentCardConfig, ExtendedCardConfig, SkillConfig};
use url::Url;

use adam_agent_fs::AgentManifest;

use crate::assembly::Assembly;
use crate::def::AgentDef;
use crate::error::{Error, Origin};

impl AgentDef {
    /// The card of this definition's root agent, before any tool, state or model is bound.
    ///
    /// The card is a fact about the files, not about the running agent, so a process that only
    /// serves A2A (a control plane: no model, no tools, no credentials) can advertise the same
    /// card as the workers that step the runs. It is exactly what
    /// [`Assembly::card`](crate::Assembly::card) returns for the assembled agent.
    ///
    /// # Errors
    ///
    /// [`Error::MissingCardDescription`] when neither the card nor the agent has a description.
    pub fn card(&self, url: Url, version: impl Into<String>) -> Result<AgentCardConfig, Error> {
        card_of(self.manifest(), url, version.into())
    }
}

impl Assembly {
    /// The root agent's `card:` as an [`AgentCardConfig`], ready for
    /// `A2aServer::router`.
    ///
    /// The frontmatter holds what describes the agent: `card.name` (default: the agent's name),
    /// `card.description` (default: the frontmatter `description`) and `card.skills`. What
    /// belongs to the deployment is not in a file: `url` is where clients reach the server and
    /// `version` is the agent's own version (usually `env!("CARGO_PKG_VERSION")`). A2A card
    /// skills are what the agent offers to callers; they are unrelated to Agent Skills.
    ///
    /// # Errors
    ///
    /// [`Error::MissingCardDescription`] when neither the card nor the agent has a description.
    pub fn card(&self, url: Url, version: impl Into<String>) -> Result<AgentCardConfig, Error> {
        card_of(self.manifest(), url, version.into())
    }
}

/// The card `manifest`'s root agent declares, with the deployment's `url` and `version`.
fn card_of(manifest: &AgentManifest, url: Url, version: String) -> Result<AgentCardConfig, Error> {
    let front = &manifest.frontmatter;
    let card = front.card.as_ref();
    let name = card
        .and_then(|c| c.name.clone())
        .unwrap_or_else(|| manifest.name.clone());
    let description = card
        .and_then(|c| c.description.clone())
        .or_else(|| front.description.clone())
        .filter(|d| !d.trim().is_empty())
        .ok_or_else(|| Error::MissingCardDescription {
            origin: Origin::new(manifest.name.clone(), manifest.path.clone()),
        })?;
    let mut config = AgentCardConfig::new(name, description, url, version);
    for skill in card.map(|c| c.skills.as_slice()).unwrap_or_default() {
        config = config.with_skill(skill_of(skill));
    }
    // `card.extended`: what an authenticated caller sees on top. Nothing declared, nothing
    // served (an extended card that adds nothing is not worth declaring).
    if let Some(extended) = card.and_then(|c| c.extended.as_ref()) {
        let mut extra = ExtendedCardConfig::new();
        extra.description = extended
            .description
            .clone()
            .filter(|d| !d.trim().is_empty());
        for skill in &extended.skills {
            extra = extra.with_skill(skill_of(skill));
        }
        if !extra.is_empty() {
            config = config.with_extended_card(extra);
        }
    }
    Ok(config)
}

fn skill_of(skill: &adam_agent_fs::CardSkill) -> SkillConfig {
    SkillConfig {
        id: skill.id.clone(),
        name: skill.name.clone(),
        description: skill.description.clone(),
        tags: skill.tags.clone(),
        examples: skill.examples.clone(),
    }
}
