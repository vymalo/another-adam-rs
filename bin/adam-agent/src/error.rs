//! Why the process cannot serve its agent, and what exit code that is.

use std::path::PathBuf;

use adam::AssemblyError;
use adam::agent_fs::{Diagnostic, Error as FilesError};
use adam_error::{Classify, ErrorClass};
use adam_service::ServeError;

/// Why [`serve`](crate::serve) (or a step of it) failed. The message names the step; the cause is
/// the [`source`](std::error::Error::source), so an error chain printed whole says each thing once.
///
/// Everything wrong with the files, the model settings or what the files ask of the deployment is
/// the deployment's mistake ([`ErrorClass::Invalid`], exit 78), except what the assembly classifies
/// otherwise: an MCP server that is down is exit 69. See [`exit_code`](crate::exit_code).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AgentError {
    /// The agent folder cannot be read, has errors in its files, or holds more than one agent.
    /// Every diagnostic is in the message, as `path:line: error: what is wrong`.
    #[error("cannot read the agent folder `{}` (ADAM_AGENT_DIR): {}", path.display(), explain(reason))]
    Folder {
        /// The folder, as `ADAM_AGENT_DIR` named it.
        path: PathBuf,
        /// What the assembly reported (boxed: it is large). Not a `source`: the message already
        /// says all of it.
        reason: Box<AssemblyError>,
    },
    /// The agent card cannot be made from the files (a folder that declares no description).
    #[error("building the agent card")]
    Card(#[source] Box<AssemblyError>),
    /// The model client cannot be built from `MODEL_BASE_URL` and `MODEL_API_KEY`.
    #[error("building the model client")]
    Model(#[source] adam_service::OpenAiConfigError),
    /// An MCP server of the folder cannot be connected, or the policy refuses it.
    #[error(
        "connecting the MCP servers of the agent files (MCP_ALLOW_STDIO, MCP_ALLOW_INSECURE and \
         MCP_ALLOW_URL_VARS decide which kinds they may be)"
    )]
    Mcp(#[source] Box<AssemblyError>),
    /// The file of extra MCP servers (`ADAM_EXTRA_MCP_FILE`) cannot be read, has errors, or names
    /// a server the folder already has.
    #[error("adding the extra MCP servers of ADAM_EXTRA_MCP_FILE to the folder's own")]
    ExtraMcp(#[source] Box<AssemblyError>),
    /// The files and the code disagree: an unknown tool in `tools:`, a var with no value, a
    /// placeholder the frontmatter does not declare, a model alias the assembly refuses.
    #[error("assembling the agent")]
    Assembly(#[source] Box<AssemblyError>),
    /// The service stopped or could not start: Postgres, the listen address, a component.
    #[error("running the service")]
    Serve(#[source] ServeError),
}

impl AgentError {
    /// The class of the errors that are about the files, the model settings or the deployment;
    /// `None` for the rest, which [`exit_code`](crate::exit_code) finds further down the chain.
    pub(crate) fn own_class(&self) -> Option<ErrorClass> {
        match self {
            Self::Folder { .. } => Some(ErrorClass::Invalid),
            Self::Card(e) | Self::Mcp(e) | Self::ExtraMcp(e) | Self::Assembly(e) => Some(e.class()),
            Self::Model(_) | Self::Serve(_) => None,
        }
    }

    /// The findings of a folder with errors, in discovery order (empty for any other reason).
    pub fn diagnostics(&self) -> &[Diagnostic] {
        match self {
            Self::Folder { reason, .. } => diagnostics_of(reason),
            _ => &[],
        }
    }
}

impl From<ServeError> for AgentError {
    fn from(e: ServeError) -> Self {
        Self::Serve(e)
    }
}

/// The reason, with every diagnostic of a folder that has errors, one per line.
fn explain(reason: &AssemblyError) -> String {
    match diagnostics_of(reason) {
        [] => reason.to_string(),
        diagnostics => {
            let lines: Vec<String> = diagnostics.iter().map(ToString::to_string).collect();
            format!("\n  - {}", lines.join("\n  - "))
        }
    }
}

fn diagnostics_of(reason: &AssemblyError) -> &[Diagnostic] {
    match reason {
        AssemblyError::Manifest(FilesError::Invalid { diagnostics }) => diagnostics,
        _ => &[],
    }
}
