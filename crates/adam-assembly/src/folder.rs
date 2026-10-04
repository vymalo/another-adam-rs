//! Run-time folders (no feature): read an agent's folder when the process starts, and say where
//! the folder is.
//!
//! [`AgentFolder::load`] does what a build script does for the embedded copy (read, validate,
//! digest), at run time, for exactly one agent. The folder is read once: a binary that wants the
//! files reloaded when they change needs the feature `dev` and `LiveAssembly`.
//!
//! The environment variable [`AGENT_DIR_ENV`] names the folder a deployment mounts;
//! [`agent_dir_from_env`] reads it, and [`agent_dir`] reads it with a default.
//! [`EXTRA_MCP_FILE_ENV`] names one more file, of MCP servers to add to the agent's own
//! (`AgentDef::with_extra_mcp_file`): [`extra_mcp_file_from_env`] reads it.

use std::ffi::OsString;
use std::path::PathBuf;

use adam_agent_fs::{Diagnostic, Digest, Dir, ManifestSource, Strictness};

use crate::def::AgentDef;
use crate::error::Error;

/// The environment variable that names the agent folder to read at run time:
/// `ADAM_AGENT_DIR=/etc/adam/agent`.
pub const AGENT_DIR_ENV: &str = "ADAM_AGENT_DIR";

/// The environment variable that names a file of extra MCP servers, in the shape of `mcp.json`
/// (`{"mcpServers": {...}}`), to add to the agent's own at startup:
/// `ADAM_EXTRA_MCP_FILE=/etc/adam/extra-mcp/mcp.json`. It adds servers to the folder's `mcp.json`
/// (or to the embedded copy's) and never replaces one: see [`AgentDef::with_extra_mcp_file`].
pub const EXTRA_MCP_FILE_ENV: &str = "ADAM_EXTRA_MCP_FILE";

/// [`EXTRA_MCP_FILE_ENV`] when it is set and not empty, else `None`: there is nothing to add.
pub fn extra_mcp_file_from_env() -> Option<PathBuf> {
    from_env_value(std::env::var_os(EXTRA_MCP_FILE_ENV))
}

/// The directory to read: [`AGENT_DIR_ENV`] when it is set and not empty, else `default`.
pub fn agent_dir(default: impl Into<PathBuf>) -> PathBuf {
    resolve_dir(default.into(), std::env::var_os(AGENT_DIR_ENV))
}

/// [`AGENT_DIR_ENV`] when it is set and not empty (surrounding whitespace does not count as a
/// value), else `None`: the caller then uses the copy it has embedded, or refuses to start.
pub fn agent_dir_from_env() -> Option<PathBuf> {
    from_env_value(std::env::var_os(AGENT_DIR_ENV))
}

fn resolve_dir(default: PathBuf, env: Option<OsString>) -> PathBuf {
    from_env_value(env).unwrap_or(default)
}

fn from_env_value(env: Option<OsString>) -> Option<PathBuf> {
    env.filter(|dir| !dir.to_string_lossy().trim().is_empty())
        .map(PathBuf::from)
}

/// The root a [`Dir`] reads (the directory that holds `agent/` or `agents/`) for a path that is
/// either that root or the `agent/` (`agents/`) directory itself.
pub(crate) fn project_root(path: PathBuf) -> PathBuf {
    if path.join("agent").is_dir() || path.join("agents").is_dir() {
        return path;
    }
    let named = path
        .file_name()
        .is_some_and(|name| name == "agent" || name == "agents");
    match path.parent() {
        Some(parent) if named && path.is_dir() => {
            if parent.as_os_str().is_empty() {
                PathBuf::from(".")
            } else {
                parent.to_path_buf()
            }
        }
        _ => path,
    }
}

impl AgentDef {
    /// One definition per agent found in a directory: [`from_source`](Self::from_source) over a
    /// [`Dir`], with [`Strictness::Lenient`].
    ///
    /// `path` is the directory that holds `agent/` (or `agents/`), or that directory itself. The
    /// environment variable [`AGENT_DIR_ENV`] replaces it when set. A process that serves one
    /// agent and wants the warnings and the digest too uses [`AgentFolder::load`].
    ///
    /// # Errors
    ///
    /// [`Error::Manifest`] when the directory cannot be read or has errors.
    pub fn from_dir(path: impl Into<PathBuf>) -> Result<Vec<Self>, Error> {
        let root = project_root(agent_dir(path));
        Self::from_source(&Dir::new(root), Strictness::Lenient)
    }
}

/// One agent, read from a folder at run time: the definition, what the load warned about, and
/// the digest that names these exact files.
///
/// The folder is what `build.rs` embeds, read when the process starts instead of when it was
/// built, so a deployment can change an agent's instructions, card, skills and `mcp.json`
/// without a build. It is read once; a change on disk is picked up by the next start.
///
/// ```
/// use adam_assembly::AgentFolder;
/// use adam_llm_agent::ToolSet;
/// # let root = std::env::temp_dir().join("adam-assembly-doc-folder");
/// # let _ = std::fs::remove_dir_all(&root);
/// # std::fs::create_dir_all(root.join("agent")).unwrap();
/// # std::fs::write(root.join("agent/instructions.md"),
/// #     "---\nname: helper\n---\nAnswer in plain words.\n").unwrap();
/// // `ADAM_AGENT_DIR=/etc/adam/agent`: the folder that holds `agent/`, or `agent/` itself.
/// let folder = AgentFolder::load(&root)?;
/// assert_eq!(folder.def.name(), "helper");
/// assert!(folder.warnings.is_empty());
/// println!("{}", folder.digest); // sha256:...
/// let bound = folder.def.bind(ToolSet::new())?;
/// # let _ = bound;
/// # let _ = std::fs::remove_dir_all(&root);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone)]
pub struct AgentFolder {
    /// The directory that holds `agent/` (or `agents/`): what was read.
    pub root: PathBuf,
    /// The agent. Nothing is bound yet: give it its vars and tools like any other
    /// [`AgentDef`].
    pub def: AgentDef,
    /// What the load found that does not stop it (an unknown key, a spec departure), in
    /// discovery order. Errors are an [`Err`] instead.
    pub warnings: Vec<Diagnostic>,
    /// The digest of the manifest and the bytes of the files its skills bundle. The same files
    /// give the same digest wherever they were read from, so it can be compared with the embedded
    /// copy's.
    pub digest: Digest,
}

impl AgentFolder {
    /// Read the one agent `path` holds, with [`Strictness::Lenient`]: warnings are returned, an
    /// error is an [`Err`].
    ///
    /// `path` is the directory that holds `agent/` (or `agents/`), or that directory itself.
    /// [`AGENT_DIR_ENV`] is not consulted here: [`agent_dir_from_env`] is how a caller reads it.
    /// An `agents/` folder is accepted when it holds exactly one agent.
    ///
    /// # Errors
    ///
    /// * [`Error::Manifest`] when the folder cannot be read (it does not exist, or is a file),
    ///   or has errors: its [`Invalid`](adam_agent_fs::Error::Invalid) carries every diagnostic.
    /// * [`Error::NotOneAgent`] when it holds no agent or more than one.
    pub fn load(path: impl Into<PathBuf>) -> Result<Self, Error> {
        let root = project_root(path.into());
        let dir = Dir::new(root.clone());
        let report = dir.load()?;
        let warnings: Vec<Diagnostic> = report.warnings().cloned().collect();
        let mut agents = report.into_package(Strictness::Lenient)?.agents;
        if agents.len() != 1 {
            return Err(Error::NotOneAgent {
                root,
                found: agents.into_iter().map(|agent| agent.name).collect(),
            });
        }
        let manifest = agents.remove(0);
        let digest = dir.digest(&manifest)?;
        let def = AgentDef::from_manifest(manifest)?.resources_from(&dir)?;
        Ok(Self {
            root,
            def,
            warnings,
            digest,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_environment_value_replaces_the_default_unless_it_is_empty() {
        let default = PathBuf::from("agent");
        assert_eq!(resolve_dir(default.clone(), None), default);
        assert_eq!(resolve_dir(default.clone(), Some(OsString::new())), default);
        assert_eq!(
            resolve_dir(default.clone(), Some(OsString::from("  "))),
            default
        );
        assert_eq!(
            resolve_dir(default, Some(OsString::from("/srv/other"))),
            PathBuf::from("/srv/other")
        );
    }

    #[test]
    fn the_variable_is_a_folder_only_when_it_says_something() {
        assert_eq!(from_env_value(None), None);
        assert_eq!(from_env_value(Some(OsString::new())), None);
        assert_eq!(from_env_value(Some(OsString::from(" \t"))), None);
        assert_eq!(
            from_env_value(Some(OsString::from("/etc/adam/agent"))),
            Some(PathBuf::from("/etc/adam/agent"))
        );
    }

    #[test]
    fn a_path_may_name_the_root_or_the_agent_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("project");
        std::fs::create_dir_all(root.join("agent")).unwrap();
        assert_eq!(project_root(root.clone()), root);
        assert_eq!(project_root(root.join("agent")), root);
        // `agents/` too, and a directory that is neither stays as it is.
        let many = tmp.path().join("many");
        std::fs::create_dir_all(many.join("agents/a")).unwrap();
        assert_eq!(project_root(many.join("agents")), many);
        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(project_root(plain.clone()), plain);
        // A path that does not exist is left for the load to report.
        let missing = tmp.path().join("missing").join("agent");
        assert_eq!(project_root(missing.clone()), missing);
    }
}
