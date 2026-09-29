//! OpenCode's inline configuration and how the ACP child is launched.
//!
//! # The configuration
//!
//! [`opencode_config`] builds the JSON handed to OpenCode in
//! `OPENCODE_CONFIG_CONTENT`. It points OpenCode at the same OpenAI-compatible
//! gateway the agent uses, as a custom provider:
//!
//! ```json
//! {
//!   "$schema": "https://opencode.ai/config.json",
//!   "model": "gateway/<OPENCODE_MODEL>",
//!   "autoupdate": false,
//!   "share": "disabled",
//!   "provider": {
//!     "gateway": {
//!       "npm": "@ai-sdk/openai-compatible",
//!       "name": "Model gateway",
//!       "options": { "baseURL": "<MODEL_BASE_URL>", "apiKey": "{env:MODEL_API_KEY}" },
//!       "models": { "<OPENCODE_MODEL>": { "name": "<OPENCODE_MODEL>" } }
//!     }
//!   }
//! }
//! ```
//!
//! The key is **never inlined**: `{env:MODEL_API_KEY}` is OpenCode's own
//! environment substitution, resolved in the child, which inherits the
//! variable from this process. So the config can be logged or shown in a
//! process listing without leaking anything.
//!
//! **Verified 2026-09-29** against the OpenCode source at
//! `sst/opencode@7945de2`, `packages/web/src/content/docs/providers.mdx`
//! (custom provider: `npm = "@ai-sdk/openai-compatible"`, `options.baseURL`,
//! `models` keyed by id with a display `name`) and `config.mdx` (`{env:VAR}`
//! substitution, which becomes an empty string when the variable is unset;
//! `model` is `provider_id/model_id`; `autoupdate: false`; `share`). The
//! pinned CLI is `opencode-ai@1.18.33`. Not verified: a live request from
//! OpenCode to a real gateway (see the crate README's smoke test).
//!
//! # Launching
//!
//! [`OpenCodeLaunch`] is the command that speaks ACP on stdio: `opencode acp`
//! in production, the scripted `adam-acp-fake-agent` in tests. Secrets this
//! process holds that OpenCode has no use for (`GITHUB_TOKEN`, `DATABASE_URL`,
//! `A2A_BEARER_TOKENS`) are blanked in the child's environment: OpenCode runs
//! arbitrary model-chosen shell commands, and pushing is done by this process
//! through `adam-workspace`, never by the child.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use adam_acp::AcpCommand;
use serde_json::{Value, json};

/// Provider id under which the gateway is registered in OpenCode.
pub const PROVIDER_ID: &str = "gateway";

/// Environment variable OpenCode reads its inline JSON configuration from.
pub const CONFIG_ENV: &str = "OPENCODE_CONFIG_CONTENT";

/// Variables of this process that the OpenCode child must not see.
const HIDDEN_FROM_CHILD: &[&str] = &["GITHUB_TOKEN", "DATABASE_URL", "A2A_BEARER_TOKENS"];

/// The inline OpenCode configuration for `model` behind `base_url`, with the
/// key read from the environment variable `api_key_env` by OpenCode itself.
pub fn opencode_config(base_url: &str, model: &str, api_key_env: &str) -> Value {
    json!({
        "$schema": "https://opencode.ai/config.json",
        "model": format!("{PROVIDER_ID}/{model}"),
        "autoupdate": false,
        "share": "disabled",
        "provider": {
            PROVIDER_ID: {
                "npm": "@ai-sdk/openai-compatible",
                "name": "Model gateway",
                "options": {
                    "baseURL": base_url,
                    "apiKey": format!("{{env:{api_key_env}}}"),
                },
                "models": {
                    model: { "name": model },
                },
            },
        },
    })
}

/// How to start the ACP agent in a worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenCodeLaunch {
    /// The program (looked up on `PATH` if it has no directory part).
    pub program: PathBuf,
    /// Its arguments.
    pub args: Vec<String>,
    /// Environment added on top of this process's.
    pub env: BTreeMap<String, String>,
}

impl OpenCodeLaunch {
    /// `opencode acp` configured for the gateway: the inline config,
    /// `OPENCODE_DISABLE_AUTOUPDATE=1`, and the key by reference.
    pub fn opencode(base_url: &str, model: &str) -> Self {
        Self::from_command(&["opencode".to_owned(), "acp".to_owned()], base_url, model)
    }

    /// Like [`opencode`](Self::opencode) with a custom `program args...`
    /// (empty falls back to `opencode acp`).
    pub fn from_command(command: &[String], base_url: &str, model: &str) -> Self {
        let (program, args) = match command.split_first() {
            Some((program, args)) => (PathBuf::from(program), args.to_vec()),
            None => (PathBuf::from("opencode"), vec!["acp".to_owned()]),
        };
        let mut env = BTreeMap::new();
        env.insert(
            CONFIG_ENV.to_owned(),
            opencode_config(base_url, model, "MODEL_API_KEY").to_string(),
        );
        env.insert("OPENCODE_DISABLE_AUTOUPDATE".to_owned(), "1".to_owned());
        Self { program, args, env }
    }

    /// A launcher for an arbitrary ACP program (tests, other agents), with no
    /// OpenCode configuration.
    pub fn program(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
        }
    }

    /// Add an environment variable for the child.
    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// The command to run in `cwd`.
    pub(crate) fn command(&self, cwd: &Path) -> AcpCommand {
        let mut cmd = AcpCommand::new(self.program.clone(), cwd);
        cmd.args.clone_from(&self.args);
        for name in HIDDEN_FROM_CHILD {
            cmd = cmd.env(*name, "");
        }
        for (k, v) in &self.env {
            cmd = cmd.env(k, v);
        }
        cmd
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_has_the_custom_provider_shape_and_no_secret() {
        let c = opencode_config("https://gw.example/v1", "coder-large", "MODEL_API_KEY");
        assert_eq!(c["model"], "gateway/coder-large");
        assert_eq!(c["autoupdate"], false);
        let p = &c["provider"]["gateway"];
        assert_eq!(p["npm"], "@ai-sdk/openai-compatible");
        assert_eq!(p["options"]["baseURL"], "https://gw.example/v1");
        assert_eq!(p["options"]["apiKey"], "{env:MODEL_API_KEY}");
        assert_eq!(p["models"]["coder-large"]["name"], "coder-large");
    }

    #[test]
    fn launch_sets_config_and_blanks_unneeded_secrets() {
        let launch = OpenCodeLaunch::opencode("https://gw.example/v1", "m");
        let cmd = launch.command(Path::new("/work/worktrees/r"));
        assert_eq!(cmd.program, PathBuf::from("opencode"));
        assert_eq!(cmd.args, ["acp"]);
        assert_eq!(cmd.cwd, PathBuf::from("/work/worktrees/r"));
        assert_eq!(
            cmd.env
                .get("OPENCODE_DISABLE_AUTOUPDATE")
                .map(String::as_str),
            Some("1")
        );
        assert_eq!(cmd.env.get("GITHUB_TOKEN").map(String::as_str), Some(""));
        assert_eq!(cmd.env.get("DATABASE_URL").map(String::as_str), Some(""));
        assert!(
            cmd.env
                .get(CONFIG_ENV)
                .is_some_and(|c| c.contains("@ai-sdk/openai-compatible"))
        );
        assert!(
            !cmd.env.contains_key("MODEL_API_KEY"),
            "the key is inherited, never copied"
        );
    }

    #[test]
    fn a_custom_command_replaces_the_program_but_keeps_the_config() {
        let launch = OpenCodeLaunch::from_command(
            &[
                "/usr/local/bin/oc".into(),
                "acp".into(),
                "--print-logs".into(),
            ],
            "u",
            "m",
        );
        assert_eq!(launch.program, PathBuf::from("/usr/local/bin/oc"));
        assert_eq!(launch.args, ["acp", "--print-logs"]);
        assert!(launch.env.contains_key(CONFIG_ENV));
    }
}
