//! The per-model profiles of `MODEL_ALIASES_FILE` ([ADR 0063](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0063-a-reasoning-effort-is-one-setting.md),
//! [ADR 0064](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0064-model-profiles-reasoning-effort-and-output-cap-follow-the-model.md)):
//! which reasoning levels a model takes and how they are sent, its output cap, the members merged into
//! its requests, and the named presets.
//!
//! The types are pure data: no file is read here. [`ModelProfiles::parse_json`] parses and validates a
//! document and reports every problem it finds, naming the profile (or preset) and the key; the binary
//! that reads the file adds the file's path and stops with exit 78.
//!
//! The document is **JSON only for now**: the workspace has no YAML parser, and adding one would
//! collide with another change. A file that is not JSON is one problem, not a crash.
//!
//! ```json
//! {
//!   "presets": { "quick": { "reasoningEffort": "low" } },
//!   "profiles": {
//!     "large-coder": {
//!       "preset": "quick",
//!       "reasoningEfforts": ["low", "medium", "high", "max"],
//!       "reasoningWire": "reasoning_effort",
//!       "maxOutputTokensLimit": 131072
//!     },
//!     "small-chat": {
//!       "reasoningEfforts": [],
//!       "maxOutputTokensLimit": 65536,
//!       "extraBody": { "enable_thinking": false }
//!     }
//!   }
//! }
//! ```
//!
//! A profile is keyed by the model's id, the name the request's `model` carries. Absent or empty
//! `reasoningEfforts` means the model takes no reasoning setting; `reasoningEfforts: []` says so
//! explicitly. Unknown members are ignored, so a file may carry facts a later slice will read.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How much a model reasons for one call, from the least to the most: the gateway's observed levels,
/// a closed list on purpose ([ADR 0063](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0063-a-reasoning-effort-is-one-setting.md)
/// decision 1). A new variant must fail to compile where a level is matched.
///
/// `None` is a real level: some models reason unless it is sent. An **absent** value sends nothing and
/// is the model's own default, which may reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    /// Do not reason (some models reason unless told this).
    None,
    /// The least reasoning a model takes.
    Minimal,
    /// Low.
    Low,
    /// Medium.
    Medium,
    /// High.
    High,
    /// Extra high; a gateway may refuse it (Gemini answers 400 for `xhigh`).
    XHigh,
    /// The most.
    Max,
}

impl ReasoningEffort {
    /// The seven levels, lowest first.
    pub const ALL: [Self; 7] = [
        Self::None,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
        Self::Max,
    ];

    /// The word on the wire: `none`, `minimal`, `low`, `medium`, `high`, `xhigh` or `max`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
}

impl fmt::Display for ReasoningEffort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A word that is none of the seven levels.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not a reasoning effort: use none, minimal, low, medium, high, xhigh or max")]
pub struct NotAnEffort(pub String);

impl FromStr for ReasoningEffort {
    type Err = NotAnEffort;

    /// Exactly the seven words, in lower case: no near spelling is rounded to a level.
    fn from_str(word: &str) -> Result<Self, NotAnEffort> {
        Self::ALL
            .into_iter()
            .find(|effort| effort.as_str() == word)
            .ok_or_else(|| NotAnEffort(word.to_owned()))
    }
}

/// How a level reaches the model: the closed set of shapes a gateway was observed to take
/// ([ADR 0063](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0063-a-reasoning-effort-is-one-setting.md)
/// decision 2). A fourth shape is a new decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningWire {
    /// `{"reasoning_effort": "<level>"}`: the only wire verified on the deployment's gateway.
    ReasoningEffort,
    /// `{"reasoning": {"effort": "<level>"}}`: for another gateway, unverified here.
    ReasoningObject,
    /// `{"thinking": {"type": "enabled", "budget_tokens": <n>}}`: a token budget per level, for another
    /// gateway, unverified here.
    ThinkingBudget,
}

impl ReasoningWire {
    /// The three wires, in the order of the decision.
    pub const ALL: [Self; 3] = [
        Self::ReasoningEffort,
        Self::ReasoningObject,
        Self::ThinkingBudget,
    ];

    /// The word in the file: `reasoning_effort`, `reasoning_object` or `thinking_budget`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReasoningEffort => "reasoning_effort",
            Self::ReasoningObject => "reasoning_object",
            Self::ThinkingBudget => "thinking_budget",
        }
    }
}

impl fmt::Display for ReasoningWire {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A word that is none of the three wires.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not a reasoning wire: use reasoning_effort, reasoning_object or thinking_budget")]
pub struct NotAWire(pub String);

impl FromStr for ReasoningWire {
    type Err = NotAWire;

    fn from_str(word: &str) -> Result<Self, NotAWire> {
        Self::ALL
            .into_iter()
            .find(|wire| wire.as_str() == word)
            .ok_or_else(|| NotAWire(word.to_owned()))
    }
}

/// The output cap a model call asks for when nothing else sets one ([ADR 0064](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0064-model-profiles-reasoning-effort-and-output-cap-follow-the-model.md)):
/// the median of the flagship models' documented maximums in 2026, written as 128,000, the lowest
/// reading of 128K. The assembly resolves it per model and clamps it to the model's
/// `maxOutputTokensLimit` and context window; the loop's `Limits` default is the same value for a
/// host that builds an agent by hand.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 128_000;

/// The members of a request body the runtime owns: a profile's `extraBody` may not set one.
pub const RESERVED_EXTRA_BODY_KEYS: [&str; 8] = [
    "model",
    "messages",
    "tools",
    "tool_choice",
    "stream",
    "reasoning_effort",
    "max_tokens",
    "max_completion_tokens",
];

/// One model's profile: the reasoning levels it takes and how they are sent ([ADR 0063](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0063-a-reasoning-effort-is-one-setting.md)),
/// its output cap, the members merged into its requests only, and the preset it starts from
/// ([ADR 0064](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0064-model-profiles-reasoning-effort-and-output-cap-follow-the-model.md)).
/// Every member is optional; a profile may set only a cap or only an extra body. Unknown members are
/// ignored.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ModelProfile {
    /// The levels this model takes, lowest first, each once. Absent or empty: it takes none.
    pub reasoning_efforts: Option<Vec<ReasoningEffort>>,
    /// How a level is sent. Required with at least one level, and useless without one.
    pub reasoning_wire: Option<ReasoningWire>,
    /// The token budget of **every** declared level, for the `thinking_budget` wire only.
    pub reasoning_budgets: Option<BTreeMap<ReasoningEffort, u32>>,
    /// The model's own level, one of [`reasoning_efforts`](Self::reasoning_efforts).
    pub reasoning_effort: Option<ReasoningEffort>,
    /// The output cap asked of this model.
    pub max_output_tokens: Option<u32>,
    /// The model's own documented maximum for one request: a cap above it is refused.
    pub max_output_tokens_limit: Option<u32>,
    /// Members merged into this model's requests only; never one the runtime owns
    /// ([`RESERVED_EXTRA_BODY_KEYS`]).
    pub extra_body: Option<serde_json::Map<String, Value>>,
    /// The name of the preset this profile starts from; the profile's own keys win.
    pub preset: Option<String>,
    /// The model a run of this model continues on, once, when the endpoint refuses this one as
    /// locked (ADR 0064, amended 2026-11-23): another profile of the document. An agent's
    /// `model_fallback`, then its preset's, win over it.
    pub fallback: Option<String>,
}

/// A named bundle a profile (or an agent's `model_preset`) starts from.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Preset {
    /// The reasoning level the preset sets.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// The output cap the preset sets.
    pub max_output_tokens: Option<u32>,
    /// The model alias a run continues on when its model is locked, for an agent that names this
    /// preset and no `model_fallback` of its own.
    pub fallback: Option<String>,
}

/// The whole document: the profiles by model id, and the presets by name.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ModelProfiles {
    /// The profile of each model, keyed by the id the request's `model` carries.
    pub profiles: BTreeMap<String, ModelProfile>,
    /// The named presets a profile or an agent's `model_preset` may name.
    pub presets: BTreeMap<String, Preset>,
}

impl ModelProfiles {
    /// Whether the document names no profile and no preset.
    pub fn is_empty(&self) -> bool {
        self.profiles.is_empty() && self.presets.is_empty()
    }

    /// The profile of `model`, when the deployment declared one.
    pub fn profile(&self, model: &str) -> Option<&ModelProfile> {
        self.profiles.get(model)
    }

    /// The preset named `name`, when it is declared.
    pub fn preset(&self, name: &str) -> Option<&Preset> {
        self.presets.get(name)
    }

    /// The preset the profile of `model` starts from, when it names a declared one.
    pub fn preset_of(&self, model: &str) -> Option<&Preset> {
        let name = self.profile(model)?.preset.as_deref()?;
        self.preset(name)
    }

    /// Parse and validate a document, JSON only for now.
    ///
    /// Every problem is a line that names the profile (or preset) and the key it is about, but not
    /// the file: the caller adds the path. A document with any problem is refused whole, so a
    /// process never runs on half a file.
    ///
    /// # Errors
    ///
    /// The problems, one per line, when the text is not JSON, is not an object, or breaks a rule of
    /// [ADR 0063](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0063-a-reasoning-effort-is-one-setting.md)
    /// or [ADR 0064](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0064-model-profiles-reasoning-effort-and-output-cap-follow-the-model.md).
    pub fn parse_json(text: &str) -> Result<Self, Vec<String>> {
        let value: Value = match serde_json::from_str(text) {
            Ok(value) => value,
            Err(e) => {
                return Err(vec![format!(
                    "is not valid JSON ({} at line {} column {})",
                    match e.classify() {
                        serde_json::error::Category::Eof => "it ends too soon",
                        _ => "it does not parse",
                    },
                    e.line(),
                    e.column()
                )]);
            }
        };
        let Value::Object(mut root) = value else {
            return Err(vec![
                "must be a JSON object with `profiles` and `presets`".into(),
            ]);
        };
        let mut problems = Vec::new();
        let mut presets = BTreeMap::new();
        for (name, value) in take_object(&mut root, "presets", &mut problems) {
            let raw: RawPreset = match serde_json::from_value(value) {
                Ok(raw) => raw,
                Err(e) => {
                    problems.push(format!("presets.{name:?}: {e}"));
                    continue;
                }
            };
            if let Some(preset) = convert_preset(&name, raw, &mut problems) {
                presets.insert(name, preset);
            }
        }
        let mut profiles = BTreeMap::new();
        for (alias, value) in take_object(&mut root, "profiles", &mut problems) {
            let raw: RawProfile = match serde_json::from_value(value) {
                Ok(raw) => raw,
                Err(e) => {
                    problems.push(format!("profiles.{alias:?}: {e}"));
                    continue;
                }
            };
            if let Some(profile) = convert_profile(&alias, raw, &mut problems) {
                profiles.insert(alias, profile);
            }
        }
        if !problems.is_empty() {
            return Err(problems);
        }
        let profiles = Self { profiles, presets };
        let problems = profiles.validate();
        if problems.is_empty() {
            Ok(profiles)
        } else {
            Err(problems)
        }
    }

    /// The cross-field rules: the effort/wire pairing, the budgets, the caps, the preset references
    /// and the extra body's reserved members. Conversion problems are already gone when this runs.
    fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();
        for (alias, profile) in &self.profiles {
            let at = |key: &str| format!("profiles.{alias:?}.{key}");
            if let Some(fallback) = &profile.fallback {
                if fallback == alias {
                    problems.push(format!(
                        "{}: a model cannot be its own fallback",
                        at("fallback")
                    ));
                } else if !self.profiles.contains_key(fallback) {
                    problems.push(format!(
                        "{}: {fallback:?} has no profile; a fallback is another model of this document",
                        at("fallback")
                    ));
                }
            }
            let efforts = profile.reasoning_efforts.as_deref().unwrap_or(&[]);
            match (profile.reasoning_wire, efforts.is_empty()) {
                (None, false) => problems.push(format!(
                    "{}: reasoning efforts need a reasoning wire",
                    at("reasoningWire")
                )),
                (Some(_), true) => problems.push(format!(
                    "{}: a reasoning wire needs at least one reasoning effort",
                    at("reasoningWire")
                )),
                _ => {}
            }
            if profile.reasoning_wire == Some(ReasoningWire::ThinkingBudget) {
                // The shape has no disabled form: a level that means "do not reason" would send
                // `thinking: {type: enabled, ...}`, the opposite.
                if efforts.contains(&ReasoningEffort::None) {
                    problems.push(format!(
                        "{}: {:?} is not available with the `thinking_budget` wire, which has no disabled form: leave it out or use another wire",
                        at("reasoningEfforts"),
                        ReasoningEffort::None.as_str()
                    ));
                }
                match profile.reasoning_budgets.as_ref() {
                    None => problems.push(format!(
                        "{}: the `thinking_budget` wire needs a budget for every declared level",
                        at("reasoningBudgets")
                    )),
                    Some(budgets) => {
                        for level in efforts {
                            // `none` is refused above: asking for its budget would be a second
                            // problem telling the user to do what the first forbids.
                            if *level != ReasoningEffort::None && !budgets.contains_key(level) {
                                problems.push(format!(
                                    "{}: {:?} is declared but has no budget",
                                    at("reasoningBudgets"),
                                    level.as_str()
                                ));
                            }
                        }
                        for level in budgets.keys() {
                            if !efforts.contains(level) {
                                problems.push(format!(
                                    "{}: {:?} is not a declared reasoning effort",
                                    at("reasoningBudgets"),
                                    level.as_str()
                                ));
                            }
                        }
                    }
                }
            } else if profile.reasoning_budgets.is_some() {
                problems.push(format!(
                    "{}: budgets need the `thinking_budget` wire",
                    at("reasoningBudgets")
                ));
            }
            if let Some(effort) = profile.reasoning_effort
                && !efforts.contains(&effort)
            {
                problems.push(format!(
                    "{}: {:?} is not one of the model's reasoning efforts",
                    at("reasoningEffort"),
                    effort.as_str()
                ));
            }
            if let (Some(cap), Some(limit)) =
                (profile.max_output_tokens, profile.max_output_tokens_limit)
                && cap > limit
            {
                problems.push(format!(
                    "{} ({cap}) is above {} ({limit})",
                    at("maxOutputTokens"),
                    at("maxOutputTokensLimit")
                ));
            }
            if let Some(body) = &profile.extra_body {
                for key in RESERVED_EXTRA_BODY_KEYS {
                    if body.contains_key(key) {
                        problems.push(format!(
                            "{}: may not set `{key}`: the runtime owns it",
                            at("extraBody")
                        ));
                    }
                }
            }
            if let Some(name) = &profile.preset {
                match self.presets.get(name) {
                    None => problems.push(format!(
                        "{}: {name:?} is not a declared preset",
                        at("preset")
                    )),
                    Some(preset) => {
                        if let Some(effort) = preset.reasoning_effort
                            && !efforts.contains(&effort)
                        {
                            problems.push(format!(
                                "presets.{name:?}.reasoningEffort: {:?} is not one of the reasoning efforts of profiles.{alias:?}",
                                effort.as_str()
                            ));
                        }
                        if let (Some(cap), Some(limit)) =
                            (preset.max_output_tokens, profile.max_output_tokens_limit)
                            && cap > limit
                        {
                            problems.push(format!(
                                "presets.{name:?}.maxOutputTokens ({cap}) is above profiles.{alias:?}.maxOutputTokensLimit ({limit})"
                            ));
                        }
                    }
                }
            }
        }
        problems
    }
}

/// The `presets` or `profiles` map of the document; an absent one is empty, a wrong shape a problem.
fn take_object(
    root: &mut serde_json::Map<String, Value>,
    key: &str,
    problems: &mut Vec<String>,
) -> serde_json::Map<String, Value> {
    match root.remove(key) {
        None => serde_json::Map::new(),
        Some(Value::Object(map)) => map,
        Some(_) => {
            problems.push(format!("{key} must be an object, keyed by name"));
            serde_json::Map::new()
        }
    }
}

/// A profile as written: the enums are words here, so an unknown one names the alias instead of
/// failing somewhere in serde's line and column.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct RawProfile {
    reasoning_efforts: Option<Vec<String>>,
    reasoning_wire: Option<String>,
    reasoning_budgets: Option<BTreeMap<String, u32>>,
    reasoning_effort: Option<String>,
    max_output_tokens: Option<u32>,
    max_output_tokens_limit: Option<u32>,
    extra_body: Option<serde_json::Map<String, Value>>,
    preset: Option<String>,
    fallback: Option<String>,
}

/// A preset as written.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct RawPreset {
    reasoning_effort: Option<String>,
    max_output_tokens: Option<u32>,
    fallback: Option<String>,
}

/// A cap of `0` is no cap a model can honour: a problem, not a value.
fn cap(value: Option<u32>, at: &str, problems: &mut Vec<String>) -> Option<u32> {
    match value {
        Some(0) => {
            problems.push(format!("{at} must be at least 1"));
            None
        }
        value => value,
    }
}

fn convert_preset(name: &str, raw: RawPreset, problems: &mut Vec<String>) -> Option<Preset> {
    let mut ok = true;
    let reasoning_effort = match raw.reasoning_effort {
        None => None,
        Some(word) => match word.parse::<ReasoningEffort>() {
            Ok(effort) => Some(effort),
            Err(e) => {
                problems.push(format!("presets.{name:?}.reasoningEffort: {e}"));
                ok = false;
                None
            }
        },
    };
    let max_output_tokens = cap(
        raw.max_output_tokens,
        &format!("presets.{name:?}.maxOutputTokens"),
        problems,
    );
    if max_output_tokens.is_none() && raw.max_output_tokens.is_some() {
        ok = false;
    }
    ok.then_some(Preset {
        reasoning_effort,
        max_output_tokens,
        fallback: raw.fallback,
    })
}

fn convert_profile(
    alias: &str,
    raw: RawProfile,
    problems: &mut Vec<String>,
) -> Option<ModelProfile> {
    let at = |key: &str| format!("profiles.{alias:?}.{key}");
    let mut ok = true;
    let reasoning_efforts = match raw.reasoning_efforts {
        None => None,
        Some(words) => {
            let mut efforts = Vec::with_capacity(words.len());
            for word in words {
                match word.parse::<ReasoningEffort>() {
                    Ok(effort) => efforts.push(effort),
                    Err(e) => {
                        problems.push(format!("{}: {e}", at("reasoningEfforts")));
                        ok = false;
                    }
                }
            }
            efforts.sort_unstable();
            if let Some(pair) = efforts.windows(2).find(|pair| pair[0] == pair[1]) {
                problems.push(format!(
                    "{}: {:?} is declared twice",
                    at("reasoningEfforts"),
                    pair[0].as_str()
                ));
                ok = false;
            }
            Some(efforts)
        }
    };
    let reasoning_wire = match raw.reasoning_wire {
        None => None,
        Some(word) => match word.parse::<ReasoningWire>() {
            Ok(wire) => Some(wire),
            Err(e) => {
                problems.push(format!("{}: {e}", at("reasoningWire")));
                ok = false;
                None
            }
        },
    };
    let reasoning_budgets = match raw.reasoning_budgets {
        None => None,
        Some(entries) => {
            let mut budgets = BTreeMap::new();
            for (word, tokens) in entries {
                match word.parse::<ReasoningEffort>() {
                    Ok(effort) => {
                        if tokens == 0 {
                            problems.push(format!(
                                "{}: the budget of {:?} must be at least 1",
                                at("reasoningBudgets"),
                                effort.as_str()
                            ));
                            ok = false;
                        }
                        budgets.insert(effort, tokens);
                    }
                    Err(e) => {
                        problems.push(format!("{}: {e}", at("reasoningBudgets")));
                        ok = false;
                    }
                }
            }
            Some(budgets)
        }
    };
    let reasoning_effort = match raw.reasoning_effort {
        None => None,
        Some(word) => match word.parse::<ReasoningEffort>() {
            Ok(effort) => Some(effort),
            Err(e) => {
                problems.push(format!("{}: {e}", at("reasoningEffort")));
                ok = false;
                None
            }
        },
    };
    let max_output_tokens = cap(raw.max_output_tokens, &at("maxOutputTokens"), problems);
    if max_output_tokens.is_none() && raw.max_output_tokens.is_some() {
        ok = false;
    }
    let max_output_tokens_limit = cap(
        raw.max_output_tokens_limit,
        &at("maxOutputTokensLimit"),
        problems,
    );
    if max_output_tokens_limit.is_none() && raw.max_output_tokens_limit.is_some() {
        ok = false;
    }
    ok.then_some(ModelProfile {
        reasoning_efforts,
        reasoning_wire,
        reasoning_budgets,
        reasoning_effort,
        max_output_tokens,
        max_output_tokens_limit,
        extra_body: raw.extra_body,
        preset: raw.preset,
        fallback: raw.fallback,
    })
}

#[cfg(test)]
mod tests {
    use serde::de::DeserializeOwned;
    use serde_json::json;

    use super::*;

    fn roundtrip<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(value: T) {
        let json = serde_json::to_string(&value).expect("serialize");
        let back: T = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(value, back, "via {json}");
    }

    /// The example of ADR 0064, translated to JSON, with an unknown `contextWindow` (a fact of ADR
    /// 0054 slice 11 that this loader ignores).
    fn example() -> serde_json::Value {
        json!({
            "presets": {
                "quick": { "reasoningEffort": "low" },
                "thorough": { "reasoningEffort": "high", "maxOutputTokens": 64000 }
            },
            "profiles": {
                "large-coder": {
                    "preset": "quick",
                    "reasoningEfforts": ["low", "medium", "high", "max"],
                    "reasoningWire": "reasoning_effort",
                    "maxOutputTokensLimit": 131072,
                    "contextWindow": 1000000
                },
                "small-chat": {
                    "reasoningEfforts": [],
                    "maxOutputTokensLimit": 65536,
                    "extraBody": { "enable_thinking": false }
                }
            }
        })
    }

    fn problems_of(value: &serde_json::Value) -> Vec<String> {
        ModelProfiles::parse_json(&value.to_string()).expect_err("refused")
    }

    #[test]
    fn the_efforts_are_seven_words_lowest_first() {
        assert_eq!(ReasoningEffort::ALL.len(), 7);
        for (index, effort) in ReasoningEffort::ALL.into_iter().enumerate() {
            assert_eq!(
                ReasoningEffort::ALL[index],
                effort,
                "ALL is in order at {index}"
            );
            assert_eq!(effort.as_str().parse::<ReasoningEffort>(), Ok(effort));
            assert_eq!(effort.to_string(), effort.as_str());
            assert_eq!(
                serde_json::to_value(effort).unwrap(),
                json!(effort.as_str())
            );
        }
        let words: Vec<&str> = ReasoningEffort::ALL.iter().map(|e| e.as_str()).collect();
        assert_eq!(
            words,
            ["none", "minimal", "low", "medium", "high", "xhigh", "max"]
        );
        assert!(ReasoningEffort::None < ReasoningEffort::Minimal);
        assert!(ReasoningEffort::Minimal < ReasoningEffort::Low);
        assert!(ReasoningEffort::Low < ReasoningEffort::Medium);
        assert!(ReasoningEffort::Medium < ReasoningEffort::High);
        assert!(ReasoningEffort::High < ReasoningEffort::XHigh);
        assert!(ReasoningEffort::XHigh < ReasoningEffort::Max);
    }

    #[test]
    fn a_near_spelling_is_not_an_effort() {
        for bad in [
            "", "None", "NONE", "XHigh", "x-high", " low", "low ", "highest", "off",
        ] {
            assert_eq!(
                bad.parse::<ReasoningEffort>(),
                Err(NotAnEffort(bad.to_owned())),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn the_wires_are_three_words() {
        let words: Vec<&str> = ReasoningWire::ALL.iter().map(|w| w.as_str()).collect();
        assert_eq!(
            words,
            ["reasoning_effort", "reasoning_object", "thinking_budget"]
        );
        for wire in ReasoningWire::ALL {
            assert_eq!(wire.as_str().parse::<ReasoningWire>(), Ok(wire));
            assert_eq!(serde_json::to_value(wire).unwrap(), json!(wire.as_str()));
        }
        assert_eq!(
            "reasoning".parse::<ReasoningWire>(),
            Err(NotAWire("reasoning".into()))
        );
    }

    #[test]
    fn a_profile_roundtrips_and_ignores_unknown_members() {
        let profile = ModelProfile {
            reasoning_efforts: Some(vec![ReasoningEffort::Low, ReasoningEffort::High]),
            reasoning_wire: Some(ReasoningWire::ThinkingBudget),
            reasoning_budgets: Some(BTreeMap::from([
                (ReasoningEffort::Low, 1024),
                (ReasoningEffort::High, 8192),
            ])),
            reasoning_effort: Some(ReasoningEffort::Low),
            max_output_tokens: Some(64_000),
            max_output_tokens_limit: Some(131_072),
            extra_body: Some(serde_json::Map::from_iter([(
                "enable_thinking".to_owned(),
                json!(false),
            )])),
            preset: Some("quick".into()),
            fallback: Some("other".into()),
        };
        roundtrip(profile.clone());
        assert_eq!(
            serde_json::to_value(&profile).unwrap()["reasoningEfforts"],
            json!(["low", "high"])
        );
        // An unknown member (a fact of ADR 0054 slice 11) is ignored, not refused.
        let with_unknown = json!({
            "reasoningEfforts": ["low"],
            "reasoningWire": "reasoning_effort",
            "contextWindow": 1000000,
            "temperature": 0.2
        });
        let parsed: ModelProfile = serde_json::from_value(with_unknown).unwrap();
        assert_eq!(parsed.reasoning_efforts, Some(vec![ReasoningEffort::Low]));
    }

    #[test]
    fn the_0064_example_loads_and_sorts_its_levels() {
        let profiles = ModelProfiles::parse_json(&example().to_string()).expect("valid");
        assert_eq!(profiles.profiles.len(), 2);
        let large = profiles.profile("large-coder").expect("large-coder");
        assert_eq!(
            large.reasoning_efforts.as_deref(),
            Some(
                &[
                    ReasoningEffort::Low,
                    ReasoningEffort::Medium,
                    ReasoningEffort::High,
                    ReasoningEffort::Max
                ][..]
            )
        );
        assert_eq!(large.reasoning_wire, Some(ReasoningWire::ReasoningEffort));
        assert_eq!(large.max_output_tokens_limit, Some(131_072));
        assert_eq!(
            profiles
                .preset_of("large-coder")
                .map(|p| p.reasoning_effort),
            Some(Some(ReasoningEffort::Low))
        );
        assert_eq!(
            profiles.profile("small-chat").unwrap().reasoning_efforts,
            Some(vec![])
        );
        assert!(profiles.preset_of("small-chat").is_none());
        assert!(!profiles.is_empty());
        assert!(ModelProfiles::default().is_empty());
        // An unsorted list is stored lowest first.
        let sorted = json!({
            "profiles": {
                "m": { "reasoningEfforts": ["max", "low", "none"], "reasoningWire": "reasoning_effort" }
            }
        });
        let profiles = ModelProfiles::parse_json(&sorted.to_string()).unwrap();
        assert_eq!(
            profiles.profile("m").unwrap().reasoning_efforts.as_deref(),
            Some(
                &[
                    ReasoningEffort::None,
                    ReasoningEffort::Low,
                    ReasoningEffort::Max
                ][..]
            )
        );
    }

    #[test]
    fn a_level_outside_the_vocabulary_names_the_alias_and_the_key() {
        let bad = json!({
            "profiles": { "gpt-x": { "reasoningEfforts": ["ultra"], "reasoningWire": "reasoning_effort" } }
        });
        let problems = problems_of(&bad);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains(r#"profiles."gpt-x".reasoningEfforts"#),
            "{problems:?}"
        );
        assert!(
            problems[0].contains("not a reasoning effort"),
            "{problems:?}"
        );
    }

    #[test]
    fn an_unknown_wire_is_refused() {
        let bad = json!({
            "profiles": { "m": { "reasoningEfforts": ["low"], "reasoningWire": "thinking" } }
        });
        let problems = problems_of(&bad);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains(r#"profiles."m".reasoningWire"#),
            "{problems:?}"
        );
    }

    #[test]
    fn a_duplicate_level_is_refused() {
        let bad = json!({
            "profiles": { "m": { "reasoningEfforts": ["low", "high", "low"], "reasoningWire": "reasoning_effort" } }
        });
        let problems = problems_of(&bad);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("declared twice"), "{problems:?}");
        assert!(problems[0].contains(r#""low""#), "{problems:?}");
    }

    #[test]
    fn budgets_belong_to_the_thinking_budget_wire_and_cover_every_level() {
        // Budgets without the wire.
        let bad = json!({
            "profiles": { "m": {
                "reasoningEfforts": ["low"],
                "reasoningWire": "reasoning_effort",
                "reasoningBudgets": { "low": 1024 }
            } }
        });
        let problems = problems_of(&bad);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains(r#"profiles."m".reasoningBudgets"#),
            "{problems:?}"
        );

        // A declared level with no budget, and a budget for a level not declared.
        let bad = json!({
            "profiles": { "m": {
                "reasoningEfforts": ["low", "high"],
                "reasoningWire": "thinking_budget",
                "reasoningBudgets": { "low": 1024, "max": 4096 }
            } }
        });
        let problems = problems_of(&bad);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(
            problems.iter().any(|p| p.contains(r#""high""#)),
            "{problems:?}"
        );
        assert!(
            problems.iter().any(|p| p.contains(r#""max""#)),
            "{problems:?}"
        );

        // The wire with no budgets at all.
        let bad = json!({
            "profiles": { "m": {
                "reasoningEfforts": ["low"],
                "reasoningWire": "thinking_budget"
            } }
        });
        let problems = problems_of(&bad);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("thinking_budget"), "{problems:?}");

        // A budget of 0.
        let bad = json!({
            "profiles": { "m": {
                "reasoningEfforts": ["low"],
                "reasoningWire": "thinking_budget",
                "reasoningBudgets": { "low": 0 }
            } }
        });
        let problems = problems_of(&bad);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("at least 1"), "{problems:?}");

        // A budget for a level that is not declared, with the wire on.
        let bad = json!({
            "profiles": { "m": {
                "reasoningEfforts": ["low"],
                "reasoningWire": "thinking_budget",
                "reasoningBudgets": { "low": 1024, "max": 4096 }
            } }
        });
        let problems = problems_of(&bad);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains(r#""max""#), "{problems:?}");

        // A budget with no wire at all.
        let bad = json!({ "profiles": { "m": { "reasoningBudgets": { "low": 1024 } } } });
        let problems = problems_of(&bad);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("thinking_budget"), "{problems:?}");
    }

    #[test]
    fn the_thinking_budget_wire_refuses_the_level_none() {
        // A valid file: the wire's levels, each with its budget, and no `none`.
        let good = json!({
            "profiles": { "m": {
                "reasoningEfforts": ["low", "high"],
                "reasoningWire": "thinking_budget",
                "reasoningBudgets": { "low": 1024, "high": 8192 }
            } }
        });
        let profiles = ModelProfiles::parse_json(&good.to_string()).expect("valid");
        assert_eq!(
            profiles.profile("m").unwrap().reasoning_efforts.as_deref(),
            Some(&[ReasoningEffort::Low, ReasoningEffort::High][..])
        );

        // `none` would send `thinking: {type: enabled, ...}`: the shape has no disabled form.
        let bad = json!({
            "profiles": { "m": {
                "reasoningEfforts": ["none", "low"],
                "reasoningWire": "thinking_budget",
                "reasoningBudgets": { "none": 1, "low": 1024 }
            } }
        });
        let problems = problems_of(&bad);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains(r#"profiles."m".reasoningEfforts"#),
            "{problems:?}"
        );
        assert!(problems[0].contains(r#""none""#), "{problems:?}");
        assert!(problems[0].contains("thinking_budget"), "{problems:?}");
        assert!(problems[0].contains("no disabled form"), "{problems:?}");
        assert!(problems[0].contains("another wire"), "{problems:?}");

        // The natural bad file has no budget for `none`: one problem, the refusal, never a
        // second telling the user to budget a level the wire forbids.
        let natural = json!({
            "profiles": { "m": {
                "reasoningEfforts": ["none", "low"],
                "reasoningWire": "thinking_budget",
                "reasoningBudgets": { "low": 1024 }
            } }
        });
        let problems = problems_of(&natural);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("no disabled form"), "{problems:?}");
        assert!(!problems[0].contains("no budget"), "{problems:?}");
    }

    #[test]
    fn an_effort_outside_the_models_levels_is_refused() {
        let bad = json!({
            "profiles": { "m": {
                "reasoningEfforts": ["low"],
                "reasoningWire": "reasoning_effort",
                "reasoningEffort": "max"
            } }
        });
        let problems = problems_of(&bad);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains(r#"profiles."m".reasoningEffort"#),
            "{problems:?}"
        );
        // A model that takes none refuses any effort, including through a preset.
        let bad = json!({
            "presets": { "loud": { "reasoningEffort": "max" } },
            "profiles": { "m": { "reasoningEfforts": [], "preset": "loud" } }
        });
        let problems = problems_of(&bad);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains(r#"presets."loud".reasoningEffort"#),
            "{problems:?}"
        );
    }

    #[test]
    fn an_unknown_preset_reference_is_refused() {
        let bad = json!({ "profiles": { "m": { "preset": "nowhere" } } });
        let problems = problems_of(&bad);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains(r#"profiles."m".preset"#),
            "{problems:?}"
        );
        assert!(problems[0].contains("nowhere"), "{problems:?}");
    }

    #[test]
    fn efforts_need_a_wire_and_a_wire_needs_efforts() {
        let bad = json!({ "profiles": { "m": { "reasoningEfforts": ["low"] } } });
        let problems = problems_of(&bad);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains(r#"profiles."m".reasoningWire"#),
            "{problems:?}"
        );

        let bad = json!({ "profiles": { "m": { "reasoningWire": "reasoning_effort" } } });
        let problems = problems_of(&bad);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("at least one"), "{problems:?}");

        // `[]` with no wire is the model saying it takes none: valid.
        let good = json!({ "profiles": { "m": { "reasoningEfforts": [] } } });
        assert!(ModelProfiles::parse_json(&good.to_string()).is_ok());
    }

    #[test]
    fn a_cap_above_the_limit_or_zero_is_refused() {
        let bad = json!({
            "profiles": { "m": { "maxOutputTokens": 200000, "maxOutputTokensLimit": 131072 } }
        });
        let problems = problems_of(&bad);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("200000"), "{problems:?}");
        assert!(problems[0].contains("131072"), "{problems:?}");

        for key in ["maxOutputTokens", "maxOutputTokensLimit"] {
            let bad = json!({ "profiles": { "m": { key: 0 } } });
            let problems = problems_of(&bad);
            assert_eq!(problems.len(), 1, "{key}: {problems:?}");
            assert!(problems[0].contains("at least 1"), "{key}: {problems:?}");
        }

        // A preset's cap above the model's limit, through the profile that names it.
        let bad = json!({
            "presets": { "huge": { "maxOutputTokens": 200000 } },
            "profiles": { "m": { "preset": "huge", "maxOutputTokensLimit": 131072 } }
        });
        let problems = problems_of(&bad);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains(r#"presets."huge".maxOutputTokens"#),
            "{problems:?}"
        );
    }

    #[test]
    fn an_extra_body_may_not_set_a_member_the_runtime_owns() {
        for key in RESERVED_EXTRA_BODY_KEYS {
            let bad = json!({ "profiles": { "m": { "extraBody": { key: 1 } } } });
            let problems = problems_of(&bad);
            assert_eq!(problems.len(), 1, "{key}: {problems:?}");
            assert!(problems[0].contains(key), "{key}: {problems:?}");
            assert!(problems[0].contains("runtime owns"), "{key}: {problems:?}");
        }
        // A member the runtime does not own is the deployment's.
        let good = json!({
            "profiles": { "m": { "extraBody": { "enable_thinking": false, "chat_template_kwargs": {} } } }
        });
        assert!(ModelProfiles::parse_json(&good.to_string()).is_ok());
    }

    #[test]
    fn a_profile_with_only_a_cap_or_an_extra_body_is_valid() {
        let good = json!({
            "profiles": {
                "capped": { "maxOutputTokens": 32000 },
                "flagged": { "extraBody": { "thinking": { "type": "enabled" } } },
                "bare": {}
            }
        });
        let profiles = ModelProfiles::parse_json(&good.to_string()).unwrap();
        assert_eq!(profiles.profiles.len(), 3);
        assert_eq!(profiles.profile("bare"), Some(&ModelProfile::default()));
    }

    #[test]
    fn a_document_that_is_not_json_or_not_an_object_is_refused() {
        for bad in ["", "{oops", "[]", "\"profiles\"", "null"] {
            let problems = ModelProfiles::parse_json(bad).expect_err(bad);
            assert_eq!(problems.len(), 1, "{bad:?}: {problems:?}");
        }
        let problems = ModelProfiles::parse_json("{oops").unwrap_err();
        assert!(problems[0].contains("not valid JSON"), "{problems:?}");
        // `profiles` and `presets` must be objects.
        let problems = ModelProfiles::parse_json(r#"{"profiles": []}"#).unwrap_err();
        assert!(
            problems[0].contains("profiles must be an object"),
            "{problems:?}"
        );
        // An empty document is a valid, empty set.
        assert!(ModelProfiles::parse_json("{}").unwrap().is_empty());
    }
}
