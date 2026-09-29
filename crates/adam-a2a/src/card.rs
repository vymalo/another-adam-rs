//! Agent card configuration and its rendering as an A2A 1.0 `AgentCard`.

use std::collections::HashMap;

use a2a::{
    AgentCapabilities, AgentCard, AgentExtension, AgentInterface, AgentSkill,
    HttpAuthSecurityScheme, SecurityScheme, TRANSPORT_PROTOCOL_JSONRPC,
};
use url::Url;

/// Name under which the bearer scheme is advertised in the card.
const BEARER_SCHEME_NAME: &str = "bearer";

/// What an agent tells the world about itself.
///
/// Build it with [`AgentCardConfig::new`] and the `with_*` methods so that
/// fields added later do not break your code.
#[derive(Clone, Debug)]
pub struct AgentCardConfig {
    /// Human-readable agent name.
    pub name: String,
    /// What the agent does.
    pub description: String,
    /// The public URL of this server's JSON-RPC endpoint, i.e. where the
    /// router returned by [`A2aServer::router`](crate::A2aServer::router) is
    /// reachable as seen by clients (include any path prefix you nest it
    /// under). Clients POST to exactly this URL.
    pub url: Url,
    /// The agent's own version string (not the protocol version).
    pub version: String,
    /// Skills advertised on the card.
    pub skills: Vec<SkillConfig>,
    /// A2A extensions this agent declares (for example the platform's
    /// release-channels extension). Empty by default; declaring one here only
    /// advertises it, the server does not implement any extension itself.
    pub extensions: Vec<ExtensionConfig>,
}

impl AgentCardConfig {
    /// A card with no skills and no extensions.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        url: Url,
        version: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            url,
            version: version.into(),
            skills: Vec::new(),
            extensions: Vec::new(),
        }
    }

    /// Add a skill.
    #[must_use]
    pub fn with_skill(mut self, skill: SkillConfig) -> Self {
        self.skills.push(skill);
        self
    }

    /// Declare an extension.
    #[must_use]
    pub fn with_extension(mut self, extension: ExtensionConfig) -> Self {
        self.extensions.push(extension);
        self
    }
}

/// One advertised skill.
#[derive(Clone, Debug, Default)]
pub struct SkillConfig {
    /// Stable skill identifier.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// What the skill does.
    pub description: String,
    /// Free-form tags for discovery.
    pub tags: Vec<String>,
    /// Example prompts.
    pub examples: Vec<String>,
}

impl SkillConfig {
    /// A skill with no tags or examples.
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            description: description.into(),
            ..Self::default()
        }
    }
}

/// An A2A extension declaration on the card.
#[derive(Clone, Debug, Default)]
pub struct ExtensionConfig {
    /// The extension's URI (its versioned identity).
    pub uri: String,
    /// What the extension is for.
    pub description: Option<String>,
    /// Whether clients must support it.
    pub required: bool,
    /// Extension-specific parameters.
    pub params: serde_json::Map<String, serde_json::Value>,
}

impl ExtensionConfig {
    /// An optional extension with no parameters.
    pub fn new(uri: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            ..Self::default()
        }
    }
}

/// Render the config as the served card.
///
/// Streaming is on, push notifications are off, and the bearer scheme is
/// advertised exactly when the server enforces bearer tokens.
pub(crate) fn build_card(config: &AgentCardConfig, bearer_auth: bool) -> AgentCard {
    let extensions = (!config.extensions.is_empty()).then(|| {
        config
            .extensions
            .iter()
            .map(|e| AgentExtension {
                uri: e.uri.clone(),
                description: e.description.clone(),
                required: Some(e.required),
                params: (!e.params.is_empty()).then(|| {
                    e.params
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect()
                }),
            })
            .collect()
    });

    let (security_schemes, security_requirements) = if bearer_auth {
        let scheme = SecurityScheme::HttpAuth(HttpAuthSecurityScheme {
            scheme: "Bearer".to_owned(),
            description: Some("Bearer token in the Authorization header".to_owned()),
            bearer_format: Some("opaque".to_owned()),
        });
        (
            Some(HashMap::from([(BEARER_SCHEME_NAME.to_owned(), scheme)])),
            Some(vec![HashMap::from([(
                BEARER_SCHEME_NAME.to_owned(),
                Vec::new(),
            )])]),
        )
    } else {
        (None, None)
    };

    AgentCard {
        name: config.name.clone(),
        description: config.description.clone(),
        version: config.version.clone(),
        supported_interfaces: vec![AgentInterface::new(
            config.url.as_str(),
            TRANSPORT_PROTOCOL_JSONRPC,
        )],
        capabilities: AgentCapabilities {
            streaming: Some(true),
            push_notifications: Some(false),
            extensions,
            extended_agent_card: Some(false),
        },
        default_input_modes: vec!["text/plain".to_owned()],
        default_output_modes: vec!["text/plain".to_owned()],
        skills: config
            .skills
            .iter()
            .map(|s| AgentSkill {
                id: s.id.clone(),
                name: s.name.clone(),
                description: s.description.clone(),
                tags: s.tags.clone(),
                examples: (!s.examples.is_empty()).then(|| s.examples.clone()),
                input_modes: None,
                output_modes: None,
                security_requirements: None,
            })
            .collect(),
        provider: None,
        documentation_url: None,
        icon_url: None,
        security_schemes,
        security_requirements,
        signatures: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> AgentCardConfig {
        AgentCardConfig::new(
            "echo",
            "Echoes",
            Url::parse("http://localhost:8080/").unwrap(),
            "1.2.3",
        )
        .with_skill(SkillConfig::new("echo", "Echo", "Repeats input"))
    }

    #[test]
    fn card_advertises_streaming_no_push_and_the_configured_url() {
        let card = build_card(&config(), false);
        assert_eq!(card.capabilities.streaming, Some(true));
        assert_eq!(card.capabilities.push_notifications, Some(false));
        assert_eq!(card.supported_interfaces.len(), 1);
        assert_eq!(card.supported_interfaces[0].url, "http://localhost:8080/");
        assert_eq!(card.skills[0].id, "echo");
        assert!(card.security_schemes.is_none());
        assert!(card.security_requirements.is_none());
    }

    #[test]
    fn bearer_scheme_is_advertised_only_when_enforced() {
        let card = build_card(&config(), true);
        let schemes = card.security_schemes.unwrap();
        assert!(matches!(
            schemes.get("bearer"),
            Some(SecurityScheme::HttpAuth(h)) if h.scheme == "Bearer"
        ));
        assert_eq!(card.security_requirements.unwrap().len(), 1);
    }

    #[test]
    fn extensions_are_declared_when_configured() {
        let mut ext = ExtensionConfig::new("https://example.com/ext/v1");
        ext.required = true;
        let card = build_card(&config().with_extension(ext), false);
        let exts = card.capabilities.extensions.unwrap();
        assert_eq!(exts[0].uri, "https://example.com/ext/v1");
        assert_eq!(exts[0].required, Some(true));
        assert!(
            build_card(&config(), false)
                .capabilities
                .extensions
                .is_none()
        );
    }
}
