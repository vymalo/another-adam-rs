//! `agent-mode/v1`: the mode a message picks for its run (ADR 0039, decision 3).
//!
//! A folder declares primary modes (`modes/<name>.md`), one of them the default, and a person
//! picks one **per message**, as they pick the access tier. The choice travels in the message's
//! metadata under [`AGENT_MODE_EXTENSION`], as `{"mode": "<name>"}`; the card lists the modes the
//! agent declares, `params.modes`, and which one a message that names none works in,
//! `params.default`.
//!
//! Three rules keep the reading honest:
//!
//! * **Only an activated extension is read.** A message whose request did not name the extension
//!   carries no mode, whatever its metadata says. Activation is the caller's check
//!   ([`Caller::has_extension`](crate::Caller::has_extension)), as for every extension.
//! * **Only a declared mode is accepted, fail closed.** A mode outside [`AgentModes::modes`] is
//!   refused with [`MODE_NOT_OFFERED`] and the modes, before anything is written.
//! * **A mode only narrows.** It withholds the tools it does not list and adds its words to the
//!   agent's; the access tier still caps every tool it allows, and nothing a mode says can raise
//!   the tier, a limit or a credential.
//!
//! The contract page is the orchestration layer's (`docs/api/agent-mode-v1.md` of
//! `vymalo/another-agentic-system`, ANR-979); ADR 0039 decision 3 is this repository's side.

use std::collections::HashSet;

use a2a::Message;
use serde_json::{Value, json};

use crate::extensions::{AGENT_MODE_EXTENSION, ModeInfo};

/// The member of the entry under [`AGENT_MODE_EXTENSION`] that carries the mode:
/// `{"mode": "plan"}`. It is part of the wire: clients and agents read it by name.
pub const MODE_MEMBER: &str = "mode";

/// The code a refusal carries, first in the sentence, so a client can tell a mode the agent does
/// not declare from any other `InvalidParams`: `mode_not_offered`.
pub const MODE_NOT_OFFERED: &str = "mode_not_offered";

/// The modes a served agent declares, and the one a message that names none works in: what the
/// card's `agent-mode/v1` entry advertises (`params.modes` and `params.default`) and what a
/// message's `mode` is checked against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentModes {
    modes: Vec<ModeInfo>,
    default: String,
}

/// Why a set of modes cannot be declared.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AgentModesError {
    /// No mode is declared.
    #[error("an agent that declares modes declares at least one")]
    Empty,
    /// The default is not one of the modes declared.
    #[error("the default mode {0:?} is not one of the modes declared")]
    DefaultNotDeclared(String),
}

impl AgentModes {
    /// `modes` (each name once, in the order given) with `default` among them.
    ///
    /// # Errors
    ///
    /// [`AgentModesError::Empty`] for no mode, [`AgentModesError::DefaultNotDeclared`] when
    /// `default` is not one of them.
    pub fn new(
        modes: impl IntoIterator<Item = ModeInfo>,
        default: impl Into<String>,
    ) -> Result<Self, AgentModesError> {
        let mut seen = HashSet::new();
        let modes: Vec<ModeInfo> = modes
            .into_iter()
            .filter(|mode| !mode.name.trim().is_empty() && seen.insert(mode.name.clone()))
            .collect();
        if modes.is_empty() {
            return Err(AgentModesError::Empty);
        }
        let default = default.into();
        if !modes.iter().any(|mode| mode.name == default) {
            return Err(AgentModesError::DefaultNotDeclared(default));
        }
        Ok(Self { modes, default })
    }

    /// The modes declared, in the order given.
    pub fn modes(&self) -> &[ModeInfo] {
        &self.modes
    }

    /// What a run works in when its message names none.
    pub fn default_mode(&self) -> &str {
        &self.default
    }

    /// Whether `name` is one of the modes declared.
    pub fn offers(&self, name: &str) -> bool {
        self.modes.iter().any(|mode| mode.name == name)
    }

    /// The modes as the card's `params.modes` lists them: each `{"name", "title",
    /// "description"}`.
    pub fn entries(&self) -> Vec<Value> {
        self.modes
            .iter()
            .map(|mode| {
                json!({
                    "name": mode.name,
                    "title": mode.title,
                    "description": mode.description,
                })
            })
            .collect()
    }

    /// The mode `message` picks for its run, from
    /// `metadata[AGENT_MODE_EXTENSION] = {"mode": "<name>"}`.
    ///
    /// Activation is the caller's to check, as for every extension: only a request that named
    /// [`AGENT_MODE_EXTENSION`] (`Caller::has_extension`) may call this, and the runtime wires it. A
    /// missing entry, an entry that is not an object, a missing, blank or non-string
    /// [`MODE_MEMBER`], and a mode [`Self::offers`] does not list are each a refusal that begins
    /// with [`MODE_NOT_OFFERED`] and names the modes. Other members of the entry are ignored.
    ///
    /// # Errors
    ///
    /// The refusal sentence, to be the `InvalidParams` message.
    pub fn requested(&self, message: &Message) -> Result<String, String> {
        let entry = message
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get(AGENT_MODE_EXTENSION))
            .ok_or_else(|| {
                self.refusal(format!(
                    "the message activates {AGENT_MODE_EXTENSION} and names no mode: put \
                     {{\"{MODE_MEMBER}\": ..}} in its metadata under that URI"
                ))
            })?;
        let object = entry.as_object().ok_or_else(|| {
            self.refusal(format!(
                "the message's {AGENT_MODE_EXTENSION} entry is not an object"
            ))
        })?;
        let name = object
            .get(MODE_MEMBER)
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| {
                self.refusal(format!(
                    "the message's {AGENT_MODE_EXTENSION} entry has no string {MODE_MEMBER:?}"
                ))
            })?;
        if !self.offers(name) {
            return Err(self.refusal(format!(
                "the message names the mode {name:?}, which this agent does not declare"
            )));
        }
        Ok(name.to_owned())
    }

    /// `MODE_NOT_OFFERED: <why>; it declares <the modes>`, the sentence of every refusal.
    fn refusal(&self, why: String) -> String {
        let offered = self
            .modes
            .iter()
            .map(|mode| mode.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        format!("{MODE_NOT_OFFERED}: {why}; it declares {offered}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2a::{Part, Role};
    use std::collections::HashMap;

    fn info(name: &str) -> ModeInfo {
        ModeInfo {
            name: name.into(),
            title: format!("{name} mode"),
            description: format!("does {name}"),
        }
    }

    fn modes() -> AgentModes {
        AgentModes::new([info("build"), info("plan")], "build").expect("declared")
    }

    fn saying(entry: Option<Value>) -> Message {
        let mut message = Message::new(Role::User, vec![Part::text("go")]);
        message.metadata =
            entry.map(|entry| HashMap::from([(AGENT_MODE_EXTENSION.to_owned(), entry)]));
        message
    }

    fn refused(message: &Message) -> String {
        modes().requested(message).expect_err("refused")
    }

    #[test]
    fn the_modes_are_each_once_in_order_and_the_default_is_one_of_them() {
        let set = AgentModes::new(
            [info("plan"), info(" "), info("build"), info("plan")],
            "build",
        )
        .expect("declared");
        assert_eq!(
            set.modes()
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>(),
            ["plan", "build"]
        );
        assert_eq!(set.default_mode(), "build");
        assert!(set.offers("plan") && !set.offers("Build"));
        assert_eq!(AgentModes::new([], "build"), Err(AgentModesError::Empty));
        assert_eq!(
            AgentModes::new([info("plan")], "build"),
            Err(AgentModesError::DefaultNotDeclared("build".into()))
        );
    }

    #[test]
    fn the_entries_are_the_cards_shape() {
        assert_eq!(
            modes().entries(),
            [
                json!({"name": "build", "title": "build mode", "description": "does build"}),
                json!({"name": "plan", "title": "plan mode", "description": "does plan"}),
            ]
        );
    }

    #[test]
    fn a_declared_mode_is_the_pick() {
        for name in ["build", "plan"] {
            assert_eq!(
                modes().requested(&saying(Some(json!({ MODE_MEMBER: name })))),
                Ok(name.to_owned())
            );
        }
        // Other members of the entry are ignored.
        assert_eq!(
            modes().requested(&saying(Some(
                json!({ MODE_MEMBER: "plan", "why": "the person's" })
            ))),
            Ok("plan".to_owned())
        );
    }

    #[test]
    fn a_missing_entry_is_refused_naming_the_modes() {
        let error = refused(&saying(None));
        assert!(error.starts_with(MODE_NOT_OFFERED), "{error}");
        assert!(error.contains(AGENT_MODE_EXTENSION), "{error}");
        assert!(error.contains("build, plan"), "{error}");
    }

    #[test]
    fn an_entry_that_is_not_an_object_is_refused() {
        for entry in [json!("plan"), json!(["plan"]), json!(3), json!(null)] {
            let error = refused(&saying(Some(entry.clone())));
            assert!(error.starts_with(MODE_NOT_OFFERED), "{entry}: {error}");
            assert!(error.contains("not an object"), "{entry}: {error}");
        }
    }

    #[test]
    fn a_missing_blank_or_non_string_mode_is_refused() {
        for entry in [
            json!({}),
            json!({ MODE_MEMBER: "" }),
            json!({ MODE_MEMBER: "  " }),
            json!({ MODE_MEMBER: 3 }),
            json!({ MODE_MEMBER: null }),
            json!({ "name": "plan" }),
        ] {
            let error = refused(&saying(Some(entry.clone())));
            assert!(error.starts_with(MODE_NOT_OFFERED), "{entry}: {error}");
            assert!(error.contains("no string"), "{entry}: {error}");
        }
    }

    #[test]
    fn a_mode_outside_the_declared_ones_is_refused_naming_them() {
        for name in ["review", "Plan", " plan", "plan "] {
            let error = refused(&saying(Some(json!({ MODE_MEMBER: name }))));
            assert!(error.starts_with(MODE_NOT_OFFERED), "{name:?}: {error}");
            assert!(error.contains("does not declare"), "{name:?}: {error}");
            assert!(error.contains("build, plan"), "{name:?}: {error}");
        }
    }
}
