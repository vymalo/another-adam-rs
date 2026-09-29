//! The frontmatter of an agent file: the root `instructions.md`, a directory subagent's
//! `instructions.md` and a flat subagent (`subagents/x.md`, `x.agent.md`).
//!
//! One schema for all three, and a superset of the Claude Code and GitHub Copilot custom-agent
//! formats, so a file copied from `.claude/agents/x.md` or `.github/agents/x.agent.md` reads
//! unchanged. Keys adam does not model land in [`AgentFrontmatter::extra`].

use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;
use serde::de::{self, Deserializer, Visitor};
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Limits {
    /// Most model calls in one run.
    #[serde(alias = "maxTurns")]
    pub max_turns: Option<u32>,
    /// Most tool calls in one run.
    #[serde(alias = "maxToolCalls")]
    pub max_tool_calls: Option<u32>,
    /// `max_output_tokens` passed to the model.
    #[serde(alias = "maxOutputTokens")]
    pub max_output_tokens: Option<u32>,
    /// Budget for the history sent to the model.
    #[serde(alias = "maxHistoryTokens")]
    pub max_history_tokens: Option<u32>,
    /// Keys this schema does not know.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// One skill advertised on the A2A agent card. **Not** an Agent Skill: the two never map
/// implicitly.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Card {
    /// The card's name; defaults to the agent's.
    pub name: Option<String>,
    /// The card's description; defaults to the agent's.
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct AgentFrontmatter {
    /// The agent's name. Default: the file or directory name (subagents), the composition
    /// root's default (the root agent).
    pub name: Option<String>,
    /// What the agent does. Required on a subagent (it is the tool description the parent
    /// reads); the root agent uses it for the card.
    pub description: Option<String>,
    /// The tools the agent gets, by name: a list or a comma-separated string.
    pub tools: Option<ToolList>,
    /// The model alias, or `inherit`.
    pub model: Option<ModelRef>,
    /// The skills the catalog offers: `all` or a list. Default: every skill of the agent.
    pub skills: Option<SkillSelection>,
    /// Skills whose full body is put into the prompt instead of the catalog.
    pub preload_skills: Option<Vec<String>>,
    /// The limits of the loop, with Claude Code's `maxTurns` folded into `max_turns`.
    pub limits: Option<Limits>,
    /// Claude Code's spelling of `limits.max_turns`; the loader moves it into `limits`.
    #[serde(rename = "maxTurns")]
    pub(crate) claude_max_turns: Option<u32>,
    /// Defaults for the `{{placeholders}}` of the body. Scalars only, read as text.
    #[serde(deserialize_with = "scalar_map")]
    pub vars: BTreeMap<String, String>,
    /// The A2A card (root agent only).
    pub card: Option<Card>,
    /// The agent-card URL of a remote subagent (A2A). Makes the file a remote subagent.
    pub a2a: Option<String>,
    /// How to authenticate to a remote subagent: `bearer:ENV_VAR`.
    pub auth: Option<String>,
    /// Free-form map, passed through (an Agent Skills and Copilot convention). Scalars are
    /// read as text; a list or a map is kept as its JSON text.
    #[serde(deserialize_with = "lenient_map")]
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
