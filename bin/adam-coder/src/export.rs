//! `adam-coder export`: the coder's runs as training records (ADR 0087, slice E2).
//!
//! It reads what an export needs and nothing a role needs: `DATABASE_URL`, `ADAM_AGENT_DIR` (the
//! folder, or the embedded copy when it is unset, as serving reads it), `ADAM_EXTRA_MCP_FILE` when
//! it is set, and `EXPORT_PSEUDONYM_KEY`. No model, GitHub or A2A variable is required, and
//! serving's own validation is unchanged. The runtime registers the starters a control plane
//! registers (`coder` and each of its subagents, so a child run's transcript joins) and runs
//! nothing: no server, no worker, no janitor, no schedule, no MCP server.
//!
//! The scrub is the coder's [`Redactor`]: built from the whole serving configuration when the
//! environment holds one ([`RedactorFromConfig`]: the model key, the GitHub token or App key, the
//! A2A tokens, the database's password), else from the database's password; and in both cases
//! also every variable named like a secret and every variable the agent's `mcp.json` files read.

use std::ffi::OsString;

use adam_export::{ExportOptions, Summary};
use adam_service::ConfigError;
use adam_service::export::{self, ExportArgs, PSEUDONYM_KEY_ENV};
use anyhow::Context as _;
use secrecy::{ExposeSecret as _, SecretString};

use crate::redact::{Redactor, RedactorFromConfig, database_password};
use crate::{AgentFiles, CoderStarter, Config};

/// The words in a variable's name that say its value is a secret (those `adam-agent` reads).
const SECRET_WORDS: [&str; 8] = [
    "KEY",
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "PASSPHRASE",
    "CREDENTIAL",
    "PRIVATE",
];

/// Run `adam-coder export` with `args`, reading the process environment.
///
/// # Errors
///
/// The configuration (78), the agent files (78), the store (69 when it is down), the output file
/// (71), as [`exit_code`](crate::exit_code) maps them.
pub async fn run(args: &ExportArgs) -> anyhow::Result<Summary> {
    let lookup = |name: &str| std::env::var(name).ok();
    let get = |name: &str| lookup(name).filter(|v| !v.trim().is_empty());
    let mut problems = Vec::new();
    let agent_dir = crate::config::parse_agent_dir(&get, &mut problems);
    let database_url = match get("DATABASE_URL") {
        Some(url) => SecretString::from(url),
        None => {
            problems.push("DATABASE_URL is required".into());
            SecretString::from(String::new())
        }
    };
    let extra_mcp_file = adam_service::parse_file(&get, adam::EXTRA_MCP_FILE_ENV, &mut problems);
    let key = match export::pseudonym_key(std::env::var_os(PSEUDONYM_KEY_ENV)) {
        Ok(key) => key,
        Err(e) => {
            problems.extend(e.problems);
            None
        }
    };
    ConfigError::check(problems).context("reading the configuration")?;

    let files = AgentFiles::load(agent_dir.as_deref()).context("reading the agent files")?;
    files.log();
    let mut def = files
        .def()
        .map_err(|e| *e)
        .context("reading the agent definition")?;
    if let Some(file) = &extra_mcp_file {
        def = def
            .with_extra_mcp_file(file)
            .context("adding the extra MCP servers (ADAM_EXTRA_MCP_FILE)")?
            .0;
    }

    let redactor = redactor(
        Config::from_lookup(lookup).ok().as_ref(),
        database_url.expose_secret(),
        // The environment as pairs, leaving out what is not text.
        std::env::vars_os().filter_map(|(name, value)| {
            Some((name.into_string().ok()?, value.into_string().ok()?))
        }),
    );
    let mcp_names = def.mcp_env_references();
    redactor.add_env_values(
        mcp_names
            .iter()
            .map(String::as_str)
            .filter(|name| !crate::mcp_secrets::is_never_hidden(name)),
        lookup,
    );
    let options = ExportOptions::new(export::pseudonyms(key.as_deref())?)
        .scrub(move |text: &str| redactor.scrub(text).into_owned())
        .include_reasoning(args.include_reasoning);
    let register: adam_service::Register =
        Box::new(move |builder| def.session_starters(builder.starter(CoderStarter)));
    export::run(&database_url, register, args, &options)
        .await
        .context("exporting")
}

/// The redactor of an export: `config`'s ([`RedactorFromConfig`]) when the environment reads as a
/// serving configuration, else the password of `database_url`; then the value of each variable of
/// `vars` named like a secret, and each part of a comma-separated one.
fn redactor(
    config: Option<&Config>,
    database_url: &str,
    vars: impl IntoIterator<Item = (String, String)>,
) -> Redactor {
    let redactor = match config {
        Some(config) => Redactor::from_config(config),
        None => Redactor::new(database_password(database_url)),
    };
    for (name, value) in vars {
        let upper = name.to_ascii_uppercase();
        if SECRET_WORDS.iter().any(|word| upper.contains(word)) {
            for part in value.split(',').map(str::trim) {
                redactor.add_fixed(part);
            }
            redactor.add_fixed(&value);
        }
    }
    redactor
}

/// Whether the command line asks for `export` (its first argument), and the arguments after it.
pub fn subcommand(mut args: impl Iterator<Item = OsString>) -> Option<Vec<OsString>> {
    match args.next() {
        Some(first) if first == "export" => Some(args.collect()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_first_argument_export_is_the_subcommand() {
        let args = |list: &[&str]| list.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(subcommand(args(&[]).into_iter()), None);
        assert_eq!(subcommand(args(&["--out", "export"]).into_iter()), None);
        assert_eq!(
            subcommand(args(&["export", "--agent", "coder"]).into_iter()),
            Some(args(&["--agent", "coder"]))
        );
    }

    #[test]
    fn the_export_parses_the_contract_s_flags() {
        let args = ["--agent=coder", "--status", "failed", "--include-reasoning"]
            .into_iter()
            .map(OsString::from);
        let export::Command::Export(args) = export::parse_args(args).expect("valid") else {
            panic!("not an export");
        };
        assert_eq!(args.agents, ["coder"]);
        assert!(args.include_reasoning);
        assert!(export::parse_args([OsString::from("--bogus")]).is_err());
    }

    #[test]
    fn without_a_serving_configuration_the_scrub_knows_the_named_secrets() {
        let vars = [
            ("GITHUB_TOKEN", "ghp_not_a_real_token_value"),
            ("A2A_BEARER_TOKENS", "tok-one-1, tok-two-2"),
            ("HOME", "/home/coder"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned()));
        let r = redactor(None, "postgres://u:p%40ss%2Fw0rd@db/adam", vars);
        for leaked in [
            "ghp_not_a_real_token_value",
            "tok-one-1",
            "tok-two-2",
            "p%40ss%2Fw0rd",
            "p@ss/w0rd",
        ] {
            assert_eq!(
                r.scrub(&format!("<{leaked}>")),
                format!("<{REDACTED}>"),
                "{leaked}"
            );
        }
        assert_eq!(r.scrub("/home/coder"), "/home/coder");
    }

    #[test]
    fn with_a_serving_configuration_the_scrub_knows_its_secrets_too() {
        let vars = std::collections::HashMap::from([
            ("DATABASE_URL", "postgres://u:dbpass1@db/adam"),
            ("MODEL_BASE_URL", "https://gw.example/v1"),
            ("MODEL_API_KEY", "sk-model-key"),
            ("MODEL", "m"),
            ("GITHUB_TOKEN", "ghp_token_value"),
            ("A2A_BEARER_TOKENS", "tok-one-1"),
            ("PUBLIC_URL", "http://coder:8080/"),
        ]);
        let config = Config::from_lookup(|k| vars.get(k).map(|v| (*v).to_owned()))
            .expect("a serving configuration");
        let r = redactor(Some(&config), "postgres://u:dbpass1@db/adam", []);
        for leaked in ["dbpass1", "sk-model-key", "ghp_token_value", "tok-one-1"] {
            assert_eq!(r.scrub(leaked), REDACTED, "{leaked}");
        }
    }

    use crate::redact::REDACTED;
}
