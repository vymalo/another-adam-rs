//! `adam-agent export`: the folder's runs as training records (ADR 0085, slice E2).
//!
//! It reads what an export needs and nothing a role needs: `ADAM_AGENT_DIR` (the folder, as
//! serving reads it), `DATABASE_URL`, `ADAM_EXTRA_MCP_FILE` when it is set, and
//! `EXPORT_PSEUDONYM_KEY`. No model key, A2A token or `PUBLIC_URL` is required, and serving's own
//! validation is unchanged. The runtime registers the starters a control plane registers (the
//! root's and each subagent's, so a child run's transcript joins) and runs nothing: no server, no
//! worker, no schedule, no MCP server.

use std::ffi::OsString;

use adam_export::{ExportOptions, Summary};
use adam_llm_agent::LlmStarter;
use adam_service::ConfigError;
use adam_service::export::{self, ExportArgs, PSEUDONYM_KEY_ENV};
use secrecy::{ExposeSecret as _, SecretString};

use crate::error::AgentError;
use crate::{config, folder, redact};

/// Why `export` stopped. The message names the step; the cause is the source.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ExportFailure {
    /// The environment does not read.
    #[error("reading the configuration")]
    Config(#[source] ConfigError),
    /// The folder, or the extra MCP file, does not read.
    #[error("reading the agent")]
    Agent(#[source] AgentError),
    /// The export itself.
    #[error("exporting")]
    Export(#[source] export::ExportCommandError),
}

/// Run `adam-agent export` with `args`, reading the process environment.
///
/// # Errors
///
/// [`ExportFailure`]; [`exit_code`](crate::exit_code) maps it like serving's errors (78 for the
/// configuration and the files, 69 for a store that is down).
pub async fn run(args: &ExportArgs) -> Result<Summary, ExportFailure> {
    let lookup = |name: &str| std::env::var(name).ok();
    let mut problems = Vec::new();
    let agent_dir = config::parse_agent_dir(&lookup, &mut problems);
    let database_url = database_url(&lookup, &mut problems);
    let extra_mcp_file = adam_service::parse_file(
        &|name: &str| lookup(name).filter(|v| !v.trim().is_empty()),
        adam::EXTRA_MCP_FILE_ENV,
        &mut problems,
    );
    let key = match export::pseudonym_key(std::env::var_os(PSEUDONYM_KEY_ENV)) {
        Ok(key) => key,
        Err(e) => {
            problems.extend(e.problems);
            None
        }
    };
    ConfigError::check(problems).map_err(ExportFailure::Config)?;

    let mut folder = folder::load(&agent_dir).map_err(ExportFailure::Agent)?;
    folder::log(&folder);
    if let Some(file) = &extra_mcp_file {
        let (def, _warnings) = folder
            .def
            .clone()
            .with_extra_mcp_file(file)
            .map_err(|e| ExportFailure::Agent(AgentError::ExtraMcp(Box::new(e))))?;
        folder.def = def;
    }
    let def = folder.def;

    // Every string of a record is scrubbed of the secrets this process holds: the database's
    // password, the variables named like secrets, and those the MCP servers read.
    let secrets = redact::export_secret_values(
        database_url.expose_secret(),
        redact::process_vars(),
        &def.mcp_env_references(),
    );
    let options =
        ExportOptions::new(export::pseudonyms(key.as_deref()).map_err(ExportFailure::Export)?)
            .scrub(move |text: &str| redact::scrub(&secrets, text))
            .include_reasoning(args.include_reasoning);
    let name = def.name().to_owned();
    let register: adam_service::Register =
        Box::new(move |builder| def.session_starters(builder.starter(LlmStarter::new(&name))));
    export::run(&database_url, register, args, &options)
        .await
        .map_err(ExportFailure::Export)
}

/// `DATABASE_URL`, required.
fn database_url(
    lookup: &impl Fn(&str) -> Option<String>,
    problems: &mut Vec<String>,
) -> SecretString {
    match lookup("DATABASE_URL").filter(|v| !v.trim().is_empty()) {
        Some(url) => SecretString::from(url),
        None => {
            problems.push("DATABASE_URL is required".into());
            SecretString::from(String::new())
        }
    }
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
        assert_eq!(subcommand(args(&["serve"]).into_iter()), None);
        assert_eq!(subcommand(args(&["--out", "export"]).into_iter()), None);
        assert_eq!(
            subcommand(args(&["export", "--status", "all"]).into_iter()),
            Some(args(&["--status", "all"]))
        );
    }

    #[test]
    fn the_export_parses_the_contract_s_flags() {
        let args = [
            "--agent",
            "chat",
            "--require",
            "lang=rust",
            "--out",
            "x.jsonl",
        ]
        .into_iter()
        .map(OsString::from);
        let adam_service::export::Command::Export(args) =
            adam_service::export::parse_args(args).expect("valid")
        else {
            panic!("not an export");
        };
        assert_eq!(args.agents, ["chat"]);
        assert!(adam_service::export::parse_args([OsString::from("--current-tools")]).is_err());
    }

    #[test]
    fn the_export_needs_the_database_and_the_folder_only() {
        let dir = tempfile::tempdir().expect("a directory");
        let dir_text = dir.path().display().to_string();
        let lookup = |name: &str| match name {
            "DATABASE_URL" => Some("sqlite:///tmp/x.sqlite".to_owned()),
            n if n == adam::AGENT_DIR_ENV => Some(dir_text.clone()),
            _ => None,
        };
        let mut problems = Vec::new();
        assert_eq!(config::parse_agent_dir(&lookup, &mut problems), dir.path());
        assert_eq!(
            database_url(&lookup, &mut problems).expose_secret(),
            "sqlite:///tmp/x.sqlite"
        );
        assert!(problems.is_empty(), "{problems:?}");
        database_url(&|_: &str| None, &mut problems);
        config::parse_agent_dir(&|_: &str| None, &mut problems);
        assert_eq!(problems.len(), 2, "{problems:?}");
    }

    #[test]
    fn the_export_s_secrets_are_the_database_password_and_the_named_variables() {
        let vars = [
            ("MODEL_API_KEY", "sk-model-key"),
            ("A2A_BEARER_TOKENS", "tok-one-1,tok-two-2"),
            ("SEARCH_ACCESS", "search-credential"),
            ("PATH", "/usr/bin:/bin"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned()));
        let named = std::collections::BTreeSet::from(["SEARCH_ACCESS".to_owned()]);
        let secrets = redact::export_secret_values("postgres://u:p%40ss@db/adam", vars, &named);
        for secret in [
            "sk-model-key",
            "tok-one-1",
            "tok-two-2",
            "search-credential",
            "p@ss",
            "p%40ss",
        ] {
            assert!(secrets.iter().any(|s| s == secret), "{secret} is not known");
        }
        assert!(!secrets.iter().any(|s| s == "/usr/bin:/bin"));
        assert_eq!(
            redact::scrub(&secrets, "key=sk-model-key"),
            format!("key={}", redact::REDACTED)
        );
    }
}
