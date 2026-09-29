//! How to start the agent process.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::error::AcpError;

/// The agent process to spawn: `program args...` in `cwd`, with `env` added on
/// top of the parent's environment (nothing is cleared: agents need `HOME`,
/// `PATH`, proxy settings, ...).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpCommand {
    /// The program. A bare name is looked up on `PATH` (a `PATH` entry in
    /// [`env`](Self::env) wins); anything with a directory part is taken
    /// relative to the *parent's* current directory, not to `cwd`.
    pub program: PathBuf,
    /// Arguments.
    pub args: Vec<String>,
    /// Extra environment variables.
    pub env: BTreeMap<String, String>,
    /// Working directory of the child.
    pub cwd: PathBuf,
}

impl AcpCommand {
    /// A command with no arguments and no extra environment.
    pub fn new(program: impl Into<PathBuf>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: cwd.into(),
        }
    }

    /// `opencode acp` in `cwd`, with OpenCode's self-updater disabled.
    ///
    /// OpenCode picks its own provider and model from its config; use
    /// [`with_config_content`](Self::with_config_content) to inject some.
    pub fn opencode(cwd: impl Into<PathBuf>) -> Self {
        let mut cmd = Self::new("opencode", cwd);
        cmd.args.push("acp".to_owned());
        cmd.env
            .insert("OPENCODE_DISABLE_AUTOUPDATE".to_owned(), "1".to_owned());
        cmd
    }

    /// Append one argument.
    #[must_use]
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Set one environment variable (replacing an earlier value).
    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Set `OPENCODE_CONFIG_CONTENT`, OpenCode's inline JSON configuration
    /// (models, providers, permissions). It ranks above the user's global and
    /// the project's `opencode.json`.
    #[must_use]
    pub fn with_config_content(self, json: impl Into<String>) -> Self {
        self.env("OPENCODE_CONFIG_CONTENT", json)
    }

    /// The program as an absolute path.
    ///
    /// Resolving up front avoids the classic trap where a relative program
    /// plus a different `current_dir` fails with `ENOENT`.
    pub(crate) fn resolve_program(&self) -> Result<PathBuf, AcpError> {
        let spawn_err = |source: std::io::Error| AcpError::Spawn {
            program: self.program.display().to_string(),
            source,
        };
        let has_dir = self.program.is_absolute() || self.program.components().count() > 1;
        if has_dir {
            let abs = std::path::absolute(&self.program).map_err(spawn_err)?;
            return if is_executable_file(&abs) {
                Ok(abs)
            } else {
                Err(spawn_err(not_found()))
            };
        }
        let path_var: OsString = self
            .env
            .get("PATH")
            .map(OsString::from)
            .or_else(|| std::env::var_os("PATH"))
            .unwrap_or_default();
        for dir in std::env::split_paths(&path_var) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            for candidate in candidates(&dir, &self.program) {
                if is_executable_file(&candidate) {
                    return std::path::absolute(candidate).map_err(spawn_err);
                }
            }
        }
        Err(spawn_err(not_found()))
    }
}

fn not_found() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::NotFound, "program not found")
}

#[cfg(windows)]
fn candidates(dir: &Path, program: &Path) -> Vec<PathBuf> {
    vec![
        dir.join(program),
        dir.join(program).with_extension("exe"),
        dir.join(program).with_extension("cmd"),
    ]
}

#[cfg(not(windows))]
fn candidates(dir: &Path, program: &Path) -> Vec<PathBuf> {
    vec![dir.join(program)]
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opencode_defaults() {
        let cmd = AcpCommand::opencode("/work");
        assert_eq!(cmd.program, PathBuf::from("opencode"));
        assert_eq!(cmd.args, ["acp"]);
        assert_eq!(cmd.cwd, PathBuf::from("/work"));
        assert_eq!(
            cmd.env
                .get("OPENCODE_DISABLE_AUTOUPDATE")
                .map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn builders_set_env() {
        let cmd = AcpCommand::opencode("/w")
            .env("A", "1")
            .with_config_content("{}");
        assert_eq!(cmd.env.get("A").map(String::as_str), Some("1"));
        assert_eq!(
            cmd.env.get("OPENCODE_CONFIG_CONTENT").map(String::as_str),
            Some("{}")
        );
    }

    #[test]
    fn missing_program_is_a_spawn_error() {
        let cmd = AcpCommand::new("definitely-not-a-real-program-xyz", "/");
        assert!(matches!(cmd.resolve_program(), Err(AcpError::Spawn { .. })));
    }

    #[cfg(unix)]
    #[test]
    fn bare_name_resolves_via_path_override() {
        let cmd = AcpCommand::new("sh", "/").env("PATH", "/bin:/usr/bin");
        let abs = cmd.resolve_program().unwrap();
        assert!(abs.is_absolute());
        assert!(abs.ends_with("sh"));
    }
}
