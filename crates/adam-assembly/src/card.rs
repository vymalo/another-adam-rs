//! The root agent's A2A card (feature `a2a`).

use adam_a2a::{AgentCardConfig, SkillConfig};
use url::Url;

use crate::assembly::Assembly;
use crate::error::{Error, Origin};

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
        let manifest = self.manifest();
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
            config = config.with_skill(SkillConfig {
                id: skill.id.clone(),
                name: skill.name.clone(),
                description: skill.description.clone(),
                tags: skill.tags.clone(),
                examples: skill.examples.clone(),
            });
        }
        Ok(config)
    }
}
