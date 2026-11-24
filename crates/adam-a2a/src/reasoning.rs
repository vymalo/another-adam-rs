//! `reasoning/v1`: the reasoning effort a message names for the model that answers.
//!
//! A client that lets the person pick how hard the model thinks names the level **for each
//! message**, in the message's metadata under [`REASONING_EXTENSION`], as `{"effort": "<level>"}`.
//! The card lists the **root** model's levels, lowest first, and the default the folder, the
//! agent's preset or the model's profile resolved; a model whose profile takes no level lists no
//! extension (ADR 0063, decision 5). The vocabulary is [`adam_model::ReasoningEffort`], the one
//! closed list of seven words: this crate keeps no second copy.
//!
//! Two rules keep the reading honest:
//!
//! * **Only an activated extension is read.** A message whose request did not name the extension
//!   carries no effort, whatever its metadata says. Activation is the caller's check
//!   ([`Caller::has_extension`](crate::Caller::has_extension)), as for every extension; the runtime
//!   wires it (PR B).
//! * **A level outside the model's set is refused, fail closed.** The entry must be an object with
//!   a string `effort` that is one of the seven words **and** one the agent's model takes; anything
//!   else is a sentence naming the agent and its levels, to be the `InvalidParams` (`-32602`)
//!   message before anything is written. Other members of the entry are ignored.

use std::collections::HashMap;

use a2a::Message;
use adam_model::ReasoningEffort;
use serde_json::Value;

use crate::extensions::REASONING_EXTENSION;

/// The member of the entry under [`REASONING_EXTENSION`] that carries the level:
/// `{"effort": "high"}`. It is part of the wire: clients and agents read it by name.
pub const REASONING_EFFORT_MEMBER: &str = "effort";

/// The levels each agent's model takes and, when one is resolved, its default: what the card's
/// `reasoning/v1` entry advertises and what a message's `effort` is checked against.
///
/// Keyed by the **agent registration name** (the `AgentInfo` name, which carries its model alias),
/// never the literal alias: two agents may share one model and differ, and an agent may be renamed
/// without renaming its model. The levels are the model profile's `reasoningEfforts`, kept lowest
/// first whatever order the caller lists them in; the default is the resolved folder, preset or
/// profile value (ADR 0063, decision 3), or none when none of them sets one or the value is not one
/// of the levels.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReasoningEfforts {
    agents: HashMap<String, AgentEfforts>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AgentEfforts {
    levels: Vec<ReasoningEffort>,
    default: Option<ReasoningEffort>,
}

impl ReasoningEfforts {
    /// The empty set: no agent takes a level, so no card lists the extension.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether no agent has an entry: an empty set lists no extension and refuses every request,
    /// so a backend given one reads nothing, as a card that lists no `reasoning/v1`.
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }

    /// The set from `(agent, levels, default)` triples, each agent once (a later triple replaces an
    /// earlier one), every level re-ordered by [`ReasoningEffort::ALL`] and kept once.
    pub fn from_agents<I, A, L>(agents: I) -> Self
    where
        I: IntoIterator<Item = (A, L, Option<ReasoningEffort>)>,
        A: Into<String>,
        L: IntoIterator<Item = ReasoningEffort>,
    {
        agents
            .into_iter()
            .fold(Self::new(), |set, (agent, levels, default)| {
                set.with_agent(agent, levels, default)
            })
    }

    /// `self` with `agent`'s entry: `levels` re-ordered by [`ReasoningEffort::ALL`] (lowest first)
    /// with duplicates dropped, and `default` when it is one of them. A default the agent does not
    /// take is dropped **silently**: a builder has no error channel, and the assembly's chain
    /// refuses an out-of-level default before it reaches here (ADR 0063). The card never advertises
    /// a level the reading would refuse.
    #[must_use]
    pub fn with_agent(
        mut self,
        agent: impl Into<String>,
        levels: impl IntoIterator<Item = ReasoningEffort>,
        default: Option<ReasoningEffort>,
    ) -> Self {
        let named: Vec<ReasoningEffort> = levels.into_iter().collect();
        let levels: Vec<ReasoningEffort> = ReasoningEffort::ALL
            .into_iter()
            .filter(|effort| named.contains(effort))
            .collect();
        let default = default.filter(|effort| levels.contains(effort));
        self.agents
            .insert(agent.into(), AgentEfforts { levels, default });
        self
    }

    /// The set of an assembly's agents, from `(agent, model alias, default)` triples and the
    /// deployment's `MODEL_ALIASES_FILE` profiles ([ADR 0063](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0063-a-reasoning-effort-is-one-setting.md),
    /// decision 5): `agent`'s levels are the `reasoningEfforts` of the profile of the model `alias`
    /// names, lowest first, and `default` is the level the folder, its `model_preset` or the profile
    /// resolved for it ([`Self::with_agent`] drops one the model does not take). An alias with no
    /// profile, or a profile with `reasoningEfforts: []`, takes no level: its card lists no
    /// extension.
    pub fn from_models<'a, I>(agents: I, profiles: &adam_model::ModelProfiles) -> Self
    where
        I: IntoIterator<Item = (&'a str, &'a str, Option<ReasoningEffort>)>,
    {
        Self::from_agents(agents.into_iter().map(|(agent, alias, default)| {
            let levels = profiles
                .profile(alias)
                .and_then(|profile| profile.reasoning_efforts.clone())
                .unwrap_or_default();
            (agent.to_owned(), levels, default)
        }))
    }

    /// The levels `agent`'s model takes, lowest first; empty for an agent with no entry.
    pub fn levels(&self, agent: &str) -> &[ReasoningEffort] {
        self.agents
            .get(agent)
            .map(|entry| entry.levels.as_slice())
            .unwrap_or_default()
    }

    /// The default the caller resolved for `agent` (the folder's, else the preset's, else the
    /// profile's), or `None` when none of them sets one, the agent has no entry, or the value is
    /// not one of [`Self::levels`]: a default the agent does not take is no default at all.
    pub fn default_of(&self, agent: &str) -> Option<ReasoningEffort> {
        self.agents.get(agent).and_then(|entry| entry.default)
    }

    /// Whether `agent`'s model takes `effort`.
    pub fn honours(&self, agent: &str, effort: ReasoningEffort) -> bool {
        self.levels(agent).contains(&effort)
    }

    /// The effort `message` names for `agent`, from
    /// `metadata[REASONING_EXTENSION] = {"effort": "<level>"}`.
    ///
    /// Activation is the caller's to check, as for every extension: only a request that named
    /// [`REASONING_EXTENSION`] (`Caller::has_extension`) may call this, and the runtime wires it
    /// (PR B). A missing entry, an entry that is not an object, a missing or non-string
    /// [`REASONING_EFFORT_MEMBER`], a word outside the seven levels, and a level `agent`'s model
    /// does not take are each a sentence naming the agent and its levels. Other members of the
    /// entry are ignored.
    ///
    /// # Errors
    ///
    /// The refusal sentence, to be the `InvalidParams` message (ADR 0063, decision 5).
    pub fn requested(&self, agent: &str, message: &Message) -> Result<ReasoningEffort, String> {
        let levels = self.levels(agent);
        let entry = message
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get(REASONING_EXTENSION))
            .ok_or_else(|| {
                format!(
                    "agent {agent:?} was asked for a reasoning effort but the message carries no \
                     {REASONING_EXTENSION} entry; it {}",
                    takes(levels)
                )
            })?;
        let object = entry.as_object().ok_or_else(|| {
            format!(
                "agent {agent:?} was sent a {REASONING_EXTENSION} entry that is not an object; it {}",
                takes(levels)
            )
        })?;
        let word = object
            .get(REASONING_EFFORT_MEMBER)
            .and_then(Value::as_str)
            .ok_or_else(|| {
                format!(
                    "agent {agent:?} was sent a {REASONING_EXTENSION} entry with no string \
                     {REASONING_EFFORT_MEMBER:?}; it {}",
                    takes(levels)
                )
            })?;
        let effort = word.parse::<ReasoningEffort>().map_err(|_| {
            format!(
                "agent {agent:?} was asked for reasoning effort {word:?}, which is not one of \
                 {}; it {}",
                all_efforts(),
                takes(levels)
            )
        })?;
        if !levels.contains(&effort) {
            return Err(format!(
                "agent {agent:?} was asked for reasoning effort {:?}, which it does not take; it \
                 {}",
                effort.as_str(),
                takes(levels)
            ));
        }
        Ok(effort)
    }
}

/// `takes a, b` or `takes no reasoning level`, the tail of a refusal sentence.
fn takes(levels: &[ReasoningEffort]) -> String {
    if levels.is_empty() {
        "takes no reasoning level".to_owned()
    } else {
        let words: Vec<&str> = levels.iter().map(|effort| effort.as_str()).collect();
        format!("takes {}", words.join(", "))
    }
}

/// The vocabulary as a refusal names it, built from [`ReasoningEffort::ALL`] so a new variant cannot
/// leave the sentence stale: `none, minimal, low, medium, high, xhigh or max`.
fn all_efforts() -> String {
    let words: Vec<&str> = ReasoningEffort::ALL
        .iter()
        .map(|effort| effort.as_str())
        .collect();
    match words.split_last() {
        Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2a::{Part, Role};
    use serde_json::json;

    /// `coder` takes low and high, defaulting to low; `writer` takes none, minimal and medium,
    /// with no default.
    fn efforts() -> ReasoningEfforts {
        ReasoningEfforts::new()
            .with_agent(
                "coder",
                [ReasoningEffort::High, ReasoningEffort::Low],
                Some(ReasoningEffort::Low),
            )
            .with_agent(
                "writer",
                [
                    ReasoningEffort::None,
                    ReasoningEffort::Minimal,
                    ReasoningEffort::Medium,
                ],
                None,
            )
    }

    fn saying(entry: Option<Value>) -> Message {
        let mut message = Message::new(Role::User, vec![Part::text("think hard")]);
        message.metadata =
            entry.map(|entry| HashMap::from([(REASONING_EXTENSION.to_owned(), entry)]));
        message
    }

    fn refused(agent: &str, message: &Message) -> String {
        efforts().requested(agent, message).expect_err("refused")
    }

    #[test]
    fn the_levels_are_the_models_lowest_first_each_once() {
        let set = ReasoningEfforts::new().with_agent(
            "coder",
            [
                ReasoningEffort::Max,
                ReasoningEffort::Low,
                ReasoningEffort::Max,
                ReasoningEffort::None,
            ],
            Some(ReasoningEffort::Low),
        );
        assert_eq!(
            set.levels("coder"),
            [
                ReasoningEffort::None,
                ReasoningEffort::Low,
                ReasoningEffort::Max
            ]
        );
        assert_eq!(set.default_of("coder"), Some(ReasoningEffort::Low));
        assert!(set.honours("coder", ReasoningEffort::Low));
        assert!(!set.honours("coder", ReasoningEffort::High));
        // An agent with no entry takes nothing.
        assert!(set.levels("nobody").is_empty());
        assert_eq!(set.default_of("nobody"), None);
        assert!(!set.honours("nobody", ReasoningEffort::Low));
    }

    #[test]
    fn a_default_the_agent_does_not_take_is_no_default() {
        let set = ReasoningEfforts::new().with_agent(
            "coder",
            [ReasoningEffort::Low],
            Some(ReasoningEffort::Max),
        );
        assert_eq!(set.default_of("coder"), None);
        // The card never advertises a default the reading would refuse.
        let e = crate::ExtensionConfig::reasoning(&set, "coder").expect("listed");
        assert_eq!(
            serde_json::Value::Object(e.params),
            json!({"efforts": ["low"]})
        );
    }

    #[test]
    fn the_set_of_an_assembly_is_each_models_own_levels() {
        let profiles = adam_model::ModelProfiles::parse_json(
            r#"{"profiles": {"wide": {"reasoningEfforts": ["max", "low"], "reasoningWire": "reasoning_effort"}, "bare": {}, "off": {"reasoningEfforts": []}}}"#,
        )
        .expect("valid profiles");
        let set = ReasoningEfforts::from_models(
            [
                ("coder", "wide", Some(ReasoningEffort::Low)),
                ("chat", "bare", None),
                ("tutor", "off", None),
                ("nobody", "unknown", None),
            ],
            &profiles,
        );
        assert_eq!(
            set.levels("coder"),
            [ReasoningEffort::Low, ReasoningEffort::Max],
            "the model's levels, lowest first"
        );
        assert_eq!(set.default_of("coder"), Some(ReasoningEffort::Low));
        for agent in ["chat", "tutor", "nobody"] {
            assert!(set.levels(agent).is_empty(), "{agent}");
            assert_eq!(set.default_of(agent), None, "{agent}");
        }
        assert!(!set.is_empty());
        assert!(ReasoningEfforts::new().is_empty());
    }

    #[test]
    fn the_set_is_built_from_agent_triples_each_agent_once() {
        let set = ReasoningEfforts::from_agents([
            (
                "coder",
                vec![ReasoningEffort::High, ReasoningEffort::Low],
                Some(ReasoningEffort::High),
            ),
            (
                "writer",
                vec![ReasoningEffort::None, ReasoningEffort::Minimal],
                None,
            ),
            ("coder", vec![ReasoningEffort::Medium], None),
        ]);
        assert_eq!(set.levels("coder"), [ReasoningEffort::Medium]);
        assert_eq!(
            set.default_of("coder"),
            None,
            "the later triple replaces it"
        );
        assert_eq!(
            set.levels("writer"),
            [ReasoningEffort::None, ReasoningEffort::Minimal]
        );
    }

    #[test]
    fn every_level_the_agent_takes_is_accepted() {
        for effort in [ReasoningEffort::Low, ReasoningEffort::High] {
            let message = saying(Some(json!({ REASONING_EFFORT_MEMBER: effort.as_str() })));
            assert_eq!(efforts().requested("coder", &message), Ok(effort));
        }
        for effort in [
            ReasoningEffort::None,
            ReasoningEffort::Minimal,
            ReasoningEffort::Medium,
        ] {
            let message = saying(Some(json!({ REASONING_EFFORT_MEMBER: effort.as_str() })));
            assert_eq!(efforts().requested("writer", &message), Ok(effort));
        }
    }

    #[test]
    fn the_key_is_the_agent_and_two_agents_may_take_different_levels() {
        let message = saying(Some(json!({ REASONING_EFFORT_MEMBER: "none" })));
        assert_eq!(
            efforts().requested("writer", &message),
            Ok(ReasoningEffort::None)
        );
        assert!(
            efforts().requested("coder", &message).is_err(),
            "the same entry is refused for the agent whose model does not take it"
        );
    }

    #[test]
    fn a_missing_entry_is_refused_naming_the_agent_and_its_levels() {
        let error = refused("coder", &saying(None));
        assert!(error.contains("coder"), "{error}");
        assert!(error.contains("low, high"), "{error}");
        // An agent with no entry takes no level, and the sentence says so.
        let error = refused("nobody", &saying(None));
        assert!(error.contains("nobody"), "{error}");
        assert!(error.contains("no reasoning level"), "{error}");
    }

    #[test]
    fn an_entry_that_is_not_an_object_is_refused() {
        for entry in [json!("high"), json!(["high"]), json!(3), json!(null)] {
            let error = refused("coder", &saying(Some(entry.clone())));
            assert!(error.contains("coder"), "{entry}: {error}");
            assert!(error.contains("low, high"), "{entry}: {error}");
            assert!(error.contains("not an object"), "{entry}: {error}");
        }
    }

    #[test]
    fn a_missing_or_non_string_effort_is_refused() {
        for entry in [
            json!({}),
            json!({ "effort": 3 }),
            json!({ "effort": null }),
            json!({ "effort": ["high"] }),
            json!({ "level": "high" }),
        ] {
            let error = refused("coder", &saying(Some(entry.clone())));
            assert!(error.contains("coder"), "{entry}: {error}");
            assert!(error.contains("low, high"), "{entry}: {error}");
            assert!(error.contains("no string"), "{entry}: {error}");
        }
    }

    #[test]
    fn a_word_outside_the_seven_is_refused() {
        for word in ["High", "HIGH", "ultra", "", " high", "high ", "3"] {
            let error = refused(
                "coder",
                &saying(Some(json!({ REASONING_EFFORT_MEMBER: word }))),
            );
            assert!(error.contains("coder"), "{word:?}: {error}");
            assert!(error.contains("low, high"), "{word:?}: {error}");
            assert!(
                error.contains("none, minimal, low, medium, high, xhigh or max"),
                "{word:?}: {error}"
            );
        }
    }

    #[test]
    fn a_level_the_agent_does_not_take_is_refused() {
        for word in ["none", "minimal", "medium", "max", "xhigh"] {
            let error = refused(
                "coder",
                &saying(Some(json!({ REASONING_EFFORT_MEMBER: word }))),
            );
            assert!(error.contains("coder"), "{word:?}: {error}");
            assert!(error.contains("low, high"), "{word:?}: {error}");
            assert!(error.contains("does not take"), "{word:?}: {error}");
        }
    }

    #[test]
    fn other_members_of_the_entry_are_ignored() {
        let message = saying(Some(json!({
            REASONING_EFFORT_MEMBER: "high",
            "why": "a hard problem",
            "note": { "source": "the person" },
        })));
        assert_eq!(
            efforts().requested("coder", &message),
            Ok(ReasoningEffort::High)
        );
    }

    /// The reading takes no `Caller`: activation is the caller's gate
    /// (`Caller::has_extension`), wired by the runtime in PR B. This pins that `requested` is the
    /// reading only, not a second activation gate; a caller that did not check is a wiring bug.
    #[test]
    fn the_reading_does_not_check_activation_the_caller_does() {
        let named = saying(Some(json!({ REASONING_EFFORT_MEMBER: "high" })));
        assert_eq!(
            efforts().requested("coder", &named),
            Ok(ReasoningEffort::High)
        );
    }
}
