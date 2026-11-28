//! The agent as one process: [`serve`] reads the folder, assembles the agent (for the roles that
//! run workers: the model and the MCP servers) and hands it to [`adam_service::serve`], which
//! connects Postgres and runs the halves the `ROLE` asks for until told to stop.
//!
//! The sequence and the lifecycle are in the [README](https://github.com/vymalo/another-adam-rs/blob/main/bin/adam-agent/README.md#the-process).
//!
//! Everything that can be wrong with the files is found before anything connects to Postgres:
//! a mistake in them stops the process with every diagnostic, whether or not the database is up.

use std::future::Future;

use adam::AgentDef;
use adam_llm_agent::{HookSet, ModelParking};

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
            tracing::warn!(warning = %warning, "a warning while adding the extra MCP servers");
        }
        tracing::info!(file = %file.display(), "extra MCP servers added to the folder's own");
        folder.def = def;
    }
    // The deployment's session limits for every role (SESSION_*); the root's file may only lower them.
    // `MAX_PARALLEL_TOOL_CALLS` too, so a control plane, which binds no agent, holds a person's
    // `start.config` to the bounds a worker's session tools use (ADR 0054 decision 2).
    folder.def = folder
        .def
        .session_limits(config.service.sessions.limits)
        .session_max_parallel(config.service.sessions.max_parallel_tool_calls);
    if !folder.def.manifest().subagents.is_empty() {
        let limits = folder.def.effective_session_limits();
        tracing::info!(
            max_open = limits.max_open(),
            max_working = limits.max_working(),
            max_rounds = limits.max_rounds(),
            idle_days = config.service.sessions.idle_ttl.as_secs() / 86_400,
            "session limits"
        );
    }
    if config.worker.is_some() {
        folder.def = for_workers(folder.def, &config);
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
            // The deployment's hook programs that apply to the agent this process serves (ADR
            // 0050, slice P2b), started with the workers' parts: one that cannot start stops the
            // process (exit 78), as an unreadable key file does, and it happens before the model
            // client is built or any MCP server connects (the memory's folder is opened earlier,
            // by `for_workers`). The assembly gives the set to the root and its subagents.
            hooks: hooks_of(&config, folder.def.name())?,
            model: worker.model.client().map_err(AgentError::Model)?,
            alias: &worker.model.alias,
            profiles: worker.model.profiles.clone(),
            mcp: worker.mcp.policy(),
            options: settings.options(),
            // What a call was given and what it answered go into its step: the secrets of this
            // process (the configuration's and the environment's) are scrubbed from both.
            step_io: redact::step_io_named(&config, redact::process_vars(), &named_vars),
            max_parallel: settings.max_parallel_tool_calls,
            // A refused model call parks the run (ADR 0067); behind the injector, so does a gateway
            // that cannot be reached.
            parking: ModelParking::new(worker.model.park_limit)
                .when_unreachable(worker.model.injected_run),
        }),
        _ => None,
    };

    let agents = agents(folder.def, card, parts).await?;
    adam_service::serve(&config.service, agents, shutdown).await?;
    Ok(())
}

/// The deployment's hook programs that apply to the agent `agent` (ADR 0050, slice P2b): a
/// [`HookSet`] of the programs started, scrubbed with this process's own secrets. Empty when
/// `ADAM_HOOKS_FILE` is unset, so a deployment without one behaves as it did before.
///
/// # Errors
///
/// [`AgentError::Hooks`] for the first program that cannot start (exit 78): a deployment that
/// declared a program and cannot run it does not serve.
fn hooks_of(config: &Config, agent: &str) -> Result<HookSet, AgentError> {
    let secrets = redact::secret_values(config, redact::process_vars());
    config
        .service
        .hooks
        .hook_set_for(agent, move |text: &str| redact::scrub(&secrets, text))
        .map_err(AgentError::Hooks)
}

/// The definition the workers bind: what the deployment decides of the folder's remote subagents
/// applied, which is whether one may be at plain `http` on another machine (a service of the same
/// cluster: `A2A_ALLOW_INSECURE_REMOTES`).
fn for_workers(def: AgentDef, config: &Config) -> AgentDef {
    let Some(worker) = &config.worker else {
        return def;
    };
    let def = def
        .allow_insecure_remotes(worker.allow_insecure_remotes)
        .si_memory_ceiling(worker.si_memory);
    // The memory is the deployment's L1 (ADR 0081 decision 2): the folder it names, the ceiling
    // it set, and the folder's own `si_memory:` key. No folder, or a ceiling of `off`, means the
    // run holds none and is offered neither tool.
    let Some(dir) = &worker.si_memory_dir else {
        return def;
    };
    if !def.si_memory().is_on() {
        return def;
    }
    let secrets = crate::redact::secret_values(config, crate::redact::process_vars());
    let scrub = {
        let secrets = secrets.clone();
        adam_memory::Scrubber::new(move |text: &str| crate::redact::scrub(&secrets, text))
    };
    let memory = match adam_memory::FolderSiMemory::open(dir, adam_memory::Settings::new(scrub)) {
        Ok(memory) => std::sync::Arc::new(memory),
        Err(error) => {
            // A folder that cannot be opened is not a reason to refuse to serve: the run goes on
            // without a memory (ADR 0081 decision 12), and the operator reads why.
            tracing::warn!(%error, dir = %dir.display(), "the self-improvement memory's folder cannot be opened; runs go on without it");
            return def;
        }
    };
    // The folder is the deployment's L1, so its key is the agent it serves; two agents of one
    // process are kept apart by the agent key as well. The caller is the message's: the A2A
    // conversation id is `<subject>:<context id>`.
    let name = def.name().to_owned();
    let handle = adam_llm_agent::SiMemoryHandle::new(
        memory,
        adam_memory::Scope::agent(name.clone(), "-", name.clone()),
        |ctx: &adam_llm_agent::ToolCtx| {
            ctx.conversation_id()
                .and_then(|id| id.split_once(':'))
                .map(|(subject, _)| subject.to_owned())
                .filter(|subject| !subject.is_empty())
                .unwrap_or_else(|| "-".to_owned())
        },
    );
    // The deployment's L2, when `MEMORY_URL` names one: the audience's lessons come back from
    // `si_recall` after the L1 problems. A store that cannot be reached is a warning, not a
    // refusal to serve (ADR 0081 decision 12).
    let handle = match &worker.memory {
        None => handle,
        Some(crate::config::MemoryStore::Postgres(url)) => {
            let settings =
                adam_memory::LessonSettings::new(adam_memory::Scrubber::new(move |text: &str| {
                    crate::redact::scrub(&secrets, text)
                }));
            match adam_store_postgres::PgMemory::connect(
                url,
                settings,
                adam_memory::MemoryScope::new(name.clone()),
            ) {
                // The table is schema version 10 of the run store's database; a deployment that
                // shares that database has it already. `migrate` is idempotent and takes the same
                // advisory lock, so a process that serves memory alone can create it here.
                Ok(l2) => match l2.migrate() {
                    Ok(()) => {
                        handle.with_l2(std::sync::Arc::new(l2), adam_memory::Audience::new(name))
                    }
                    Err(error) => {
                        tracing::warn!(%error, "the L2 memory table cannot be created; runs read L1 alone");
                        handle
                    }
                },
                Err(error) => {
                    tracing::warn!(%error, "the L2 memory cannot be reached; runs read L1 alone");
                    handle
                }
            }
        }
        Some(crate::config::MemoryStore::Mongo(url)) => {
            let settings =
                adam_memory::LessonSettings::new(adam_memory::Scrubber::new(move |text: &str| {
                    crate::redact::scrub(&secrets, text)
                }));
            match adam_store_mongodb::memory::MongoMemory::connect(
                url,
                MEMORY_DATABASE,
                settings,
                adam_memory::MemoryScope::new(name.clone()),
            ) {
                Ok(l2) => handle.with_l2(
                    std::sync::Arc::new(l2),
                    adam_memory::Audience::new(name.clone()),
                ),
                Err(error) => {
                    tracing::warn!(%error, "the L2 memory cannot be reached; runs read L1 alone");
                    handle
                }
            }
        }
    };
    def.si_memory_handle(handle)
}

/// The database `MEMORY_URL` selects when its path names none.
const MEMORY_DATABASE: &str = "adam_memory";

#[cfg(test)]
mod tests {
    use adam_llm_agent::ToolSet;

    use super::*;

    /// `A2A_ALLOW_INSECURE_REMOTES` reaches the binding of the remote subagents through `serve`'s
    /// own step: off, an in-cluster `http` remote stops the start; on, it binds.
    #[test]
    fn the_switch_reaches_the_remote_subagents() {
        let tmp = tempfile::tempdir().expect("a temporary directory");
        std::fs::create_dir_all(tmp.path().join("agent/subagents")).expect("the folders");
        std::fs::write(
            tmp.path().join("agent/instructions.md"),
            "---\nname: chat\ndescription: Chats.\ntools: []\n---\nHello.\n",
        )
        .expect("the instructions");
        std::fs::write(
            tmp.path().join("agent/subagents/browser.md"),
            "---\ndescription: Reads pages.\na2a: http://browser.agents.svc:8080/card\n---\n",
        )
        .expect("the subagent");
        let config = |flag: Option<&str>| {
            let dir = tmp.path().display().to_string();
            Config::from_lookup(|name| {
                Some(
                    match name {
                        "DATABASE_URL" => "postgres://u:p@db/adam",
                        "MODEL_BASE_URL" => "https://gw.example/v1",
                        "MODEL_API_KEY" => "k",
                        "MODEL" => "m",
                        "A2A_BEARER_TOKENS" => "t",
                        "PUBLIC_URL" => "http://agent.svc:8080/",
                        "A2A_ALLOW_INSECURE_REMOTES" => return flag.map(str::to_owned),
                        n if n == adam::AGENT_DIR_ENV => return Some(dir.clone()),
                        _ => return None,
                    }
                    .to_owned(),
                )
            })
            .expect("a valid configuration")
        };
        let bind = |flag: Option<&str>| {
            let config = config(flag);
            assert!(config.worker.is_some(), "the role runs workers");
            let def = crate::folder::load(tmp.path()).expect("the folder").def;
            for_workers(def, &config).bind(ToolSet::new()).is_ok()
        };
        assert!(!bind(None), "off by default");
        assert!(!bind(Some("false")));
        assert!(bind(Some("true")));
    }
}
