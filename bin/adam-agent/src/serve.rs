//! The agent as one process: [`serve`] reads the folder, assembles the agent (for the roles that
//! run workers: the model and the MCP servers) and hands it to [`adam_service::serve`], which
//! connects Postgres and runs the halves the `ROLE` asks for until told to stop.
//!
//! The sequence and the lifecycle are in the [README](https://github.com/vymalo/another-adam-rs/blob/main/bin/adam-agent/README.md#the-process).
//!
//! Everything that can be wrong with the files is found before anything connects to Postgres:
//! a mistake in them stops the process with every diagnostic, whether or not the database is up.

use std::future::Future;

use crate::agent::{WorkerParts, agents, card_of_folder};
use crate::config::Config;
use crate::error::AgentError;
use crate::folder;
use crate::redact;

/// Run the agent until `shutdown` resolves (SIGTERM in the binary).
///
/// Which components run depends on the role; see [`adam_service::serve`]. On shutdown the server
/// stops taking connections, and the workers finish and commit the steps they are in before this
/// returns.
///
/// # Errors
///
/// [`AgentError`]: the folder (`ADAM_AGENT_DIR`) cannot be read, an MCP server of its `mcp.json`
/// cannot be connected, the files and the code disagree, or the service cannot start or stops
/// unexpectedly (Postgres, the listen address, a component). Each error says which step failed
/// and never contains a credential.
pub async fn serve(
    config: Config,
    shutdown: impl Future<Output = ()> + Send,
) -> Result<(), AgentError> {
    // The files first, for every role and before anything connects.
    let mut folder = folder::load(&config.agent_dir)?;
    folder::log(&folder);

    // The extra MCP servers `ADAM_EXTRA_MCP_FILE` names are added to the folder's own, by the
    // roles that connect servers; a name the folder has is refused.
    if let Some(file) = config
        .worker
        .as_ref()
        .and_then(|worker| worker.extra_mcp_file.as_ref())
    {
        let (def, warnings) = folder
            .def
            .clone()
            .with_extra_mcp_file(file)
            .map_err(|e| AgentError::ExtraMcp(Box::new(e)))?;
        for warning in &warnings {
            tracing::warn!("{warning}");
        }
        tracing::info!(file = %file.display(), "extra MCP servers added to the folder's own");
        folder.def = def;
    }
    let named_vars = folder.def.mcp_env_references();

    // The card of the files this process runs, for the roles that serve A2A.
    let card = config
        .service
        .public_url
        .as_ref()
        .map(|url| card_of_folder(&folder, url))
        .transpose()?;

    // Only a role that runs workers builds the model client and connects the MCP servers.
    let parts = match (&config.worker, &config.service.worker) {
        (Some(worker), Some(settings)) => Some(WorkerParts {
            model: worker.model.client().map_err(AgentError::Model)?,
            alias: &worker.model.alias,
            mcp: worker.mcp.policy(),
            options: settings.options(),
            // What a call was given and what it answered go into its step: the secrets of this
            // process (the configuration's and the environment's) are scrubbed from both.
            step_io: redact::step_io_named(&config, redact::process_vars(), &named_vars),
        }),
        _ => None,
    };

    let agents = agents(folder.def, card, parts).await?;
    adam_service::serve(&config.service, agents, shutdown).await?;
    Ok(())
}
