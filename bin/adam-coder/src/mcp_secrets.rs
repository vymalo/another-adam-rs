//! The variables an agent's `mcp.json` files refer to as `${VAR}` are secrets of this process: a
//! key in a header, a token in a stdio server's `env`. They must reach neither the code a run
//! executes nor the model, so [`protect`] hides every one of them from the processes of runs
//! ([`HidingEnvironment`]) and registers its value with the [`Redactor`], for good.
//!
//! Not every name a file may read is a secret, and hiding some would break the coder itself: an
//! `env` of a stdio server that says `"PATH": "${PATH}"` would blank `PATH` for every command and
//! redact it from every output, and `${MODEL_API_KEY}` would blank the key OpenCode reads. Those
//! names ([`NEVER_HIDDEN`], and `LC_*`) are skipped, with a warning that names the variable (never
//! a value): the server still gets the value when it connects, and the coder's own children keep
//! what they need. The alternative, refusing to start, would punish a harmless `${HOME}` in a
//! server's `env`.

use std::collections::BTreeSet;
use std::sync::Arc;

use adam_workspace::{DynEnvironment, HidingEnvironment};

use crate::redact::Redactor;

/// Variables of the coder's own environment that its children need and that are not secrets
/// (`MODEL_API_KEY` is the exception: it is a secret, but the coder's OpenCode reads it, and it is
/// already redacted from the configuration). Never hidden and never redacted, whatever a file says.
pub const NEVER_HIDDEN: &[&str] = &["MODEL_API_KEY", "PATH", "HOME", "LANG", "TMPDIR", "USER"];

/// Whether `name` is one the coder's children need: [`NEVER_HIDDEN`], or a locale (`LC_*`).
pub fn is_never_hidden(name: &str) -> bool {
    NEVER_HIDDEN.contains(&name) || name.starts_with("LC_")
}

/// What [`protect`] did, for the startup log: names and counts, never a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Protected {
    /// The names hidden from every process of every run, sorted.
    pub hidden: Vec<String>,
    /// The names a file refers to that were left alone ([`is_never_hidden`]).
    pub skipped: Vec<String>,
    /// How many values were registered with the redactor (a name with no value, or one too short
    /// to be safe to replace, adds none).
    pub registered: usize,
}

/// Protect the variables `names` (`AgentDef::mcp_env_references`): register each value (read with
/// `lookup`) with `redactor`, permanently, and list the names to hide. Names that
/// [`is_never_hidden`] are skipped with a `warn!`.
pub fn protect(
    names: &BTreeSet<String>,
    redactor: &Redactor,
    lookup: impl Fn(&str) -> Option<String>,
) -> Protected {
    let (skipped, hidden): (Vec<String>, Vec<String>) = names
        .iter()
        .cloned()
        .partition(|name| is_never_hidden(name));
    for name in &skipped {
        tracing::warn!(
            variable = %name,
            "an MCP server's file refers to this variable, which the coder's own processes need: it is \
             neither hidden from them nor redacted (the server still reads it when it connects)"
        );
    }
    let registered = redactor.add_env_values(hidden.iter().map(String::as_str), lookup);
    Protected {
        hidden,
        skipped,
        registered,
    }
}

/// `environment`, hiding `names` from every process it runs. With no names it is `environment`.
pub fn hiding(environment: DynEnvironment, names: &[String]) -> DynEnvironment {
    if names.is_empty() {
        environment
    } else {
        Arc::new(HidingEnvironment::new(environment, names.iter().cloned()))
    }
}
