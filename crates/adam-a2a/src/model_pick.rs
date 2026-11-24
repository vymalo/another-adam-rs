//! `model-pick/v1`: the model a message names for its run.
//!
//! A client that lets the person pick the model names the **alias** for each message, in the
//! message's metadata under [`MODEL_PICK_EXTENSION`], as `{"model": "<alias>"}`. The card lists the
//! aliases the deployment offers, `params.choices`, and a message may name only one of them
//! (ADR 0064, amended 2026-11-23).
//!
//! Three rules keep the reading honest:
//!
//! * **Only an activated extension is read.** A message whose request did not name the extension
//!   carries no pick, whatever its metadata says. Activation is the caller's check
//!   ([`Caller::has_extension`](crate::Caller::has_extension)), as for every extension; the runtime
//!   wires it.
//! * **Only an offered alias is accepted, fail closed.** A pick outside [`ModelChoices::aliases`]
//!   is refused with [`MODEL_NOT_OFFERED`] and the choices, before anything is written.
//! * **The gateway still decides.** A pick is a request, never an authorisation: the model call
//!   goes through the deployment's gateway as any other, and a locked model is refused there
//!   (`model_locked`, ADR 0067).
//!
//! Each choice also carries the deployment's **endpoint label** for its alias, when
//! `MODEL_ALIASES_FILE` names one ([`ModelProfiles::endpoint_of`](adam_model::ModelProfiles::endpoint_of),
//! ANR-623): a label only, never a provider, a host, a URL or a key. A deployment that names none
//! lists no `endpoint`, and a malformed label is omitted rather than guessed.

use std::collections::{BTreeMap, HashSet};

use a2a::Message;
use serde_json::{Value, json};

use crate::extensions::MODEL_PICK_EXTENSION;
use adam_model::ModelProfiles;

/// The member of the entry under [`MODEL_PICK_EXTENSION`] that carries the alias:
/// `{"model": "alias-a"}`. It is part of the wire: clients and agents read it by name.
pub const MODEL_PICK_MEMBER: &str = "model";

/// The code a refusal carries, first in the sentence, so a client can tell a pick outside the
/// card's choices from any other `InvalidParams`: `model_not_offered`.
pub const MODEL_NOT_OFFERED: &str = "model_not_offered";

/// The model aliases a served agent offers a message to pick: what the card's `model-pick/v1`
/// entry advertises (`params.choices`) and what a message's `model` is checked against.
///
/// The deployment's list is the models of `MODEL_ALIASES_FILE` that this agent can be bound to, its
/// own model left out; an agent the deployment gives one alias offers none, so its card lists no
/// extension. Each choice may also carry the deployment's endpoint label for that alias
/// ([`Self::endpoint_of`], ANR-623).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelChoices {
    aliases: Vec<String>,
    /// The endpoint label of each offered alias that names one, and only those: the label is a
    /// property of the choice, so it never outlives the alias.
    endpoints: BTreeMap<String, String>,
}

impl ModelChoices {
    /// The empty set: no model is offered, so no card lists the extension.
    pub fn new() -> Self {
        Self::default()
    }

    /// The choices `aliases`, in the order given, each once, blank ones left out.
    pub fn from_aliases<I, S>(aliases: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut seen = HashSet::new();
        Self {
            aliases: aliases
                .into_iter()
                .map(Into::into)
                .filter(|alias| !alias.trim().is_empty())
                .filter(|alias| seen.insert(alias.clone()))
                .collect(),
            endpoints: BTreeMap::new(),
        }
    }

    /// The choices `aliases` with the endpoint label each one's profile names in `profiles`
    /// (ANR-623): the same list [`Self::from_aliases`] builds, each offered alias carrying its
    /// profile's `endpoint` when the deployment names a well-formed one.
    ///
    /// A profile that names no endpoint, and a malformed one, leave the choice without a label.
    pub fn from_profiles<I, S>(aliases: I, profiles: &ModelProfiles) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut choices = Self::from_aliases(aliases);
        choices.endpoints = choices
            .aliases
            .iter()
            .filter_map(|alias| {
                profiles
                    .endpoint_of(alias)
                    .map(|endpoint| (alias.clone(), endpoint.to_owned()))
            })
            .collect();
        choices
    }

    /// The aliases offered, in the order the deployment lists them.
    pub fn aliases(&self) -> &[String] {
        &self.aliases
    }

    /// The endpoint label the deployment names for `alias`, when it names one: its own name for the
    /// model's endpoint, never a provider, a host, a URL or a key.
    pub fn endpoint_of(&self, alias: &str) -> Option<&str> {
        self.endpoints.get(alias).map(String::as_str)
    }

    /// The choices as the card's `params.choices` lists them: each `{"id": "<alias>"}`, and
    /// `"endpoint": "<label>"` beside the `id` when the deployment names one. `label` is left out:
    /// the alias is the name a person reads, and a client falls back to the `id`.
    pub fn entries(&self) -> Vec<Value> {
        self.aliases
            .iter()
            .map(|alias| match self.endpoint_of(alias) {
                Some(endpoint) => json!({ "id": alias, "endpoint": endpoint }),
                None => json!({ "id": alias }),
            })
            .collect()
    }

    /// Whether the agent offers no model at all.
    pub fn is_empty(&self) -> bool {
        self.aliases.is_empty()
    }

    /// Whether `alias` is one of the offered models.
    pub fn offers(&self, alias: &str) -> bool {
        self.aliases.iter().any(|offered| offered == alias)
    }

    /// The model `message` names for its run, from
    /// `metadata[MODEL_PICK_EXTENSION] = {"model": "<alias>"}`.
    ///
    /// Activation is the caller's to check, as for every extension: only a request that named
    /// [`MODEL_PICK_EXTENSION`] (`Caller::has_extension`) may call this, and the runtime wires it. A
    /// missing entry, an entry that is not an object, a missing, blank or non-string
    /// [`MODEL_PICK_MEMBER`], and an alias [`Self::offers`] does not list are each a refusal that
    /// begins with [`MODEL_NOT_OFFERED`] and names the choices. Other members of the entry are
    /// ignored.
    ///
    /// # Errors
    ///
    /// The refusal sentence, to be the `InvalidParams` message.
    pub fn requested(&self, message: &Message) -> Result<String, String> {
        let entry = message
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get(MODEL_PICK_EXTENSION))
            .ok_or_else(|| {
                self.refusal(format!(
                    "the message activates {MODEL_PICK_EXTENSION} and names no model: put \
                     {{\"{MODEL_PICK_MEMBER}\": ..}} in its metadata under that URI"
                ))
            })?;
        let object = entry.as_object().ok_or_else(|| {
            self.refusal(format!(
                "the message's {MODEL_PICK_EXTENSION} entry is not an object"
            ))
        })?;
        let alias = object
            .get(MODEL_PICK_MEMBER)
            .and_then(Value::as_str)
            .filter(|alias| !alias.trim().is_empty())
            .ok_or_else(|| {
                self.refusal(format!(
                    "the message's {MODEL_PICK_EXTENSION} entry has no string \
                     {MODEL_PICK_MEMBER:?}"
                ))
            })?;
        if !self.offers(alias) {
            return Err(self.refusal(format!(
                "the message names the model {alias:?}, which this agent does not offer"
            )));
        }
        Ok(alias.to_owned())
    }

    /// `MODEL_NOT_OFFERED: <why>; it offers <the choices>`, the sentence of every refusal.
    fn refusal(&self, why: String) -> String {
        let offers = if self.aliases.is_empty() {
            "no model".to_owned()
        } else {
            self.aliases.join(", ")
        };
        format!("{MODEL_NOT_OFFERED}: {why}; it offers {offers}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2a::{Part, Role};
    use std::collections::HashMap;

    fn choices() -> ModelChoices {
        ModelChoices::from_aliases(["alias-a", "alias-b"])
    }

    fn saying(entry: Option<Value>) -> Message {
        let mut message = Message::new(Role::User, vec![Part::text("go")]);
        message.metadata =
            entry.map(|entry| HashMap::from([(MODEL_PICK_EXTENSION.to_owned(), entry)]));
        message
    }

    fn refused(message: &Message) -> String {
        choices().requested(message).expect_err("refused")
    }

    #[test]
    fn the_choices_are_the_deployments_each_once_in_order() {
        let set = ModelChoices::from_aliases(["alias-b", " ", "alias-a", "alias-b"]);
        assert_eq!(set.aliases(), ["alias-b", "alias-a"]);
        assert!(!set.is_empty());
        assert!(set.offers("alias-a"));
        assert!(!set.offers("alias-c"));
        assert!(ModelChoices::new().is_empty());
        assert!(!ModelChoices::new().offers("alias-a"));
        // No endpoint label is invented for a list built from aliases alone.
        assert_eq!(
            set.entries(),
            [
                serde_json::json!({"id": "alias-b"}),
                serde_json::json!({"id": "alias-a"})
            ]
        );
        assert_eq!(set.endpoint_of("alias-a"), None);
    }

    #[test]
    fn a_choice_carries_the_endpoint_label_its_profile_names() {
        use adam_model::ModelProfiles;

        let profiles = ModelProfiles::parse_json(
            r#"{"profiles": {
                "alias-a": {"endpoint": "gateway-a"},
                "alias-b": {"endpoint": "   "}
            }}"#,
        )
        .expect("valid profiles");
        let set = ModelChoices::from_profiles(["alias-a", "alias-b", "unprofiled"], &profiles);
        assert_eq!(set.aliases(), ["alias-a", "alias-b", "unprofiled"]);
        assert_eq!(set.endpoint_of("alias-a"), Some("gateway-a"));
        // A malformed label and a model with no profile are both absent, never guessed.
        assert_eq!(set.endpoint_of("alias-b"), None);
        assert_eq!(set.endpoint_of("unprofiled"), None);
        assert_eq!(
            set.entries(),
            [
                serde_json::json!({"id": "alias-a", "endpoint": "gateway-a"}),
                serde_json::json!({"id": "alias-b"}),
                serde_json::json!({"id": "unprofiled"}),
            ]
        );
        // A choice the deployment does not offer never carries a label.
        assert_eq!(set.endpoint_of("not-offered"), None);
    }

    #[test]
    fn an_offered_alias_is_the_pick() {
        for alias in ["alias-a", "alias-b"] {
            assert_eq!(
                choices().requested(&saying(Some(
                    serde_json::json!({ MODEL_PICK_MEMBER: alias })
                ))),
                Ok(alias.to_owned())
            );
        }
    }

    #[test]
    fn other_members_of_the_entry_are_ignored() {
        assert_eq!(
            choices().requested(&saying(Some(serde_json::json!({
                MODEL_PICK_MEMBER: "alias-a",
                "why": "the person's",
            })))),
            Ok("alias-a".to_owned())
        );
    }

    #[test]
    fn a_missing_entry_is_refused_naming_the_choices() {
        let error = refused(&saying(None));
        assert!(error.starts_with(MODEL_NOT_OFFERED), "{error}");
        assert!(error.contains(MODEL_PICK_EXTENSION), "{error}");
        assert!(error.contains("alias-a, alias-b"), "{error}");
        // No choice is said as none, never as an empty list.
        let error = ModelChoices::new()
            .requested(&saying(None))
            .expect_err("refused");
        assert!(error.contains("offers no model"), "{error}");
    }

    #[test]
    fn an_entry_that_is_not_an_object_is_refused() {
        for entry in [
            serde_json::json!("alias-a"),
            serde_json::json!(["alias-a"]),
            serde_json::json!(3),
            serde_json::json!(null),
        ] {
            let error = refused(&saying(Some(entry.clone())));
            assert!(error.starts_with(MODEL_NOT_OFFERED), "{entry}: {error}");
            assert!(error.contains("not an object"), "{entry}: {error}");
            assert!(error.contains("alias-a, alias-b"), "{entry}: {error}");
        }
    }

    #[test]
    fn a_missing_blank_or_non_string_model_is_refused() {
        for entry in [
            serde_json::json!({}),
            serde_json::json!({ "model": "" }),
            serde_json::json!({ "model": "  " }),
            serde_json::json!({ "model": 3 }),
            serde_json::json!({ "model": null }),
            serde_json::json!({ "model": ["alias-a"] }),
            serde_json::json!({ "alias": "alias-a" }),
        ] {
            let error = refused(&saying(Some(entry.clone())));
            assert!(error.starts_with(MODEL_NOT_OFFERED), "{entry}: {error}");
            assert!(error.contains("no string"), "{entry}: {error}");
            assert!(error.contains("alias-a, alias-b"), "{entry}: {error}");
        }
    }

    #[test]
    fn an_alias_outside_the_choices_is_refused_naming_them() {
        for alias in ["alias-c", "Alias-A", " alias-a", "alias-a "] {
            let error = refused(&saying(Some(
                serde_json::json!({ MODEL_PICK_MEMBER: alias }),
            )));
            assert!(error.starts_with(MODEL_NOT_OFFERED), "{alias:?}: {error}");
            assert!(error.contains("does not offer"), "{alias:?}: {error}");
            assert!(error.contains("alias-a, alias-b"), "{alias:?}: {error}");
        }
    }
}
