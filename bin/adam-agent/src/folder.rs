//! The agent folder: read once, when the process starts, by every role.

use std::path::Path;

use adam::AgentFolder;

use crate::error::AgentError;

/// Read the folder `path` names (the directory that holds `agent/`, or `agent/` itself) with the
/// loader a build script uses: warnings are kept, an error is refused, and the folder must hold
/// exactly one agent (`agents/` is refused: one agent per process).
///
/// # Errors
///
/// [`AgentError::Folder`] when the folder cannot be read, has errors in its files or holds more
/// than one agent. Always the deployment's mistake (exit 78), and every finding is in the message.
pub fn load(path: &Path) -> Result<AgentFolder, AgentError> {
    AgentFolder::load(path).map_err(|reason| AgentError::Folder {
        path: path.to_path_buf(),
        reason: Box::new(reason),
    })
}

/// Say which files this process runs: one `agent files` line (`source=folder`, `path`, `digest`,
/// `agent`, `warnings`: the same line `adam-coder` logs), then each warning as
/// `path:line: warning: ...`, then what this binary does not do with the folder.
pub fn log(folder: &AgentFolder) {
    tracing::info!(
        source = "folder",
        path = folder.root.display().to_string(),
        digest = folder.digest.as_str(),
        agent = folder.def.name(),
        warnings = folder.warnings.len(),
        "agent files"
    );
    for warning in &folder.warnings {
        tracing::warn!("{warning}");
    }
    if !folder.def.manifest().schedules.is_empty() {
        tracing::warn!("the agent folder has schedules, which adam-agent reads but does not run");
    }
}
