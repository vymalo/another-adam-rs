//! The frontmatter of an agent file: the root `instructions.md`, a directory subagent's
//! `instructions.md` and a flat subagent (`subagents/x.md`, `x.agent.md`).
//!
//! One schema for all three, and a superset of the Claude Code and GitHub Copilot custom-agent
//! formats, so a file copied from `.claude/agents/x.md` or `.github/agents/x.agent.md` reads
//! unchanged. Keys adam does not model land in [`AgentFrontmatter::extra`].

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;

use super::scalar::{StringOrSeq, lenient_map, scalar_map};

/// Which tools an agent gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolList {
    /// `*` (or `["*"]`): every tool registered for the agent.
    All,
    /// Exactly these names. An empty list means no tools.
    Named(Vec<String>),
}

impl Serialize for ToolList {
    /// `"*"` or the list, the spellings [`Deserialize`] reads back.
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::All => s.serialize_str("*"),
            Self::Named(names) => names.serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for ToolList {
    /// Both spellings of Claude Code and Copilot: `tools: a, b` and `tools: [a, b]`.
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let names = StringOrSeq::deserialize(d)?.into_comma_list();
        Ok(if names.iter().any(|n| n == "*") {
            Self::All
        } else {
            Self::Named(names)
        })
    }
}

/// Which skills an agent's catalog offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillSelection {
    /// `all`: every skill under the agent's own `skills/`.
    All,
    /// Only these.
    Named(Vec<String>),
}

impl Serialize for SkillSelection {
    /// `"all"` or the list, the spellings [`Deserialize`] reads back.
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::All => s.serialize_str("all"),
            Self::Named(names) => names.serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for SkillSelection {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(match StringOrSeq::deserialize(d)? {
            StringOrSeq::Text(t) if t.trim() == "all" => Self::All,
            other => Self::Named(other.into_comma_list()),
        })
    }
}

/// The model an agent uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelRef {
    /// `inherit`: the parent's model (the default for a subagent).
    Inherit,
    /// A gateway alias. Never a key or an endpoint: those come from the environment.
    Alias(String),
}

impl Serialize for ModelRef {
    /// `"inherit"` or the alias, the spellings [`Deserialize`] reads back.
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Inherit => s.serialize_str("inherit"),
            Self::Alias(alias) => s.serialize_str(alias),
        }
    }
}

impl<'de> Deserialize<'de> for ModelRef {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = ModelRef;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a model alias, or `inherit`")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<ModelRef, E> {
                let v = v.trim();
                if v.is_empty() {
                    return Err(E::custom("the model alias is empty"));
                }
                Ok(if v == "inherit" {
                    ModelRef::Inherit
                } else {
                    ModelRef::Alias(v.to_owned())
                })
            }
        }
        d.deserialize_str(V)
    }
}

/// The limits of the agent loop (`adam_llm_agent::Limits`). Absent fields keep the loop's
/// default. Claude Code's top-level `maxTurns` is read as `max_turns`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct Limits {
    /// Most model calls in one run.
    #[serde(skip_serializing_if = "Option::is_none", alias = "maxTurns")]
    pub max_turns: Option<u32>,
    /// Most tool calls in one run.
    #[serde(skip_serializing_if = "Option::is_none", alias = "maxToolCalls")]
    pub max_tool_calls: Option<u32>,
    /// `max_output_tokens` passed to the model.
    #[serde(skip_serializing_if = "Option::is_none", alias = "maxOutputTokens")]
    pub max_output_tokens: Option<u32>,
    /// Budget for the history sent to the model.
    #[serde(skip_serializing_if = "Option::is_none", alias = "maxHistoryTokens")]
    pub max_history_tokens: Option<u32>,
    /// Keys this schema does not know.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// One skill advertised on the A2A agent card. **Not** an Agent Skill: the two never map
/// implicitly.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct CardSkill {
    /// Stable identifier.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// What the skill does.
    pub description: String,
    /// Free-form tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Example prompts.
    #[serde(default)]
    pub examples: Vec<String>,
    /// Keys this schema does not know.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// The A2A agent card of the root agent (`adam_a2a::AgentCardConfig`, minus the public URL and
/// the version, which the composition root supplies).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct Card {
    /// The card's name; defaults to the agent's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The card's description; defaults to the agent's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The skills the card advertises.
    pub skills: Vec<CardSkill>,
    /// Keys this schema does not know.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// The YAML frontmatter of an agent or subagent file.
///
/// Everything is optional at this level; which keys are required depends on the role of the
/// file (a subagent needs a `description`) and is checked by the loader.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct AgentFrontmatter {
    /// The agent's name. Default: the file or directory name (subagents), the composition
    /// root's default (the root agent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// What the agent does. Required on a subagent (it is the tool description the parent
    /// reads); the root agent uses it for the card.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The tools the agent gets, by name: a list or a comma-separated string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolList>,
    /// The model alias, or `inherit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    /// The skills the catalog offers: `all` or a list. Default: every skill of the agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills: Option<SkillSelection>,
    /// Skills whose full body is put into the prompt instead of the catalog.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preload_skills: Option<Vec<String>>,
    /// The limits of the loop, with Claude Code's `maxTurns` folded into `max_turns`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limits: Option<Limits>,
    /// Claude Code's spelling of `limits.max_turns`; the loader moves it into `limits`.
    #[serde(rename = "maxTurns", skip_serializing)]
    pub(crate) claude_max_turns: Option<u32>,
    /// Defaults for the `{{placeholders}}` of the body. Scalars only, read as text.
    #[serde(
        deserialize_with = "scalar_map",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub vars: BTreeMap<String, String>,
    /// The A2A card (root agent only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card: Option<Card>,
    /// The agent-card URL of a remote subagent (A2A). Makes the file a remote subagent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub a2a: Option<String>,
    /// How to authenticate to a remote subagent: `bearer:ENV_VAR`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,
    /// Free-form map, passed through (an Agent Skills and Copilot convention). Scalars are
    /// read as text; a list or a map is kept as its JSON text.
    #[serde(
        deserialize_with = "lenient_map",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub metadata: BTreeMap<String, String>,
    /// Keys this schema does not know (Claude Code and Copilot keys among them).
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl AgentFrontmatter {
    /// Move Claude's `maxTurns` into `limits.max_turns`. Returns `true` when both were set to
    /// different values (then `limits.max_turns` wins).
    pub(crate) fn fold_claude_max_turns(&mut self) -> bool {
        let Some(claude) = self.claude_max_turns.take() else {
            return false;
        };
        let limits = self.limits.get_or_insert_with(Limits::default);
        match limits.max_turns {
            Some(existing) => existing != claude,
            None => {
                limits.max_turns = Some(claude);
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T>(value: &T) -> T
    where
        T: Serialize + for<'de> Deserialize<'de>,
    {
        serde_json::from_str(&serde_json::to_string(value).unwrap()).unwrap()
    }

    #[test]
    fn the_closed_enums_read_back_what_they_write() {
        for tools in [
            ToolList::All,
            ToolList::Named(vec![]),
            ToolList::Named(vec!["a".into(), "linear__*".into()]),
        ] {
            assert_eq!(round_trip(&tools), tools);
        }
        for skills in [
            SkillSelection::All,
            SkillSelection::Named(vec![]),
            SkillSelection::Named(vec!["all".into()]),
        ] {
            assert_eq!(round_trip(&skills), skills);
        }
        for model in [ModelRef::Inherit, ModelRef::Alias("coder-large".into())] {
            assert_eq!(round_trip(&model), model);
        }
    }

    #[test]
    fn a_full_frontmatter_reads_back_what_it_writes() {
        let front = AgentFrontmatter {
            name: Some("coder".into()),
            description: Some("Codes.".into()),
            tools: Some(ToolList::Named(vec!["a".into()])),
            model: Some(ModelRef::Inherit),
            skills: Some(SkillSelection::All),
            preload_skills: Some(vec!["s".into()]),
            limits: Some(Limits {
                max_turns: Some(3),
                extra: [("other".to_owned(), serde_json::json!({"x": [1, 2.5, null]}))].into(),
                ..Limits::default()
            }),
            vars: [("n".to_owned(), "3".to_owned())].into(),
            card: Some(Card {
                name: Some("c".into()),
                skills: vec![CardSkill {
                    id: "i".into(),
                    name: "n".into(),
                    description: "d".into(),
                    tags: vec!["t".into()],
                    examples: vec![],
                    extra: Default::default(),
                }],
                ..Card::default()
            }),
            metadata: [("owner".to_owned(), "me".to_owned())].into(),
            extra: [("color".to_owned(), serde_json::json!("blue"))].into(),
            ..AgentFrontmatter::default()
        };
        assert_eq!(round_trip(&front), front);
        assert_eq!(
            round_trip(&AgentFrontmatter::default()),
            AgentFrontmatter::default()
        );
        // Absent keys are not written, so the embedded JSON stays small.
        assert_eq!(
            serde_json::to_string(&AgentFrontmatter::default()).unwrap(),
            "{}"
        );
    }
}
