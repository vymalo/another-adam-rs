//! Agent card configuration and its rendering as an A2A 1.0 `AgentCard`.

use std::collections::HashMap;

use a2a::{
    AgentCapabilities, AgentCard, AgentExtension, AgentInterface, AgentSkill,
    HttpAuthSecurityScheme, SecurityScheme, TRANSPORT_PROTOCOL_HTTP_JSON,
    TRANSPORT_PROTOCOL_JSONRPC,
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
    /// under). Clients POST to exactly this URL. The card lists it twice: for
    /// JSON-RPC first, then for HTTP+JSON, whose paths (`/message:send`, ...)
    /// are relative to it.
    pub url: Url,
    /// The agent's own version string (not the protocol version).
    pub version: String,
    /// Skills advertised on the card.
    pub skills: Vec<SkillConfig>,
    /// A2A extensions this agent declares (for example the platform's
    /// release-channels extension). Empty by default; declaring one here only
    /// advertises it, the server does not implement any extension itself.
    pub extensions: Vec<ExtensionConfig>,
    /// What an authenticated caller gets on top of the public card, from `GetExtendedAgentCard`.
    /// `None` (the default): the agent has no extended card, the card says
    /// `extendedAgentCard: false` and the method answers `UnsupportedOperation`.
    pub extended: Option<ExtendedCardConfig>,
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
            extended: None,
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

    /// Give authenticated callers an extended card: the public card plus what `extended` adds
    /// (see [`ExtendedCardConfig`]).
    #[must_use]
    pub fn with_extended_card(mut self, extended: ExtendedCardConfig) -> Self {
        self.extended = Some(extended);
        self
    }

    /// The URIs of the extensions the card declares: the ones a request may activate (see
    /// [`Caller::extensions`](crate::Caller::extensions)). Every request is authenticated, so the
    /// extensions only the extended card declares count too.
    pub fn extension_uris(&self) -> Vec<String> {
        let mut uris: Vec<String> = self.extensions.iter().map(|e| e.uri.clone()).collect();
        for e in self.extended.iter().flat_map(|x| &x.extensions) {
            if !uris.contains(&e.uri) {
                uris.push(e.uri.clone());
            }
        }
        uris
    }
}

/// What the extended agent card adds to the public one, for callers that authenticated
/// (A2A 1.0, `GetExtendedAgentCard`; *verified* 2026-10-07,
/// <https://a2a-protocol.org/latest/specification/> §3.1.11 and §13.3).
///
/// The extended card is the public card with these applied: the description replaced when given,
/// the skills added (a skill with the id of a public one replaces it), the extensions added (one
/// with the URI of a public one replaces it), and `capabilities.extendedAgentCard` true. The
/// specification asks that it hold nothing that would hurt if it leaked (no internal URLs, no
/// credentials): what goes in here is the deployment's decision.
#[derive(Clone, Debug, Default)]
pub struct ExtendedCardConfig {
    /// A longer description, replacing the public one.
    pub description: Option<String>,
    /// Skills only authenticated callers see.
    pub skills: Vec<SkillConfig>,
    /// Extensions only authenticated callers see.
    pub extensions: Vec<ExtensionConfig>,
}

impl ExtendedCardConfig {
    /// An extended card that adds nothing yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the public description.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Add a skill.
    #[must_use]
    pub fn with_skill(mut self, skill: SkillConfig) -> Self {
        self.skills.push(skill);
        self
    }

    /// Add an extension.
    #[must_use]
    pub fn with_extension(mut self, extension: ExtensionConfig) -> Self {
        self.extensions.push(extension);
        self
    }

    /// Whether it adds nothing at all (an empty extended card is not worth declaring).
    pub fn is_empty(&self) -> bool {
        self.description.is_none() && self.skills.is_empty() && self.extensions.is_empty()
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

/// What the server turned on, which the card has to say truthfully.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Flags {
    /// The server enforces bearer tokens: the card advertises the scheme.
    pub bearer: bool,
    /// Push notifications are on (the deployment allowed webhooks).
    pub push: bool,
    /// The extended card is served (configured, and the server authenticates).
    pub extended: bool,
}

fn extension_entry(e: &ExtensionConfig) -> AgentExtension {
    AgentExtension {
        uri: e.uri.clone(),
        description: e.description.clone(),
        required: Some(e.required),
        params: (!e.params.is_empty()).then(|| {
            e.params
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        }),
    }
}

fn skill_entry(s: &SkillConfig) -> AgentSkill {
    AgentSkill {
        id: s.id.clone(),
        name: s.name.clone(),
        description: s.description.clone(),
        tags: s.tags.clone(),
        examples: (!s.examples.is_empty()).then(|| s.examples.clone()),
        input_modes: None,
        output_modes: None,
        security_requirements: None,
    }
}

/// The extended card: the public card with what the config adds applied (see
/// [`ExtendedCardConfig`]).
pub(crate) fn build_extended_card(
    config: &AgentCardConfig,
    extended: &ExtendedCardConfig,
    flags: Flags,
) -> AgentCard {
    let mut card = build_card(config, flags);
    if let Some(description) = &extended.description {
        card.description.clone_from(description);
    }
    for skill in extended.skills.iter().map(skill_entry) {
        match card.skills.iter_mut().find(|s| s.id == skill.id) {
            Some(existing) => *existing = skill,
            None => card.skills.push(skill),
        }
    }
    if !extended.extensions.is_empty() {
        let list = card.capabilities.extensions.get_or_insert_with(Vec::new);
        for extension in extended.extensions.iter().map(extension_entry) {
            match list.iter_mut().find(|e| e.uri == extension.uri) {
                Some(existing) => *existing = extension,
                None => list.push(extension),
            }
        }
    }
    card
}

/// Render the config as the served public card.
///
/// Streaming is on; push notifications and the extended card are on only when the server says
/// so ([`Flags`]); the bearer scheme is advertised exactly when the server enforces bearer
/// tokens. Signatures are not added here ([`crate::CardSigner`] does it).
pub(crate) fn build_card(config: &AgentCardConfig, flags: Flags) -> AgentCard {
    let bearer_auth = flags.bearer;
    let extensions = (!config.extensions.is_empty())
        .then(|| config.extensions.iter().map(extension_entry).collect());

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
        // JSON-RPC first: a client that takes the first interface (the specification's preferred
        // one) is unchanged. HTTP+JSON is at the same base URL (its paths are relative to it).
        supported_interfaces: vec![
            AgentInterface::new(config.url.as_str(), TRANSPORT_PROTOCOL_JSONRPC),
            AgentInterface::new(config.url.as_str(), TRANSPORT_PROTOCOL_HTTP_JSON),
        ],
        capabilities: AgentCapabilities {
            streaming: Some(true),
            push_notifications: Some(flags.push),
            extensions,
            extended_agent_card: Some(flags.extended),
        },
        default_input_modes: vec!["text/plain".to_owned()],
        default_output_modes: vec!["text/plain".to_owned()],
        skills: config.skills.iter().map(skill_entry).collect(),
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
        let card = build_card(&config(), Flags::default());
        assert_eq!(card.capabilities.streaming, Some(true));
        assert_eq!(card.capabilities.push_notifications, Some(false));
        assert_eq!(card.capabilities.extended_agent_card, Some(false));
        let interfaces: Vec<_> = card
            .supported_interfaces
            .iter()
            .map(|i| (i.protocol_binding.as_str(), i.url.as_str()))
            .collect();
        assert_eq!(
            interfaces,
            [
                ("JSONRPC", "http://localhost:8080/"),
                ("HTTP+JSON", "http://localhost:8080/")
            ],
            "JSON-RPC first, HTTP+JSON at the same base URL"
        );
        assert_eq!(card.skills[0].id, "echo");
        assert!(card.security_schemes.is_none());
        assert!(card.security_requirements.is_none());
    }

    #[test]
    fn bearer_scheme_is_advertised_only_when_enforced() {
        let card = build_card(
            &config(),
            Flags {
                bearer: true,
                ..Flags::default()
            },
        );
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
        let card = build_card(&config().with_extension(ext), Flags::default());
        let exts = card.capabilities.extensions.unwrap();
        assert_eq!(exts[0].uri, "https://example.com/ext/v1");
        assert_eq!(exts[0].required, Some(true));
        assert!(
            build_card(&config(), Flags::default())
                .capabilities
                .extensions
                .is_none()
        );
    }

    #[test]
    fn the_capabilities_follow_the_flags() {
        let card = build_card(
            &config(),
            Flags {
                push: true,
                extended: true,
                ..Flags::default()
            },
        );
        assert_eq!(card.capabilities.push_notifications, Some(true));
        assert_eq!(card.capabilities.extended_agent_card, Some(true));
    }

    #[test]
    fn the_extended_card_is_the_public_card_plus_what_is_added() {
        let extended = ExtendedCardConfig::new()
            .with_description("A longer description")
            .with_skill(SkillConfig::new("admin", "Admin", "Only for the signed in"))
            .with_skill(SkillConfig::new(
                "echo",
                "Echo (full)",
                "Replaces the public one",
            ))
            .with_extension(ExtensionConfig::new("https://example.com/internal/v1"));
        let config = config()
            .with_extension(ExtensionConfig::new("https://example.com/public/v1"))
            .with_extended_card(extended.clone());
        let flags = Flags {
            extended: true,
            ..Flags::default()
        };
        let public = build_card(&config, flags);
        let card = build_extended_card(&config, &extended, flags);
        assert_eq!(public.skills.len(), 1, "the public card does not list it");
        assert_eq!(card.description, "A longer description");
        let ids: Vec<_> = card.skills.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["echo", "admin"]);
        assert_eq!(card.skills[0].name, "Echo (full)");
        let uris: Vec<_> = card
            .capabilities
            .extensions
            .unwrap()
            .into_iter()
            .map(|e| e.uri)
            .collect();
        assert_eq!(
            uris,
            [
                "https://example.com/public/v1",
                "https://example.com/internal/v1"
            ]
        );
        assert_eq!(card.capabilities.extended_agent_card, Some(true));
        assert_eq!(
            config.extension_uris(),
            [
                "https://example.com/public/v1",
                "https://example.com/internal/v1"
            ],
            "a caller may activate what only the extended card declares"
        );
    }
}
