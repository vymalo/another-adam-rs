//! The frontmatter of a skill: [Agent Skills](https://agentskills.io/specification).

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

use super::scalar::{lenient_map, space_list};

/// The frontmatter of `SKILL.md` (or of a flat `skills/<name>.md`).
///
/// Everything is optional here so that the loader can be lenient the way the Agent Skills
/// client guide asks: it decides what a missing `name` or `description` costs. Keys the spec
/// does not define (`argument-hint`, `globs`, ... in the wild) land in
/// [`extra`](Self::extra) without a diagnostic.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct SkillFrontmatter {
    /// 1 to 64 characters of `a-z0-9-`, no leading, trailing or doubled hyphen, equal to the
    /// directory name.
    pub name: Option<String>,
    /// What the skill does and when to use it: 1 to 1024 characters.
    pub description: Option<String>,
    /// A licence name or the name of a bundled licence file.
    pub license: Option<String>,
    /// Environment requirements: at most 500 characters.
    pub compatibility: Option<String>,
    /// Free-form map. Scalars are read as text; a list or a map is kept as its JSON text.
    #[serde(deserialize_with = "lenient_map")]
    pub metadata: BTreeMap<String, String>,
    /// Pre-approved tools (experimental in the spec). Parsed, ignored in v1.
    #[serde(rename = "allowed-tools", deserialize_with = "space_list")]
    pub allowed_tools: Vec<String>,
    /// Keys the spec does not define.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
