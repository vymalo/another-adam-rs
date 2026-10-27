//! Configuration of the `adam-git-sandbox` server, read from the environment.
//!
//! | Variable | Meaning | Default |
//! |---|---|---|
//! | `GIT_SANDBOX_LISTEN` | the Unix socket to listen on: an absolute path, made with mode `0600` | required |
//! | `GIT_SANDBOX_ROOT` | the work volume: an absolute path of an existing directory; commands run in a directory below it and nowhere else | required |
//! | `GIT_SANDBOX_GIT` | the `git` to run: an absolute path of an executable file | the first `git` on `PATH`, found once at startup |
//! | `GIT_SANDBOX_MAX_COMMANDS` | commands run at once (1 to 4096); the next waits | `64` |
//! | `RUST_LOG` | log filter; JSON logs on stdout | `info` |
//!
//! Every problem is reported at once.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use adam_error::{Classify, ErrorClass};

/// The variable the coder and the relay read to find the server's socket.
pub const SOCKET_ENV: &str = "GIT_SANDBOX_SOCKET";

/// Commands run at once, when `GIT_SANDBOX_MAX_COMMANDS` is unset.
pub const DEFAULT_MAX_COMMANDS: usize = 64;

/// Why the environment does not describe a server: every problem at once, never a secret (there is
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

/// The server's configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// `GIT_SANDBOX_LISTEN`.
    pub listen: PathBuf,
    /// `GIT_SANDBOX_ROOT`.
    pub root: PathBuf,
    /// `GIT_SANDBOX_GIT`, or the `git` found on `PATH`: the one program the server runs.
    pub git: PathBuf,
    /// `GIT_SANDBOX_MAX_COMMANDS`.
    pub max_commands: usize,
}

impl Config {
    /// Read the process environment.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] listing every problem.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| std::env::var_os(name))
    }

    /// Read variables through `lookup` (`None` = unset; a blank value counts as unset).
    ///
    /// # Errors
    ///
    /// [`ConfigError`] listing every problem.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<OsString>) -> Result<Self, ConfigError> {
        let get = |name: &str| lookup(name).filter(|v| !v.to_string_lossy().trim().is_empty());
        let mut problems = Vec::new();

        let absolute = |name: &str, problems: &mut Vec<String>| -> Option<PathBuf> {
            let Some(raw) = get(name) else {
                problems.push(format!("{name} is required"));
                return None;
            };
            let path = PathBuf::from(raw);
            if path.is_absolute() {
                Some(path)
            } else {
                problems.push(format!("{name} must be an absolute path"));
                None
            }
        };

        let listen = absolute("GIT_SANDBOX_LISTEN", &mut problems);
        let root = absolute("GIT_SANDBOX_ROOT", &mut problems);
        if let Some(root) = &root
            && !root.is_dir()
        {
            problems.push(format!(
                "GIT_SANDBOX_ROOT {} is not an existing directory",
                root.display()
            ));
        }

        let git = match get("GIT_SANDBOX_GIT") {
            Some(raw) => {
                let path = PathBuf::from(raw);
                if !path.is_absolute() {
                    problems.push("GIT_SANDBOX_GIT must be an absolute path".to_owned());
                    None
                } else if !is_executable_file(&path) {
                    problems.push(format!(
                        "GIT_SANDBOX_GIT {} is not an executable file",
                        path.display()
                    ));
                    None
                } else {
                    Some(path)
                }
            }
            None => {
                let found = find_on_path(lookup("PATH").as_deref());
                if found.is_none() {
                    problems.push("no git on PATH, and GIT_SANDBOX_GIT is not set".to_owned());
                }
                found
            }
        };

        let max_commands = match get("GIT_SANDBOX_MAX_COMMANDS") {
            None => DEFAULT_MAX_COMMANDS,
            Some(raw) => match raw.to_string_lossy().trim().parse::<usize>() {
                Ok(n) if (1..=4096).contains(&n) => n,
                _ => {
                    problems.push(format!(
                        "GIT_SANDBOX_MAX_COMMANDS {:?} is not a whole number from 1 to 4096",
                        raw.to_string_lossy()
                    ));
                    DEFAULT_MAX_COMMANDS
                }
            },
        };

        match (listen, root, git) {
            (Some(listen), Some(root), Some(git)) if problems.is_empty() => Ok(Self {
                listen,
                root,
                git,
                max_commands,
            }),
            _ => Err(ConfigError { problems }),
        }
    }
}

/// The socket a client is to use: `GIT_SANDBOX_SOCKET`, unset or blank for none.
///
/// # Errors
///
/// A message when the value is not an absolute path.
pub fn socket_from_env(value: Option<OsString>) -> Result<Option<PathBuf>, String> {
    let Some(value) = value.filter(|v| !v.to_string_lossy().trim().is_empty()) else {
        return Ok(None);
    };
    let path = PathBuf::from(value);
    if path.is_absolute() {
        Ok(Some(path))
    } else {
        Err(format!("{SOCKET_ENV} must be an absolute path"))
    }
}

fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// The first `git` in the directories of `path` that is an executable file.
fn find_on_path(path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path?)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("git"))
        .find(|candidate| is_executable_file(candidate))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn read(vars: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let map: HashMap<_, _> = vars.iter().copied().collect();
        Config::from_lookup(|name| map.get(name).map(OsString::from))
    }

    fn good(tmp: &tempfile::TempDir) -> Vec<(&'static str, String)> {
        vec![
            ("GIT_SANDBOX_LISTEN", "/run/git/git.sock".to_owned()),
            ("GIT_SANDBOX_ROOT", tmp.path().display().to_string()),
            ("PATH", std::env::var("PATH").unwrap_or_default()),
        ]
    }

    fn as_refs<'a>(v: &'a [(&'static str, String)]) -> Vec<(&'a str, &'a str)> {
        v.iter().map(|(k, s)| (*k, s.as_str())).collect()
    }

    #[test]
    fn a_minimal_environment_is_enough() {
        let tmp = tempfile::tempdir().unwrap();
        let vars = good(&tmp);
        let config = read(&as_refs(&vars)).unwrap();
        assert_eq!(config.listen, Path::new("/run/git/git.sock"));
        assert_eq!(config.max_commands, DEFAULT_MAX_COMMANDS);
        assert!(
            config.git.is_absolute() && config.git.ends_with("git"),
            "{:?}",
            config.git
        );
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let error = read(&[("PATH", "")]).unwrap_err();
        let text = error.to_string();
        assert!(text.starts_with("invalid configuration:"), "{text}");
        for want in [
            "GIT_SANDBOX_LISTEN is required",
            "GIT_SANDBOX_ROOT is required",
            "no git on PATH",
        ] {
            assert!(text.contains(want), "{want} in {text}");
        }
        assert_eq!(error.class(), ErrorClass::Invalid);
    }

    #[test]
    fn paths_must_be_absolute_and_the_root_must_exist() {
        let error = read(&[
            ("GIT_SANDBOX_LISTEN", "git.sock"),
            ("GIT_SANDBOX_ROOT", "work"),
            ("GIT_SANDBOX_GIT", "git"),
        ])
        .unwrap_err()
        .to_string();
        for want in [
            "GIT_SANDBOX_LISTEN must be an absolute path",
            "GIT_SANDBOX_ROOT must be an absolute path",
            "GIT_SANDBOX_GIT must be an absolute path",
        ] {
            assert!(error.contains(want), "{want} in {error}");
        }
        let error = read(&[
            ("GIT_SANDBOX_LISTEN", "/s"),
            ("GIT_SANDBOX_ROOT", "/nowhere/at/all"),
            ("GIT_SANDBOX_GIT", "/nowhere/git"),
        ])
        .unwrap_err()
        .to_string();
        assert!(error.contains("is not an existing directory"), "{error}");
        assert!(error.contains("is not an executable file"), "{error}");
    }

    #[test]
    fn the_git_is_the_one_named_or_the_first_on_the_path() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join("git");
        std::fs::write(&fake, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let root = tmp.path().display().to_string();
        let named = read(&[
            ("GIT_SANDBOX_LISTEN", "/s"),
            ("GIT_SANDBOX_ROOT", &root),
            ("GIT_SANDBOX_GIT", fake.to_str().unwrap()),
        ])
        .unwrap();
        assert_eq!(named.git, fake);
        let found = read(&[
            ("GIT_SANDBOX_LISTEN", "/s"),
            ("GIT_SANDBOX_ROOT", &root),
            ("PATH", &format!("relative:{root}")),
        ])
        .unwrap();
        assert_eq!(
            found.git, fake,
            "a relative directory of PATH is not searched"
        );
        // Not executable: not a git.
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            read(&[
                ("GIT_SANDBOX_LISTEN", "/s"),
                ("GIT_SANDBOX_ROOT", &root),
                ("PATH", &root),
            ])
            .is_err()
        );
    }

    #[test]
    fn the_command_limit_is_bounded() {
        let tmp = tempfile::tempdir().unwrap();
        let vars = good(&tmp);
        for ok in ["1", "64", "4096"] {
            let mut v = as_refs(&vars);
            v.push(("GIT_SANDBOX_MAX_COMMANDS", ok));
            assert_eq!(read(&v).unwrap().max_commands, ok.parse::<usize>().unwrap());
        }
        for bad in ["0", "4097", "-1", "many"] {
            let mut v = as_refs(&vars);
            v.push(("GIT_SANDBOX_MAX_COMMANDS", bad));
            let error = read(&v).unwrap_err().to_string();
            assert!(error.contains("GIT_SANDBOX_MAX_COMMANDS"), "{bad}: {error}");
        }
    }

    #[test]
    fn the_clients_socket_is_optional_and_absolute() {
        assert_eq!(socket_from_env(None).unwrap(), None);
        assert_eq!(socket_from_env(Some(OsString::from("  "))).unwrap(), None);
        assert_eq!(
            socket_from_env(Some(OsString::from("/run/git/git.sock"))).unwrap(),
            Some(PathBuf::from("/run/git/git.sock"))
        );
        assert!(socket_from_env(Some(OsString::from("git.sock"))).is_err());
    }
}
