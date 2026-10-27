//! Configuration of the `adam-egress` binary, read from the environment.
//!
//! | Variable | Meaning | Default |
//! |---|---|---|
//! | `EGRESS_LISTEN` | the address the proxy listens on | `0.0.0.0:3128` |
//! | `EGRESS_ALLOW_FILE` | a file of allowed hosts, one per line; `#` starts a comment; blank lines are ignored; must be a readable file | unset |
//! | `EGRESS_ALLOW` | allowed hosts, comma-separated; added to the file's | unset |
//! | `EGRESS_PORTS` | the ports `CONNECT` may name, comma-separated | `443` |
//! | `EGRESS_DENY_CIDRS` | IPv4 ranges (`10.96.0.0/12`) that are never connected to, on top of the built-in ones: the cluster's own | unset |
//! | `EGRESS_IDLE_SECS` | a tunnel that moves no byte for this long is closed (1 to 86400) | `300` |
//! | `EGRESS_MAX_CONNECTIONS` | connections served at once; the next gets `503` (1 to 1000000) | `1024` |
//! | `RUST_LOG` | log filter; JSON logs on stdout | `info` |
//!
//! At least one host must be allowed: an empty allow-list would be a proxy that refuses
//! everything, which is a mistake to hear about at startup. Every problem is reported at once.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use adam_error::{Classify, ErrorClass};

use crate::allow::AllowList;
use crate::ip::Cidr;

/// The address the proxy listens on, when `EGRESS_LISTEN` is unset.
pub const DEFAULT_LISTEN: &str = "0.0.0.0:3128";

/// Why the environment does not describe a proxy: every problem at once, never a secret (there is
/// none to name). Classified as [`ErrorClass::Invalid`]: the same environment never works.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
#[error("invalid configuration:\n  - {}", problems.join("\n  - "))]
pub struct ConfigError {
    /// One line per problem.
    pub problems: Vec<String>,
}

impl Classify for ConfigError {
    fn class(&self) -> ErrorClass {
        ErrorClass::Invalid
    }
}

/// The limits that are not variables: fixed numbers of the proxy, a field here so that a test can
/// make them short.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The most bytes a request head may have (8 KiB).
    pub max_head: usize,
    /// How long the head may take to arrive, whole (10 s).
    pub head_timeout: Duration,
    /// How long resolving the name may take, and how long one connection attempt may (10 s each).
    pub connect_timeout: Duration,
    /// How many of the kept addresses are tried, in order (4).
    pub max_attempts: usize,
    /// How long a SIGTERM waits for the tunnels to finish before the process ends (25 s).
    pub shutdown_grace: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_head: 8 * 1024,
            head_timeout: Duration::from_secs(10),
            connect_timeout: Duration::from_secs(10),
            max_attempts: 4,
            shutdown_grace: Duration::from_secs(25),
        }
    }
}

/// The binary's configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// `EGRESS_LISTEN`.
    pub listen: SocketAddr,
    /// `EGRESS_ALLOW_FILE` and `EGRESS_ALLOW`: never empty when read from the environment.
    pub allow: AllowList,
    /// `EGRESS_PORTS`.
    pub ports: BTreeSet<u16>,
    /// `EGRESS_DENY_CIDRS`.
    pub deny_cidrs: Vec<Cidr>,
    /// `EGRESS_IDLE_SECS`.
    pub idle: Duration,
    /// `EGRESS_MAX_CONNECTIONS`.
    pub max_connections: usize,
    /// The fixed limits.
    pub limits: Limits,
}

impl Config {
    /// Read the process environment.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] listing every problem.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Read variables through `lookup` (`None` = unset; a blank value counts as unset). The file
    /// `EGRESS_ALLOW_FILE` names is read from the file system.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] listing every problem.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let get = |name: &str| lookup(name).filter(|v| !v.trim().is_empty());
        let mut problems = Vec::new();

        let listen = match get("EGRESS_LISTEN") {
            None => DEFAULT_LISTEN.parse().ok(),
            Some(raw) => raw.trim().parse().ok().or_else(|| {
                problems.push(format!(
                    "EGRESS_LISTEN {raw:?} is not an address and port such as {DEFAULT_LISTEN}"
                ));
                None
            }),
        };

        let mut allow = AllowList::new();
        if let Some(file) = get("EGRESS_ALLOW_FILE") {
            let path = PathBuf::from(file.trim());
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    for (n, line) in text.lines().enumerate() {
                        let entry = line.split('#').next().unwrap_or_default().trim();
                        if entry.is_empty() {
                            continue;
                        }
                        if let Err(e) = allow.add(entry) {
                            problems.push(format!(
                                "EGRESS_ALLOW_FILE {} line {}: {e}",
                                path.display(),
                                n + 1
                            ));
                        }
                    }
                }
                Err(e) => problems.push(format!(
                    "EGRESS_ALLOW_FILE {} cannot be read: {e}",
                    path.display()
                )),
            }
        }
        if let Some(list) = get("EGRESS_ALLOW") {
            for entry in list.split(',').map(str::trim).filter(|e| !e.is_empty()) {
                if let Err(e) = allow.add(entry) {
                    problems.push(format!("EGRESS_ALLOW: {e}"));
                }
            }
        }
        if allow.is_empty() && problems.iter().all(|p| !p.starts_with("EGRESS_ALLOW")) {
            problems.push(
                "no host is allowed: set EGRESS_ALLOW (comma-separated) or EGRESS_ALLOW_FILE \
                 (one per line); `example.com` allows one host and `.example.com` its subdomains"
                    .to_owned(),
            );
        }

        let mut ports = BTreeSet::new();
        match get("EGRESS_PORTS") {
            None => {
                ports.insert(443);
            }
            Some(list) => {
                for item in list.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                    match item.parse::<u16>() {
                        Ok(port) if port != 0 => {
                            ports.insert(port);
                        }
                        _ => problems
                            .push(format!("EGRESS_PORTS {item:?} is not a port (1 to 65535)")),
                    }
                }
                if ports.is_empty() && problems.iter().all(|p| !p.starts_with("EGRESS_PORTS")) {
                    problems.push("EGRESS_PORTS names no port".to_owned());
                }
            }
        }

        let mut deny_cidrs = Vec::new();
        if let Some(list) = get("EGRESS_DENY_CIDRS") {
            for item in list.split(',').map(str::trim).filter(|c| !c.is_empty()) {
                match Cidr::parse(item) {
                    Ok(cidr) => deny_cidrs.extend(cidr),
                    Err(e) => problems.push(format!("EGRESS_DENY_CIDRS: {e}")),
                }
            }
        }

        let idle = number(&get, "EGRESS_IDLE_SECS", 300, 1, 86_400, &mut problems);
        let max_connections = number(
            &get,
            "EGRESS_MAX_CONNECTIONS",
            1024,
            1,
            1_000_000,
            &mut problems,
        );

        match (problems.is_empty(), listen) {
            (true, Some(listen)) => Ok(Self {
                listen,
                allow,
                ports,
                deny_cidrs,
                idle: Duration::from_secs(idle),
                max_connections: usize::try_from(max_connections).unwrap_or(1024),
                limits: Limits::default(),
            }),
            _ => Err(ConfigError { problems }),
        }
    }
}

/// The whole number in `name` from `min` to `max`, or `default` when it is unset. A value that is
/// no such number adds a problem and gives `default`.
fn number(
    get: &impl Fn(&str) -> Option<String>,
    name: &str,
    default: u64,
    min: u64,
    max: u64,
    problems: &mut Vec<String>,
) -> u64 {
    match get(name) {
        None => default,
        Some(raw) => match raw.trim().parse::<u64>() {
            Ok(n) if (min..=max).contains(&n) => n,
            _ => {
                problems.push(format!(
                    "{name} {raw:?} is not a whole number from {min} to {max}"
                ));
                default
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn read(vars: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let vars: HashMap<_, _> = vars.iter().copied().collect();
        Config::from_lookup(|name| vars.get(name).map(|v| (*v).to_owned()))
    }

    fn problems(vars: &[(&str, &str)]) -> Vec<String> {
        read(vars).unwrap_err().problems
    }

    #[test]
    fn the_defaults_are_the_documented_ones() {
        let c = read(&[("EGRESS_ALLOW", "example.com")]).unwrap();
        assert_eq!(c.listen, "0.0.0.0:3128".parse().unwrap());
        assert_eq!(c.ports, BTreeSet::from([443]));
        assert!(c.deny_cidrs.is_empty());
        assert_eq!(c.idle, Duration::from_secs(300));
        assert_eq!(c.max_connections, 1024);
        assert_eq!(c.limits, Limits::default());
        assert_eq!(c.limits.max_head, 8192);
        assert_eq!(c.limits.head_timeout, Duration::from_secs(10));
        assert_eq!(c.limits.connect_timeout, Duration::from_secs(10));
        assert_eq!(c.limits.shutdown_grace, Duration::from_secs(25));
        assert_eq!(c.allow.entries().collect::<Vec<_>>(), ["example.com"]);
    }

    #[test]
    fn every_variable_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("allow.txt");
        std::fs::write(
            &file,
            "# package registries\nregistry.npmjs.org\n\n  .pythonhosted.org  # files\n",
        )
        .unwrap();
        let c = read(&[
            ("EGRESS_LISTEN", "127.0.0.1:8888"),
            ("EGRESS_ALLOW_FILE", file.to_str().unwrap()),
            ("EGRESS_ALLOW", "github.com, .githubusercontent.com ,"),
            ("EGRESS_PORTS", "443, 8443"),
            ("EGRESS_DENY_CIDRS", "10.96.0.0/12, 10.244.0.0/16,fd00::/8"),
            ("EGRESS_IDLE_SECS", "60"),
            ("EGRESS_MAX_CONNECTIONS", "7"),
        ])
        .unwrap();
        assert_eq!(c.listen, "127.0.0.1:8888".parse().unwrap());
        assert_eq!(c.ports, BTreeSet::from([443, 8443]));
        assert_eq!(c.deny_cidrs.len(), 2, "the IPv6 range has nothing to add");
        assert_eq!(c.idle, Duration::from_secs(60));
        assert_eq!(c.max_connections, 7);
        assert_eq!(
            c.allow.entries().collect::<Vec<_>>(),
            [
                "github.com",
                "registry.npmjs.org",
                ".githubusercontent.com",
                ".pythonhosted.org"
            ]
        );
    }

    #[test]
    fn blank_values_are_unset() {
        let c = read(&[
            ("EGRESS_ALLOW", "example.com"),
            ("EGRESS_LISTEN", " "),
            ("EGRESS_PORTS", ""),
            ("EGRESS_IDLE_SECS", " "),
            ("EGRESS_ALLOW_FILE", ""),
        ])
        .unwrap();
        assert_eq!(c.listen, "0.0.0.0:3128".parse().unwrap());
        assert_eq!(c.ports, BTreeSet::from([443]));
    }

    #[test]
    fn no_allow_list_is_a_problem_that_says_how_to_make_one() {
        for vars in [&[][..], &[("EGRESS_ALLOW", " , ")], &[("EGRESS_ALLOW", "")]] {
            let p = problems(vars);
            assert_eq!(p.len(), 1, "{p:?}");
            assert!(p[0].contains("no host is allowed"), "{p:?}");
            assert!(p[0].contains("EGRESS_ALLOW"), "{p:?}");
        }
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("empty.txt");
        std::fs::write(&file, "# nothing\n\n").unwrap();
        let p = problems(&[("EGRESS_ALLOW_FILE", file.to_str().unwrap())]);
        assert!(p[0].contains("no host is allowed"), "{p:?}");
    }

    #[test]
    fn an_entry_that_is_no_name_is_a_problem_that_names_it() {
        let p = problems(&[("EGRESS_ALLOW", "good.example, *.bad.example,10.0.0.1")]);
        assert_eq!(p.len(), 2, "{p:?}");
        assert!(
            p[0].starts_with("EGRESS_ALLOW:") && p[0].contains("\"*.bad.example\""),
            "{p:?}"
        );
        assert!(p[1].contains("\"10.0.0.1\""), "{p:?}");

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("allow.txt");
        std::fs::write(&file, "fine.example\nhttps://nope.example\n").unwrap();
        let p = problems(&[("EGRESS_ALLOW_FILE", file.to_str().unwrap())]);
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(
            p[0].contains("line 2") && p[0].contains("\"https://nope.example\""),
            "{p:?}"
        );
        assert!(p[0].contains("allow.txt"), "{p:?}");
    }

    #[test]
    fn a_file_that_cannot_be_read_is_a_problem() {
        let p = problems(&[
            ("EGRESS_ALLOW_FILE", "/nonexistent/allow.txt"),
            ("EGRESS_ALLOW", "example.com"),
        ]);
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(
            p[0].contains("/nonexistent/allow.txt") && p[0].contains("cannot be read"),
            "{p:?}"
        );
        let dir = tempfile::tempdir().unwrap();
        let p = problems(&[
            ("EGRESS_ALLOW_FILE", dir.path().to_str().unwrap()),
            ("EGRESS_ALLOW", "example.com"),
        ]);
        assert!(
            p[0].contains("cannot be read"),
            "a directory is no file: {p:?}"
        );
    }

    #[test]
    fn each_bad_value_is_a_problem_that_names_its_variable() {
        let ok = ("EGRESS_ALLOW", "example.com");
        for (var, bad) in [
            ("EGRESS_LISTEN", "3128"),
            ("EGRESS_LISTEN", "localhost:3128"),
            ("EGRESS_LISTEN", "0.0.0.0:99999"),
            ("EGRESS_PORTS", "https"),
            ("EGRESS_PORTS", "0"),
            ("EGRESS_PORTS", "443,70000"),
            ("EGRESS_PORTS", "-1"),
            ("EGRESS_DENY_CIDRS", "10.0.0.0/33"),
            ("EGRESS_DENY_CIDRS", "10.0.0.0/8,banana"),
            ("EGRESS_IDLE_SECS", "0"),
            ("EGRESS_IDLE_SECS", "forever"),
            ("EGRESS_IDLE_SECS", "86401"),
            ("EGRESS_IDLE_SECS", "-5"),
            ("EGRESS_MAX_CONNECTIONS", "0"),
            ("EGRESS_MAX_CONNECTIONS", "1000001"),
            ("EGRESS_MAX_CONNECTIONS", "many"),
        ] {
            let p = problems(&[ok, (var, bad)]);
            assert_eq!(p.len(), 1, "{var}={bad}: {p:?}");
            assert!(p[0].contains(var), "{var}={bad}: {p:?}");
        }
        let p = problems(&[ok, ("EGRESS_PORTS", ",")]);
        assert_eq!(p, ["EGRESS_PORTS names no port"]);
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let e = read(&[
            ("EGRESS_LISTEN", "nope"),
            ("EGRESS_ALLOW", "*.x"),
            ("EGRESS_PORTS", "x"),
            ("EGRESS_DENY_CIDRS", "y"),
            ("EGRESS_IDLE_SECS", "z"),
            ("EGRESS_MAX_CONNECTIONS", "w"),
        ])
        .unwrap_err();
        // The allow-list entry is the one problem of the list: no second "no host is allowed".
        assert_eq!(e.problems.len(), 6, "{e}");
        assert!(
            e.to_string().starts_with("invalid configuration:\n  - "),
            "{e}"
        );
        assert_eq!(e.class(), ErrorClass::Invalid);
    }
}
