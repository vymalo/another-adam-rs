//! Configuration of the `adam-agent` binary, read from the environment.
//!
//! The variables every agent binary shares are read by [`adam_service`], with the same names,
//! defaults and messages as in `adam-coder`; this binary adds the one that says which agent it is.
//!
//! | Variable | Meaning | Default |
//! |---|---|---|
//! | `ADAM_AGENT_DIR` | the folder that holds `agent/` (or `agent/` itself): the one agent this process serves. **Required by every role** (there is no embedded agent), and an existing directory | required |
//! | `ROLE` | what this process runs: `all`, `control-plane` or `worker` ([`adam_host::Role`](https://docs.rs/adam-host)) | `all` |
//! | `DATABASE_URL` | Postgres for the run store | required |
//! | `A2A_BEARER_TOKENS` | comma-separated tokens accepted by the A2A server | required for `all` and `control-plane`, non-empty (fail closed) |
//! | `PUBLIC_URL` | URL clients reach the JSON-RPC endpoint at (agent card) | required for `all` and `control-plane` |
//! | `LISTEN_ADDR` | bind address: the A2A server, or for `worker` its `/healthz` listener | `0.0.0.0:8080` |
//! | `WORKERS` | runs advanced concurrently by this process | `4` |
//! | `WORKER_ID` | lease identity of this worker; letters, digits, `.`, `_`, `-` | random per process |
//! | `MODEL_BASE_URL` | OpenAI-compatible gateway, with its `/v1` prefix | required for `all` and `worker` |
//! | `MODEL_API_KEY` | bearer token for it (may be empty for local servers) | required for `all` and `worker` |
//! | `MODEL` | model alias of the agent | required for `all` and `worker` |
//! | `MCP_ALLOW_STDIO` | let the folder's `mcp.json` start local processes (`command` servers) | `false` |
//! | `MCP_ALLOW_INSECURE` | let it reach plain-`http` MCP servers on other machines (development only) | `false` |
//! | `MCP_ALLOW_URL_VARS` | let it write `${VAR}` in a server's `url` (headers may always) | `false` |
//!
//! Every problem is reported at once, so a misconfigured deployment is fixed in one round trip.
//! What a role does not use is not validated (a control plane connects no model and no MCP
//! server), except `ADAM_AGENT_DIR`, which every role reads: the control plane serves the card
//! of the folder and the workers run its prompt.

use std::path::PathBuf;

use adam::AGENT_DIR_ENV;
pub use adam_service::{ConfigError, McpSettings};
use adam_service::{ModelConfig, ServiceConfig};

/// The binary's configuration.
#[derive(Clone)]
pub struct Config {
    /// What every agent binary reads: the role, the database, how the process is reached, how
    /// many runs it advances.
    pub service: ServiceConfig,
    /// `ADAM_AGENT_DIR`: the folder the agent's files are read from, an existing directory.
    pub agent_dir: PathBuf,
    /// What stepping a run needs besides the files. `Some` exactly when
    /// [`Role::runs_workers`](adam_service::ServiceConfig::role); a control plane holds none of it.
    pub worker: Option<WorkerConfig>,
}

/// What the roles that run workers (`all`, `worker`) need besides the agent's files.
#[derive(Clone)]
pub struct WorkerConfig {
    /// `MODEL_BASE_URL`, `MODEL_API_KEY` and `MODEL`.
    pub model: ModelConfig,
    /// `MCP_ALLOW_*`: what the MCP servers of the folder's `mcp.json` may be.
    pub mcp: McpSettings,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("service", &self.service)
            .field("agent_dir", &self.agent_dir)
            .field("worker", &self.worker)
            .finish()
    }
}

impl std::fmt::Debug for WorkerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerConfig")
            .field("model", &self.model)
            .field("mcp", &self.mcp)
            .finish()
    }
}

impl Config {
    /// Read the process environment.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] listing every missing or malformed variable.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Read variables through `lookup` (`None` = unset). Blank values count as unset, except
    /// `MODEL_API_KEY`, which may be empty.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] listing every missing or malformed variable.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let mut problems = Vec::new();
        let service = ServiceConfig::parse(&lookup, &mut problems);

        // No default and no embedded copy: a silent persona would hide a missing mount.
        let agent_dir = match lookup(AGENT_DIR_ENV).filter(|v| !v.trim().is_empty()) {
            None => {
                problems.push(format!(
                    "{AGENT_DIR_ENV} is required: the folder that holds `agent/` (or `agent/` itself); \
                     this binary serves the agent that folder describes and has none of its own"
                ));
                PathBuf::new()
            }
            Some(raw) => {
                let path = PathBuf::from(raw);
                if !path.is_dir() {
                    problems.push(format!(
                        "{AGENT_DIR_ENV} {:?} is not a directory (name the folder that holds \
                         `agent/`, or `agent/` itself)",
                        path.display().to_string()
                    ));
                }
                path
            }
        };

        // The workers' variables: a control plane neither needs nor validates them.
        let worker = service.worker.is_some().then(|| WorkerConfig {
            model: ModelConfig::parse(&lookup, &mut problems),
            mcp: McpSettings::parse(&lookup, &mut problems),
        });

        ConfigError::check(problems)?;
        Ok(Self {
            service,
            agent_dir,
            worker,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use adam_host::Role;
    use secrecy::ExposeSecret as _;

    use super::*;

    type Vars = HashMap<String, String>;

    /// A valid configuration for the role `all`, with `ADAM_AGENT_DIR` naming `dir`.
    fn full(dir: &std::path::Path) -> Vars {
        [
            ("DATABASE_URL", "postgres://u:hunter2@db/adam"),
            ("MODEL_BASE_URL", "https://gw.example/v1"),
            ("MODEL_API_KEY", "sk-secret"),
            ("MODEL", "large"),
            ("A2A_BEARER_TOKENS", "one,two"),
            ("PUBLIC_URL", "http://agent.svc:8080/"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .chain([(AGENT_DIR_ENV.to_owned(), dir.display().to_string())])
        .collect()
    }

    fn parse(vars: &Vars) -> Result<Config, ConfigError> {
        Config::from_lookup(|k| vars.get(k).cloned())
    }

    fn problems_of(vars: &Vars) -> Vec<String> {
        parse(vars).unwrap_err().problems
    }

    #[test]
    fn a_valid_configuration_reads_the_service_the_model_and_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        let c = parse(&full(dir.path())).expect("valid");
        assert_eq!(c.agent_dir, dir.path());
        assert_eq!(c.service.role, Role::All);
        assert_eq!(c.service.listen_addr, "0.0.0.0:8080".parse().unwrap());
        let worker = c.worker.expect("the default role runs workers");
        assert_eq!(worker.model.alias, "large");
        assert_eq!(worker.model.api_key.expose_secret(), "sk-secret");
        assert_eq!(worker.mcp, McpSettings::default());
    }

    /// No default, no embedded copy: without the variable the process does not start, and the
    /// message says what to set.
    #[test]
    fn the_agent_dir_is_required_by_every_role() {
        let dir = tempfile::tempdir().unwrap();
        for role in Role::VALUES {
            let mut vars = full(dir.path());
            vars.insert("ROLE".into(), role.as_str().into());
            vars.remove(AGENT_DIR_ENV);
            let problems = problems_of(&vars);
            let problem = problems
                .iter()
                .find(|p| p.starts_with(AGENT_DIR_ENV))
                .unwrap_or_else(|| panic!("{role}: {problems:?}"));
            assert!(problem.contains("is required"), "{problem}");
            // Blank is unset.
            vars.insert(AGENT_DIR_ENV.into(), "  ".into());
            assert!(
                problems_of(&vars)
                    .iter()
                    .any(|p| p.starts_with(AGENT_DIR_ENV)),
                "{role}"
            );
        }
    }

    #[test]
    fn the_agent_dir_must_be_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, "x").unwrap();
        for bad in [dir.path().join("nowhere"), file] {
            let mut vars = full(dir.path());
            vars.insert(AGENT_DIR_ENV.into(), bad.display().to_string());
            let problems = problems_of(&vars);
            let problem = problems
                .iter()
                .find(|p| p.starts_with(AGENT_DIR_ENV))
                .unwrap_or_else(|| panic!("{problems:?}"));
            assert!(problem.contains("is not a directory"), "{problem}");
            assert!(problem.contains(&bad.display().to_string()), "{problem}");
        }
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let problems = problems_of(&HashMap::new());
        for name in [
            "DATABASE_URL",
            "PUBLIC_URL",
            "A2A_BEARER_TOKENS",
            "MODEL_BASE_URL",
            "MODEL_API_KEY",
            "MODEL",
            AGENT_DIR_ENV,
        ] {
            assert!(
                problems.iter().any(|p| p.starts_with(name)),
                "{name} missing from {problems:?}"
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let mut vars = full(dir.path());
        vars.insert("MCP_ALLOW_STDIO".into(), "maybe".into());
        vars.insert("WORKERS".into(), "0".into());
        let problems = problems_of(&vars);
        for name in ["MCP_ALLOW_STDIO", "WORKERS"] {
            assert!(problems.iter().any(|p| p.starts_with(name)), "{problems:?}");
        }
    }

    #[test]
    fn the_mcp_flags_are_read_by_the_roles_that_run_workers() {
        let dir = tempfile::tempdir().unwrap();
        let mut vars = full(dir.path());
        vars.insert("MCP_ALLOW_STDIO".into(), "true".into());
        vars.insert("MCP_ALLOW_INSECURE".into(), "1".into());
        vars.insert("MCP_ALLOW_URL_VARS".into(), "true".into());
        let mcp = parse(&vars).unwrap().worker.unwrap().mcp;
        assert!(mcp.allow_stdio && mcp.allow_insecure && mcp.allow_url_vars);
    }

    /// A control plane serves the card and starts runs, which needs no model and no MCP server, so
    /// it reads and validates none of their variables.
    #[test]
    fn a_control_plane_needs_no_model_and_validates_no_worker_variable() {
        let dir = tempfile::tempdir().unwrap();
        let mut vars = full(dir.path());
        vars.insert("ROLE".into(), "control-plane".into());
        for name in ["MODEL_BASE_URL", "MODEL_API_KEY", "MODEL"] {
            vars.remove(name);
        }
        vars.insert("MCP_ALLOW_STDIO".into(), "maybe".into());
        vars.insert("WORKERS".into(), "0".into());
        let c = parse(&vars).expect("a control plane starts runs, it does not step them");
        assert!(c.worker.is_none());
        assert!(c.service.worker.is_none());
    }

    #[test]
    fn a_worker_needs_no_front_variables_and_hides_its_secrets_from_debug() {
        let dir = tempfile::tempdir().unwrap();
        let mut vars = full(dir.path());
        vars.insert("ROLE".into(), "worker".into());
        vars.remove("A2A_BEARER_TOKENS");
        vars.remove("PUBLIC_URL");
        let c = parse(&vars).expect("a worker serves no A2A");
        let text = format!("{c:?}");
        for secret in ["hunter2", "sk-secret"] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
        assert!(text.contains("agent_dir"), "{text}");
    }
}
