//! Configuration of the `adam-coder` binary, read from the environment.
//!
//! | Variable | Meaning | Default |
//! |---|---|---|
//! | `DATABASE_URL` | Postgres for the run store (`adam-store-postgres`) | required |
//! | `MODEL_BASE_URL` | OpenAI-compatible gateway, with its `/v1` prefix | required |
//! | `MODEL_API_KEY` | bearer token for it (may be empty for local servers) | required |
//! | `MODEL` | model alias of the agent itself | required |
//! | `OPENCODE_MODEL` | model alias OpenCode uses through the same gateway | `MODEL` |
//! | `GITHUB_TOKEN` | git push and pull request token; only ever sent to the `ALLOWED_REPO_HOSTS` | required |
//! | `ALLOWED_REPO_HOSTS` | comma-separated hosts (`name` for any port, or `name:port`) repositories may live on; the token is scoped to them | `github.com` |
//! | `ALLOW_LOCAL_REPOS` | also accept local paths, `file://` and plain `http://` repositories (development and tests only) | `false` |
//! | `GITHUB_API_URL` | GitHub REST API root (GitHub Enterprise: `https://<host>/api/v3`; tests: a mock) | `https://api.github.com` |
//! | `WORKSPACE_ROOT` | mirrors and worktrees (persistent storage) | `/work` |
//! | `A2A_BEARER_TOKENS` | comma-separated tokens accepted by the A2A server | required, non-empty (fail closed) |
//! | `PUBLIC_URL` | URL clients reach the JSON-RPC endpoint at (agent card) | required |
//! | `LISTEN_ADDR` | bind address | `0.0.0.0:8080` |
//! | `WORKERS` | runs advanced concurrently by this process | `4` |
//! | `MAX_CHECK_CYCLES` | failed `run_checks` before the agent must stop | `3` |
//! | `CHECK_TIMEOUT_SECS` | time limit of one `run_checks` command | `900` |
//! | `CHECK_OUTPUT_TAIL_BYTES` | output tail `run_checks` returns | `16384` |
//! | `GIT_AUTHOR_NAME`, `GIT_AUTHOR_EMAIL` | identity of the commits | `adam-coder`, `adam-coder@users.noreply.github.com` |
//! | `PR_DRAFT` | open pull requests as drafts (`true`/`false`) | `false` |
//! | `OPENCODE_COMMAND` | program that speaks ACP on stdio (arguments follow, whitespace-separated) | `opencode acp` |
//!
//! Every problem is reported at once, so a misconfigured deployment is fixed
//! in one round trip. Secrets are wrapped in [`SecretString`] and never appear
//! in `Debug` output.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use secrecy::SecretString;
use url::Url;

/// One or more environment variables are missing or unusable.
#[derive(Debug, thiserror::Error)]
#[error("invalid configuration:\n  - {}", problems.join("\n  - "))]
pub struct ConfigError {
    /// One line per problem.
    pub problems: Vec<String>,
}

/// The binary's configuration.
#[derive(Clone)]
pub struct Config {
    /// `DATABASE_URL`.
    pub database_url: SecretString,
    /// `MODEL_BASE_URL`.
    pub model_base_url: String,
    /// `MODEL_API_KEY`.
    pub model_api_key: SecretString,
    /// `MODEL`.
    pub model: String,
    /// `OPENCODE_MODEL`.
    pub opencode_model: String,
    /// `GITHUB_TOKEN`.
    pub github_token: SecretString,
    /// `ALLOWED_REPO_HOSTS`, lowercased.
    pub allowed_repo_hosts: Vec<String>,
    /// `ALLOW_LOCAL_REPOS`.
    pub allow_local_repos: bool,
    /// `GITHUB_API_URL`.
    pub github_api_url: Url,
    /// `WORKSPACE_ROOT`.
    pub workspace_root: PathBuf,
    /// `A2A_BEARER_TOKENS`.
    pub a2a_bearer_tokens: Vec<SecretString>,
    /// `PUBLIC_URL`.
    pub public_url: Url,
    /// `LISTEN_ADDR`.
    pub listen_addr: SocketAddr,
    /// `WORKERS`.
    pub workers: usize,
    /// `MAX_CHECK_CYCLES`.
    pub max_check_cycles: u32,
    /// `CHECK_TIMEOUT_SECS`.
    pub check_timeout: Duration,
    /// `CHECK_OUTPUT_TAIL_BYTES`.
    pub check_output_tail: usize,
    /// `GIT_AUTHOR_NAME`.
    pub git_author_name: String,
    /// `GIT_AUTHOR_EMAIL`.
    pub git_author_email: String,
    /// `PR_DRAFT`.
    pub pr_draft: bool,
    /// `OPENCODE_COMMAND`, split into program and arguments.
    pub opencode_command: Vec<String>,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("database_url", &"[REDACTED]")
            .field("model_base_url", &self.model_base_url)
            .field("model", &self.model)
            .field("opencode_model", &self.opencode_model)
            .field("allowed_repo_hosts", &self.allowed_repo_hosts)
            .field("allow_local_repos", &self.allow_local_repos)
            .field("github_api_url", &self.github_api_url.as_str())
            .field("workspace_root", &self.workspace_root)
            .field("a2a_bearer_tokens", &self.a2a_bearer_tokens.len())
            .field("public_url", &self.public_url.as_str())
            .field("listen_addr", &self.listen_addr)
            .field("workers", &self.workers)
            .field("max_check_cycles", &self.max_check_cycles)
            .field("check_timeout", &self.check_timeout)
            .field("check_output_tail", &self.check_output_tail)
            .field("pr_draft", &self.pr_draft)
            .field("opencode_command", &self.opencode_command)
            .finish_non_exhaustive()
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

    /// Read variables through `lookup` (`None` = unset). Blank values count as
    /// unset, except `MODEL_API_KEY`, which may be empty.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] listing every missing or malformed variable.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let mut problems = Vec::new();
        let get = |name: &str| lookup(name).filter(|v| !v.trim().is_empty());
        let mut required = |name: &str| {
            let value = get(name);
            if value.is_none() {
                problems.push(format!("{name} is required"));
            }
            value.unwrap_or_default()
        };

        let database_url = required("DATABASE_URL");
        let model_base_url = required("MODEL_BASE_URL");
        let model = required("MODEL");
        let github_token = required("GITHUB_TOKEN");
        let public_url_raw = required("PUBLIC_URL");
        let tokens_raw = required("A2A_BEARER_TOKENS");
        let model_api_key = match lookup("MODEL_API_KEY") {
            Some(v) => v,
            None => {
                problems.push(
                    "MODEL_API_KEY is required (set it empty for a gateway without auth)".into(),
                );
                String::new()
            }
        };

        let opencode_model = get("OPENCODE_MODEL").unwrap_or_else(|| model.clone());
        let workspace_root =
            PathBuf::from(get("WORKSPACE_ROOT").unwrap_or_else(|| "/work".to_owned()));

        let public_url = match Url::parse(&public_url_raw) {
            Ok(u) if matches!(u.scheme(), "http" | "https") => Some(u),
            Ok(_) => {
                problems.push("PUBLIC_URL must be an http(s) URL".into());
                None
            }
            Err(e) if !public_url_raw.is_empty() => {
                problems.push(format!("PUBLIC_URL is not a URL: {e}"));
                None
            }
            Err(_) => None,
        };

        let a2a_bearer_tokens: Vec<SecretString> = tokens_raw
            .split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(|t| SecretString::from(t.to_owned()))
            .collect();
        if a2a_bearer_tokens.is_empty() && !tokens_raw.is_empty() {
            problems.push("A2A_BEARER_TOKENS has no usable token".into());
        }

        let listen_addr = parse_or(
            &get,
            "LISTEN_ADDR",
            SocketAddr::from(([0, 0, 0, 0], 8080)),
            &mut problems,
        );
        let workers = parse_or(&get, "WORKERS", 4usize, &mut problems);
        if workers == 0 {
            problems.push("WORKERS must be at least 1".into());
        }
        let max_check_cycles = parse_or(&get, "MAX_CHECK_CYCLES", 3u32, &mut problems);
        if max_check_cycles == 0 {
            problems.push("MAX_CHECK_CYCLES must be at least 1".into());
        }
        let check_timeout =
            Duration::from_secs(parse_or(&get, "CHECK_TIMEOUT_SECS", 900u64, &mut problems).max(1));
        let check_output_tail =
            parse_or(&get, "CHECK_OUTPUT_TAIL_BYTES", 16_384usize, &mut problems).max(256);
        let mut flag = |name: &str| match get(name).as_deref() {
            None => false,
            Some(v) if v.eq_ignore_ascii_case("true") || v == "1" => true,
            Some(v) if v.eq_ignore_ascii_case("false") || v == "0" => false,
            Some(v) => {
                problems.push(format!("{name} must be true or false, got {v:?}"));
                false
            }
        };
        let pr_draft = flag("PR_DRAFT");
        let allow_local_repos = flag("ALLOW_LOCAL_REPOS");
        let allowed_repo_hosts = match get("ALLOWED_REPO_HOSTS") {
            None => vec![DEFAULT_REPO_HOST.to_owned()],
            Some(raw) => {
                let hosts: Vec<String> = raw
                    .split(',')
                    .map(|h| h.trim().to_ascii_lowercase())
                    .filter(|h| !h.is_empty())
                    .collect();
                if hosts.is_empty() {
                    problems.push("ALLOWED_REPO_HOSTS has no usable host".into());
                }
                for host in &hosts {
                    if !is_host_entry(host) {
                        problems.push(format!(
                            "ALLOWED_REPO_HOSTS entry {host:?} is not a host name (use `github.com` or `host:port`, no scheme or path)"
                        ));
                    }
                }
                hosts
            }
        };
        let github_api_url = match get("GITHUB_API_URL") {
            None => Url::parse(DEFAULT_GITHUB_API_URL).ok(),
            Some(raw) => match Url::parse(raw.trim()) {
                Ok(u) if matches!(u.scheme(), "http" | "https") && u.host().is_some() => Some(u),
                Ok(_) => {
                    problems.push("GITHUB_API_URL must be an http(s) URL".into());
                    None
                }
                Err(e) => {
                    problems.push(format!("GITHUB_API_URL is not a URL: {e}"));
                    None
                }
            },
        };
        let opencode_command: Vec<String> = get("OPENCODE_COMMAND")
            .unwrap_or_else(|| "opencode acp".to_owned())
            .split_whitespace()
            .map(str::to_owned)
            .collect();

        let (Some(public_url), Some(github_api_url)) = (public_url, github_api_url) else {
            return Err(ConfigError { problems });
        };
        if !problems.is_empty() {
            return Err(ConfigError { problems });
        }
        Ok(Self {
            database_url: SecretString::from(database_url),
            model_base_url,
            model_api_key: SecretString::from(model_api_key),
            model,
            opencode_model,
            github_token: SecretString::from(github_token),
            allowed_repo_hosts,
            allow_local_repos,
            github_api_url,
            workspace_root,
            a2a_bearer_tokens,
            public_url,
            listen_addr,
            workers,
            max_check_cycles,
            check_timeout,
            check_output_tail,
            git_author_name: get("GIT_AUTHOR_NAME").unwrap_or_else(|| "adam-coder".to_owned()),
            git_author_email: get("GIT_AUTHOR_EMAIL")
                .unwrap_or_else(|| "adam-coder@users.noreply.github.com".to_owned()),
            pr_draft,
            opencode_command,
        })
    }
}

/// Repository host used when `ALLOWED_REPO_HOSTS` is unset.
const DEFAULT_REPO_HOST: &str = "github.com";
/// API root used when `GITHUB_API_URL` is unset.
const DEFAULT_GITHUB_API_URL: &str = "https://api.github.com";

/// `name` or `name:port`: letters, digits, dots and dashes, nothing that could
/// smuggle a scheme, path, userinfo or wildcard into an allowlist.
fn is_host_entry(entry: &str) -> bool {
    let (name, port) = match entry.split_once(':') {
        Some((name, port)) => (name, Some(port)),
        None => (entry, None),
    };
    !name.is_empty()
        && !name.starts_with(['.', '-'])
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
        && port.is_none_or(|p| !p.is_empty() && p.parse::<u16>().is_ok())
}

fn parse_or<T: std::str::FromStr>(
    get: &impl Fn(&str) -> Option<String>,
    name: &str,
    default: T,
    problems: &mut Vec<String>,
) -> T
where
    T::Err: std::fmt::Display,
{
    match get(name) {
        None => default,
        Some(raw) => raw.trim().parse().unwrap_or_else(|e| {
            problems.push(format!("{name} is invalid ({raw:?}): {e}"));
            default
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use secrecy::ExposeSecret;

    use super::*;

    fn full() -> HashMap<&'static str, &'static str> {
        HashMap::from([
            ("DATABASE_URL", "postgres://u:hunter2@db/adam"),
            ("MODEL_BASE_URL", "https://gw.example/v1"),
            ("MODEL_API_KEY", "sk-secret"),
            ("MODEL", "coder-large"),
            ("GITHUB_TOKEN", "ghp_secret"),
            ("A2A_BEARER_TOKENS", "one, two ,,"),
            ("PUBLIC_URL", "http://coder.svc:8080/"),
        ])
    }

    fn parse(vars: &HashMap<&'static str, &'static str>) -> Result<Config, ConfigError> {
        Config::from_lookup(|k| vars.get(k).map(|v| (*v).to_owned()))
    }

    #[test]
    fn defaults_apply_and_tokens_are_split() {
        let c = parse(&full()).expect("valid");
        assert_eq!(c.workspace_root, PathBuf::from("/work"));
        assert_eq!(c.listen_addr, "0.0.0.0:8080".parse().unwrap());
        assert_eq!(c.workers, 4);
        assert_eq!(c.max_check_cycles, 3);
        assert_eq!(c.opencode_model, "coder-large");
        assert_eq!(c.opencode_command, ["opencode", "acp"]);
        assert_eq!(c.allowed_repo_hosts, ["github.com"]);
        assert!(!c.allow_local_repos, "local repositories are opt-in");
        assert_eq!(c.github_api_url.as_str(), "https://api.github.com/");
        let tokens: Vec<_> = c
            .a2a_bearer_tokens
            .iter()
            .map(|t| t.expose_secret().to_owned())
            .collect();
        assert_eq!(tokens, ["one", "two"]);
        assert!(!c.pr_draft);
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let err = parse(&HashMap::new()).unwrap_err();
        for name in [
            "DATABASE_URL",
            "MODEL_BASE_URL",
            "MODEL_API_KEY",
            "MODEL",
            "GITHUB_TOKEN",
            "PUBLIC_URL",
            "A2A_BEARER_TOKENS",
        ] {
            assert!(
                err.problems.iter().any(|p| p.starts_with(name)),
                "{name} missing from {:?}",
                err.problems
            );
        }
    }

    #[test]
    fn fail_closed_on_tokens_and_validate_numbers() {
        let mut vars = full();
        vars.insert("A2A_BEARER_TOKENS", " , ");
        assert!(
            parse(&vars).is_err(),
            "no usable token must not start the server"
        );

        let mut vars = full();
        vars.insert("WORKERS", "0");
        vars.insert("MAX_CHECK_CYCLES", "many");
        vars.insert("PR_DRAFT", "maybe");
        vars.insert("PUBLIC_URL", "ftp://x");
        let err = parse(&vars).unwrap_err();
        assert!(err.problems.len() >= 4, "{:?}", err.problems);
    }

    #[test]
    fn an_empty_api_key_is_allowed_but_an_unset_one_is_not() {
        let mut vars = full();
        vars.insert("MODEL_API_KEY", "");
        assert!(parse(&vars).is_ok());
        vars.remove("MODEL_API_KEY");
        assert!(parse(&vars).is_err());
    }

    #[test]
    fn debug_output_hides_secrets() {
        let c = parse(&full()).unwrap();
        let text = format!("{c:?}");
        for secret in ["hunter2", "sk-secret", "ghp_secret", "one", "two"] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
    }

    #[test]
    fn repository_hosts_are_normalised_and_validated() {
        let mut vars = full();
        vars.insert("ALLOWED_REPO_HOSTS", " GitHub.com, ghe.example.com:8443 ,,");
        let c = parse(&vars).expect("valid");
        assert_eq!(c.allowed_repo_hosts, ["github.com", "ghe.example.com:8443"]);

        for bad in [
            "https://github.com",
            "github.com/o/r",
            "*.github.com",
            "user@github.com",
            "github.com:",
            "github.com:notaport",
            "-x.com",
            " , ",
        ] {
            let mut vars = full();
            vars.insert("ALLOWED_REPO_HOSTS", bad);
            let err = parse(&vars).unwrap_err();
            assert!(
                err.problems
                    .iter()
                    .any(|p| p.starts_with("ALLOWED_REPO_HOSTS")),
                "{bad:?} accepted or misreported: {:?}",
                err.problems
            );
        }
    }

    #[test]
    fn github_api_url_and_local_repos_are_configurable_and_checked() {
        let mut vars = full();
        vars.insert("GITHUB_API_URL", "http://127.0.0.1:9999/api/v3");
        vars.insert("ALLOW_LOCAL_REPOS", "true");
        let c = parse(&vars).expect("valid");
        assert_eq!(c.github_api_url.as_str(), "http://127.0.0.1:9999/api/v3");
        assert!(c.allow_local_repos);

        let mut vars = full();
        vars.insert("GITHUB_API_URL", "ftp://api.example");
        vars.insert("ALLOW_LOCAL_REPOS", "yes please");
        let err = parse(&vars).unwrap_err();
        for name in ["GITHUB_API_URL", "ALLOW_LOCAL_REPOS"] {
            assert!(
                err.problems.iter().any(|p| p.starts_with(name)),
                "{name} missing from {:?}",
                err.problems
            );
        }
        let mut vars = full();
        vars.insert("GITHUB_API_URL", "not a url");
        assert!(parse(&vars).is_err());
    }
}
