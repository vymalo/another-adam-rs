//! The access tiers of `access-tier/v1`: four words a person picks for one message, and what an
//! agent says it honours.
//!
//! It lives here, beside [`ReasoningEffort`](crate::ReasoningEffort), so the protocol
//! (`adam-a2a`, which re-exports it), the session policy (`adam-runtime`'s `SessionConfig` and
//! `SessionBounds`) and the loop share **one** closed list rather than a copy each. A session's
//! `access_tier` (ADR 0054, slice 8) is the option that made the runtime need the type.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// How much an agent may do alone for one message, from the least to the most. The order is the
/// contract's (`plan` < `edits` < `auto` < `bypass`): "higher" means later.
///
/// A closed list on purpose: a fifth tier is a decision for every agent and every client, so it must
/// fail to compile where a tier is matched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AccessTier {
    /// Read and propose. No change to a repository or to any external system.
    Plan,
    /// Change files, commit, push branches, open pull requests. No merge, creation, rename or delete.
    Edits,
    /// Everything the agent does as the normal course of its work. What cannot be undone is outside.
    Auto,
    /// Never asks for permission: anything the agent can do with the credentials it has.
    Bypass,
}

impl AccessTier {
    /// The four tiers, lowest first.
    pub const ALL: [Self; 4] = [Self::Plan, Self::Edits, Self::Auto, Self::Bypass];

    /// The word on the wire: `plan`, `edits`, `auto` or `bypass`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Edits => "edits",
            Self::Auto => "auto",
            Self::Bypass => "bypass",
        }
    }
}

impl fmt::Display for AccessTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A word that is none of the four tiers.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not an access tier: use plan, edits, auto or bypass")]
pub struct NotATier(pub String);

impl FromStr for AccessTier {
    type Err = NotATier;

    /// Exactly the four words, in lower case: the contract never rounds a tier up, down or to a
    /// near spelling.
    fn from_str(word: &str) -> Result<Self, NotATier> {
        Self::ALL
            .into_iter()
            .find(|tier| tier.as_str() == word)
            .ok_or_else(|| NotATier(word.to_owned()))
    }
}

/// The tiers an agent honours and the one it works under when a message names none: the `params` of
/// its card entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessTiers {
    honoured: Vec<AccessTier>,
    default: AccessTier,
}

/// Why a set of tiers cannot be declared.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AccessTiersError {
    /// No tier is honoured.
    #[error("an agent that declares access tiers honours at least one")]
    Empty,
    /// The default is not one of the tiers honoured.
    #[error("the default access tier {0} is not one of the tiers honoured")]
    DefaultNotHonoured(AccessTier),
}

impl AccessTiers {
    /// `honoured` (each once, in the order of the tiers) with `default`.
    ///
    /// # Errors
    ///
    /// [`AccessTiersError::Empty`] for no tier, [`AccessTiersError::DefaultNotHonoured`] when
    /// `default` is not among them.
    pub fn new(
        honoured: impl IntoIterator<Item = AccessTier>,
        default: AccessTier,
    ) -> Result<Self, AccessTiersError> {
        let named: Vec<AccessTier> = honoured.into_iter().collect();
        let honoured: Vec<AccessTier> = AccessTier::ALL
            .into_iter()
            .filter(|tier| named.contains(tier))
            .collect();
        if honoured.is_empty() {
            return Err(AccessTiersError::Empty);
        }
        if !honoured.contains(&default) {
            return Err(AccessTiersError::DefaultNotHonoured(default));
        }
        Ok(Self { honoured, default })
    }

    /// All four tiers, working under `default` when a message names none.
    pub fn all(default: AccessTier) -> Self {
        Self {
            honoured: AccessTier::ALL.to_vec(),
            default,
        }
    }

    /// Every tier up to and including `max`, working under `default` when a message names none, or
    /// under `max` when that is lower: what a deployment that caps the tiers declares. Never empty
    /// (`plan` is always there).
    pub fn up_to(max: AccessTier, default: AccessTier) -> Self {
        Self {
            honoured: AccessTier::ALL
                .into_iter()
                .filter(|tier| *tier <= max)
                .collect(),
            default: default.min(max),
        }
    }

    /// The tiers honoured, lowest first.
    pub fn honoured(&self) -> &[AccessTier] {
        &self.honoured
    }

    /// What the agent works under when a message names none.
    pub fn default_tier(&self) -> AccessTier {
        self.default
    }

    /// Whether `tier` is one the agent honours.
    pub fn honours(&self, tier: AccessTier) -> bool {
        self.honoured.contains(&tier)
    }

    /// The highest tier honoured: the ceiling a session may be configured with. `None` only for a
    /// set that honours nothing, which [`new`](Self::new) refuses, so every declared set has one.
    pub fn ceiling(&self) -> Option<AccessTier> {
        self.honoured.last().copied()
    }

    /// These tiers with every one above `ceiling` dropped, and a default lowered to it: what a
    /// starter whose effective tier is `ceiling` may hand a session (ADR 0054, slice 8).
    ///
    /// `None` when the narrowed set would honour nothing, which fails closed: a deployment that
    /// cannot express the ceiling refuses the option rather than admitting it.
    #[must_use]
    pub fn narrowed_to(&self, ceiling: AccessTier) -> Option<Self> {
        let honoured: Vec<AccessTier> = self
            .honoured
            .iter()
            .copied()
            .filter(|tier| *tier <= ceiling)
            .collect();
        let default = self.default.min(ceiling);
        Self::new(honoured, default).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tiers_are_four_words_in_the_contracts_order() {
        assert!(AccessTier::Plan < AccessTier::Edits);
        assert!(AccessTier::Edits < AccessTier::Auto);
        assert!(AccessTier::Auto < AccessTier::Bypass);
        let words: Vec<&str> = AccessTier::ALL.iter().map(|t| t.as_str()).collect();
        assert_eq!(words, ["plan", "edits", "auto", "bypass"]);
        for tier in AccessTier::ALL {
            assert_eq!(tier.as_str().parse::<AccessTier>(), Ok(tier));
            assert_eq!(tier.to_string(), tier.as_str());
            assert_eq!(
                serde_json::to_value(tier).unwrap(),
                serde_json::json!(tier.as_str())
            );
        }
    }

    #[test]
    fn a_near_spelling_is_not_a_tier() {
        for bad in [
            "",
            "Plan",
            "AUTO",
            " auto",
            "auto ",
            "edit",
            "bypass-all",
            "root",
        ] {
            assert_eq!(
                bad.parse::<AccessTier>(),
                Err(NotATier(bad.to_owned())),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn declared_tiers_are_each_once_in_order_and_the_default_is_one_of_them() {
        let tiers = AccessTiers::new(
            [AccessTier::Auto, AccessTier::Plan, AccessTier::Auto],
            AccessTier::Plan,
        )
        .unwrap();
        assert_eq!(tiers.honoured(), [AccessTier::Plan, AccessTier::Auto]);
        assert_eq!(tiers.default_tier(), AccessTier::Plan);
        assert!(tiers.honours(AccessTier::Auto) && !tiers.honours(AccessTier::Bypass));
        assert_eq!(
            AccessTiers::new([], AccessTier::Auto),
            Err(AccessTiersError::Empty)
        );
        assert_eq!(
            AccessTiers::new([AccessTier::Plan], AccessTier::Auto),
            Err(AccessTiersError::DefaultNotHonoured(AccessTier::Auto))
        );
        assert_eq!(AccessTiers::all(AccessTier::Auto).honoured().len(), 4);
    }

    #[test]
    fn a_cap_declares_the_tiers_up_to_it_and_never_a_default_above_it() {
        let auto = AccessTiers::up_to(AccessTier::Auto, AccessTier::Auto);
        assert_eq!(
            auto.honoured(),
            [AccessTier::Plan, AccessTier::Edits, AccessTier::Auto]
        );
        assert_eq!(auto.default_tier(), AccessTier::Auto);
        let edits = AccessTiers::up_to(AccessTier::Edits, AccessTier::Auto);
        assert_eq!(edits.honoured(), [AccessTier::Plan, AccessTier::Edits]);
        assert_eq!(edits.default_tier(), AccessTier::Edits);
        let plan = AccessTiers::up_to(AccessTier::Plan, AccessTier::Bypass);
        assert_eq!(plan.honoured(), [AccessTier::Plan]);
        assert_eq!(plan.default_tier(), AccessTier::Plan);
        assert_eq!(
            AccessTiers::up_to(AccessTier::Bypass, AccessTier::Auto),
            AccessTiers::all(AccessTier::Auto)
        );
    }

    #[test]
    fn a_narrowed_set_keeps_the_tiers_at_or_below_the_ceiling_and_lowers_the_default() {
        let all = AccessTiers::up_to(AccessTier::Bypass, AccessTier::Auto);
        assert_eq!(all.ceiling(), Some(AccessTier::Bypass));
        let plan = all.narrowed_to(AccessTier::Plan).unwrap();
        assert_eq!(plan.honoured(), [AccessTier::Plan]);
        assert_eq!(plan.default_tier(), AccessTier::Plan);
        // A set that honours nothing at or below the ceiling is refused, never widened.
        let auto = AccessTiers::up_to(AccessTier::Auto, AccessTier::Auto);
        assert_eq!(
            auto.narrowed_to(AccessTier::Plan).unwrap().honoured(),
            [AccessTier::Plan]
        );
        // A default above the ceiling comes down with it.
        let edits = AccessTiers::up_to(AccessTier::Edits, AccessTier::Edits);
        assert_eq!(
            edits.narrowed_to(AccessTier::Plan).unwrap().default_tier(),
            AccessTier::Plan
        );
    }
}
