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
//! The key is **never inlined**: it is a *reference* that OpenCode resolves in the child, and which
//! reference depends on where the child runs ([`SecretRef`], which the run's environment gives,
//! [`EnvSession::secret_ref`]): `{env:MODEL_API_KEY}`, OpenCode's
//! own environment substitution, in this container, where the child inherits the variable from this
//! process; `{file:/run/adam/secrets/model-key}`, its file substitution, in a devcontainer, which
//! has no variable of ours and gets the key as a read-only file. So the config can be logged or shown in
//! a process listing without leaking anything.
//!
//! **Verified 2026-09-29** against the OpenCode source at
//! `sst/opencode@7945de2`, `packages/web/src/content/docs/providers.mdx`
//! (custom provider: `npm = "@ai-sdk/openai-compatible"`, `options.baseURL`,
//! `models` keyed by id with a display `name`) and `config.mdx` (`{env:VAR}`
//! substitution, which becomes an empty string when the variable is unset;
//! `model` is `provider_id/model_id`; `autoupdate: false`; `share`). The
//! pinned CLI is `opencode-ai@1.18.33`. `{file:path}` is the same documentation's file
//! substitution (*verified* 2026-10-01 in the slice 7b planning, in `config.mdx`: an absolute path is
//! read as it is). Not verified: a live request from OpenCode to a real gateway (see the crate README's
//! smoke test).
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
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use adam_acp::AcpCommand;
use adam_workspace::{EnvError, EnvSession, ExecSpec, PreparedCommand, SecretRef};
use serde_json::{Value, json};

/// Provider id under which the gateway is registered in OpenCode.
pub const PROVIDER_ID: &str = "gateway";

/// Environment variable OpenCode reads its inline JSON configuration from.
pub const CONFIG_ENV: &str = "OPENCODE_CONFIG_CONTENT";

/// Variables of this process that the OpenCode child must not see.
const HIDDEN_FROM_CHILD: &[&str] = &[
    "GITHUB_TOKEN",
    "GITHUB_APP_PRIVATE_KEY",
    "DATABASE_URL",
    "A2A_BEARER_TOKENS",
];

/// OpenCode's substitution for `secret`: `{env:NAME}` or `{file:/path}`, resolved by OpenCode itself
/// in the process that reads the configuration. Nothing is inlined.
pub fn secret_reference(secret: &SecretRef) -> String {
    match secret {
        SecretRef::Env(name) => format!("{{env:{name}}}"),
        SecretRef::File(path) => format!("{{file:{}}}", path.display()),
    }
}

/// The inline OpenCode configuration for `model` behind `base_url`, with the
/// key read by OpenCode itself from `api_key` (a [`secret_reference`], or `""` for a gateway that
/// needs none).
pub fn opencode_config(base_url: &str, model: &str, api_key: &str) -> Value {
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
                    "apiKey": api_key,
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
    /// The gateway and the model of the inline configuration, which is made for each run's
    /// environment (the key's reference depends on it). `None` for a launcher that is not OpenCode.
    gateway: Option<Gateway>,
}

/// What the inline configuration names.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Gateway {
    base_url: String,
    model: String,
}

impl OpenCodeLaunch {
    /// `opencode acp` configured for the gateway: the inline config (made by
    /// `exec_spec`, with the key by reference) and `OPENCODE_DISABLE_AUTOUPDATE=1`.
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
        env.insert("OPENCODE_DISABLE_AUTOUPDATE".to_owned(), "1".to_owned());
        Self {
            program,
            args,
            env,
            gateway: Some(Gateway {
                base_url: base_url.to_owned(),
                model: model.to_owned(),
            }),
        }
    }

    /// A launcher for an arbitrary ACP program (tests, other agents), with no
    /// OpenCode configuration.
    pub fn program(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            gateway: None,
        }
    }

    /// Add an environment variable for the child.
    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Whether this launcher is OpenCode (its configuration is made here), and not another ACP
    /// program (tests, other agents).
    pub(crate) fn is_opencode(&self) -> bool {
        self.gateway.is_some()
    }

    /// `opencode --version` in `cwd`: what shows whether OpenCode starts in the run's environment (a
    /// devcontainer whose image has no glibc, or another architecture, cannot run the coder's
    /// binary). The same program the launch would start there.
    pub(crate) fn version_spec(&self, cwd: &Path, session: &dyn EnvSession) -> ExecSpec {
        let program = session
            .tool_path("opencode")
            .map_or_else(|| self.program.clone(), PathBuf::from);
        ExecSpec::argv([program.into_os_string(), OsString::from("--version")], cwd)
            .hide(HIDDEN_FROM_CHILD.iter().copied())
    }

    /// What to run in `cwd`, for the run's environment to prepare
    /// ([`EnvSession::prepare`](adam_workspace::EnvSession::prepare)): the program and its arguments,
    /// the configuration, and the secrets of this process that OpenCode has no use for to hide.
    ///
    /// `session` is the run's environment. It says where OpenCode is in it
    /// ([`tool_path`](EnvSession::tool_path): in a devcontainer, the coder's own copy, mounted; here,
    /// where the launch names it) and how a process there reads the model key
    /// ([`secret_ref`](EnvSession::secret_ref) of `"model-key"`): OpenCode reads it by reference,
    /// `{env:MODEL_API_KEY}` here or `{file:...}` in a devcontainer, so the key is in no argument
    /// and no variable of the configuration. An environment with no key to give gets an empty key in
    /// the configuration. `MODEL_API_KEY` is not among the names to hide: in this container it is what
    /// the reference reads.
    pub(crate) fn exec_spec(&self, cwd: &Path, session: &dyn EnvSession) -> ExecSpec {
        let program = if self.gateway.is_some() {
            session.tool_path("opencode")
        } else {
            None
        }
        .map_or_else(|| self.program.clone(), PathBuf::from)
        .into_os_string();
        let mut argv = vec![program];
        argv.extend(self.args.iter().map(OsString::from));
        let mut spec = ExecSpec::argv(argv, cwd).hide(HIDDEN_FROM_CHILD.iter().copied());
        spec.env.clone_from(&self.env);
        if let Some(gateway) = &self.gateway {
            let key = session
                .secret_ref("model-key")
                .as_ref()
                .map(secret_reference)
                .unwrap_or_default();
            spec.env.insert(
                CONFIG_ENV.to_owned(),
                opencode_config(&gateway.base_url, &gateway.model, &key).to_string(),
            );
        }
        spec
    }
}

/// `prepared` as the ACP client starts it: the program, the arguments, the directory and the
/// environment: added on top of this process's, with the names to remove blanked (the client
/// cannot remove a variable, and an empty one is as good as none for a secret), or, when the
/// environment says so ([`PreparedCommand::env_clear`]: a command that is the client of a process
/// running elsewhere), only what the environment set.
///
/// # Errors
///
/// [`EnvError::Refused`] for an argument or a value that is not UTF-8.
pub(crate) fn acp_command(prepared: &PreparedCommand) -> Result<AcpCommand, EnvError> {
    let text = |value: &OsString| {
        value.to_str().map(str::to_owned).ok_or_else(|| {
            EnvError::Refused("the ACP client takes only UTF-8 arguments and values".to_owned())
        })
    };
    let mut cmd = AcpCommand::new(prepared.program.clone(), prepared.cwd.clone());
    if prepared.env_clear {
        cmd = cmd.clear_env();
    }
    for arg in &prepared.args {
        cmd = cmd.arg(text(arg)?);
    }
    for (name, value) in &prepared.env {
        cmd = cmd.env(text(name)?, text(value)?);
    }
    for name in &prepared.env_remove {
        cmd = cmd.env(name, "");
    }
    Ok(cmd)
}

#[cfg(test)]
mod tests {
    use adam_workspace::{EnvSession, LocalSession, Program};

    use super::*;

    #[test]
    fn config_has_the_custom_provider_shape_and_no_secret() {
        let c = opencode_config(
            "https://gw.example/v1",
            "coder-large",
            "{env:MODEL_API_KEY}",
        );
        assert_eq!(c["model"], "gateway/coder-large");
        assert_eq!(c["autoupdate"], false);
        let p = &c["provider"]["gateway"];
        assert_eq!(p["npm"], "@ai-sdk/openai-compatible");
        assert_eq!(p["options"]["baseURL"], "https://gw.example/v1");
        assert_eq!(p["options"]["apiKey"], "{env:MODEL_API_KEY}");
        assert_eq!(p["models"]["coder-large"]["name"], "coder-large");
    }

    /// What the ACP client is given for `launch` in `cwd` in this container.
    fn local_command(launch: &OpenCodeLaunch, cwd: &str) -> AcpCommand {
        let prepared = LocalSession
            .prepare(&launch.exec_spec(Path::new(cwd), &LocalSession))
            .unwrap();
        acp_command(&prepared).unwrap()
    }

    #[test]
    fn launch_sets_config_and_blanks_unneeded_secrets() {
        let launch = OpenCodeLaunch::opencode("https://gw.example/v1", "m");
        let cmd = local_command(&launch, "/work/worktrees/r");
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
        assert_eq!(
            cmd.env.get("GITHUB_APP_PRIVATE_KEY").map(String::as_str),
            Some("")
        );
        assert_eq!(cmd.env.get("DATABASE_URL").map(String::as_str), Some(""));
        assert_eq!(
            cmd.env.get("A2A_BEARER_TOKENS").map(String::as_str),
            Some("")
        );
        assert!(
            cmd.env
                .get(CONFIG_ENV)
                .is_some_and(|c| c.contains("@ai-sdk/openai-compatible"))
        );
        assert!(
            !cmd.env.contains_key("MODEL_API_KEY"),
            "the key is inherited, never copied"
        );
        let config: Value = serde_json::from_str(&cmd.env[CONFIG_ENV]).unwrap();
        assert_eq!(
            config["provider"]["gateway"]["options"]["apiKey"], "{env:MODEL_API_KEY}",
            "in this container the key is the variable OpenCode inherits"
        );
    }

    #[test]
    fn the_spec_names_what_to_hide_and_never_carries_the_key() {
        let launch = OpenCodeLaunch::opencode("https://gw.example/v1", "m");
        let spec = launch.exec_spec(Path::new("/w"), &LocalSession);
        assert_eq!(
            spec.program,
            Program::Argv(vec!["opencode".into(), "acp".into()])
        );
        assert_eq!(
            spec.hide,
            [
                "GITHUB_TOKEN",
                "GITHUB_APP_PRIVATE_KEY",
                "DATABASE_URL",
                "A2A_BEARER_TOKENS"
            ]
        );
        assert!(!spec.hide.iter().any(|name| name == "MODEL_API_KEY"));
        assert!(!spec.env.contains_key("MODEL_API_KEY"));
        assert!(
            !spec.env.values().any(|v| v.contains("MODEL_API_KEY=")),
            "only the reference {{env:MODEL_API_KEY}} is in the configuration"
        );
    }

    #[test]
    fn a_hidden_name_stays_blank_even_when_the_launch_sets_it() {
        let launch = OpenCodeLaunch::program("agent").env("GITHUB_TOKEN", "from the launch");
        let cmd = local_command(&launch, "/w");
        assert_eq!(cmd.env.get("GITHUB_TOKEN").map(String::as_str), Some(""));
    }

    #[test]
    fn a_secret_is_a_reference_in_the_configuration_and_never_its_value() {
        assert_eq!(
            secret_reference(&SecretRef::Env("MODEL_API_KEY".into())),
            "{env:MODEL_API_KEY}"
        );
        let file = SecretRef::File(PathBuf::from("/run/adam/secrets/model-key"));
        assert_eq!(
            secret_reference(&file),
            "{file:/run/adam/secrets/model-key}"
        );
        let launch = OpenCodeLaunch::opencode("https://gw.example/v1", "m");
        let elsewhere = Elsewhere {
            key: Some(file),
            opencode: Some(PathBuf::from("/opt/adam/bin/opencode")),
        };
        let spec = launch.exec_spec(Path::new("/w"), &elsewhere);
        let config: Value = serde_json::from_str(&spec.env[CONFIG_ENV]).unwrap();
        assert_eq!(
            config["provider"]["gateway"]["options"]["apiKey"],
            "{file:/run/adam/secrets/model-key}"
        );
        // An environment with no key to give: an empty key, not a reference that names nothing.
        let none = Elsewhere {
            key: None,
            opencode: None,
        };
        let spec = launch.exec_spec(Path::new("/w"), &none);
        let config: Value = serde_json::from_str(&spec.env[CONFIG_ENV]).unwrap();
        assert_eq!(config["provider"]["gateway"]["options"]["apiKey"], "");
    }

    /// An environment that is not this container: its own key reference, its own OpenCode.
    struct Elsewhere {
        key: Option<SecretRef>,
        opencode: Option<PathBuf>,
    }

    #[async_trait::async_trait]
    impl EnvSession for Elsewhere {
        fn describe(&self) -> adam_workspace::EnvDescription {
            LocalSession.describe()
        }
        fn prepare(&self, spec: &ExecSpec) -> Result<PreparedCommand, EnvError> {
            LocalSession.prepare(spec)
        }
        async fn kill(&self, _exec: &adam_workspace::ExecId) {}
        fn tool_path(&self, name: &str) -> Option<PathBuf> {
            (name == "opencode")
                .then(|| self.opencode.clone())
                .flatten()
        }
        fn secret_ref(&self, _name: &str) -> Option<SecretRef> {
            self.key.clone()
        }
    }

    #[test]
    fn opencode_is_started_from_where_the_environment_has_it() {
        let launch = OpenCodeLaunch::from_command(
            &["/usr/local/bin/opencode".into(), "acp".into()],
            "u",
            "m",
        );
        let here = launch.exec_spec(Path::new("/w"), &LocalSession);
        assert_eq!(
            here.program,
            Program::Argv(vec!["/usr/local/bin/opencode".into(), "acp".into()])
        );
        let there = launch.exec_spec(
            Path::new("/w"),
            &Elsewhere {
                key: None,
                opencode: Some(PathBuf::from("/opt/adam/bin/opencode")),
            },
        );
        assert_eq!(
            there.program,
            Program::Argv(vec!["/opt/adam/bin/opencode".into(), "acp".into()]),
            "the mounted copy, with the launch's own arguments"
        );
        // A launcher that is not OpenCode is never swapped for it.
        let other = OpenCodeLaunch::program("/bin/agent").exec_spec(
            Path::new("/w"),
            &Elsewhere {
                key: None,
                opencode: Some(PathBuf::from("/opt/adam/bin/opencode")),
            },
        );
        assert_eq!(other.program, Program::Argv(vec!["/bin/agent".into()]));
    }

    #[test]
    fn an_environment_that_clears_the_environment_is_passed_on_to_the_client() {
        let mut prepared = LocalSession
            .prepare(&OpenCodeLaunch::program("agent").exec_spec(Path::new("/w"), &LocalSession))
            .unwrap();
        prepared.env_clear = true;
        prepared.env_remove.clear();
        let cmd = acp_command(&prepared).unwrap();
        assert!(cmd.env_clear, "the child starts from an empty environment");
        let kept = acp_command(
            &LocalSession
                .prepare(
                    &OpenCodeLaunch::program("agent").exec_spec(Path::new("/w"), &LocalSession),
                )
                .unwrap(),
        )
        .unwrap();
        assert!(!kept.env_clear, "this container's own environment is kept");
    }

    #[test]
    fn a_non_utf8_argument_is_refused() {
        use std::os::unix::ffi::OsStringExt as _;
        let mut prepared = LocalSession
            .prepare(&OpenCodeLaunch::program("agent").exec_spec(Path::new("/w"), &LocalSession))
            .unwrap();
        prepared.args.push(OsString::from_vec(vec![0xff, 0xfe]));
        let err = acp_command(&prepared).unwrap_err();
        assert!(matches!(err, EnvError::Refused(_)), "{err}");
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
        let spec = launch.exec_spec(Path::new("/w"), &LocalSession);
        assert!(spec.env.contains_key(CONFIG_ENV));
    }
}
