//! The access tiers of `access-tier/v1`, re-exported from [`adam_model::tier`]: four words a person
//! picks for one message, and what an agent says it honours.
//!
//! The vocabulary lives in `adam-model` so that this crate, `adam-runtime`'s `SessionConfig` and
//! `SessionBounds` and the agent loop share **one** closed list (ADR 0054, slice 8, which adds a
//! session's `access_tier`). Nothing changed for a caller: these are the same types at the same
//! paths.

pub use adam_model::{AccessTier, AccessTiers, AccessTiersError, NotATier};
