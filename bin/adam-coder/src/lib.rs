//! The coder agent: a coding task in, a verified pull request out, over A2A.
//!
//! Given "in repo X, do Y", the agent
//!
//! 1. prepares a git worktree (`prepare_workspace`, `adam-workspace`),
//! 2. makes the change: small, well-located edits itself (`read_file`, `write_file`, `apply_patch`),
//!    broad ones through OpenCode over ACP (`delegate_to_opencode`, `adam-acp`),
//! 3. runs the project's own checks, at most a configured number of failing
//!    cycles (`run_checks`; looking around uses `run_command`, which is no check),
//! 4. commits, pushes and opens a pull request (`commit_and_push`,
//!    `open_pull_request`), and
//! 5. streams progress throughout and reports the check results, the branch and the pull request
//!    as artifacts (`checks`, `branch`, `pull_request`).
//!
//! It is durable (an [`adam_runtime::Runtime`] journals every model and tool
//! step, so a restarted worker replays instead of repeating side effects) and
//! addressable (an [`adam_a2a`] server over [`adam_a2a_runtime`]).
//!
//! # Pieces
//!
//! | Module | What |
//! |---|---|
//! | [`agent`] | [`CoderAgent`]: the `LlmAgent` assembled from `agent/` + the completion policy (red checks or rejected credentials and no PR = failed; any other stop without a PR = a question, `input-required`) and the record of the repositories the person named; [`CoderStarter`]: its start-only half |
//! | [`tools`] | the nine tools of the coder (`#[tool]` functions reading [`ToolEnv`] from the agent's state; the screen's three, `ask_user`, `show` and `ui_catalog`, are `adam-ui`'s) and [`CoderSettings`] |
//! | `agent/instructions.md` | the system prompt, the loop's limits and the A2A card, as a file (embedded by `build.rs`, or read at startup from the folder `ADAM_AGENT_DIR` names) |
//! | [`files`] | [`AgentFiles`]: where those files come from, the embedded copy or a folder read once at startup, and why a folder is refused ([`AgentFilesError`]) |
//! | [`github_mcp`] | [`GitHubReadBearer`]: the coder's credentials as the bearer of each call to the GitHub MCP server (`http` mode, a sidecar), chosen by the owner and repository the call is about |
//! | [`redact`] | [`Redactor`]: the process's own secrets never leave in an error, an event or a tool result |
//! | [`harden`] | `make_non_dumpable` (it lives in `adam-service`, and `adam-agent` calls it too): a same-user child cannot read the coder's `/proc/<pid>/environ` |
//! | [`mcp_secrets`] | the variables an agent's `mcp.json` files read as `${VAR}`: hidden from the processes of runs (`HidingEnvironment`) and registered with the redactor, but never `PATH`, `HOME`, `MODEL_API_KEY` and the like |
//! | [`opencode`] | OpenCode's generated configuration and how it is launched |
//! | [`janitor`] | [`Janitor`]: the sweep that removes the workspaces of finished runs, a worker component of the process |
//! | [`app`] | [`Coder`]: runtime + A2A backend + router (an [`adam_service::Service`] for the coder's agent); [`Coder::control_plane`] for a process that only starts runs; [`LiveSignals`]: events and wake-up signals, in-process or across processes (from `adam-service`) |
//! | [`config`] | the binary's environment variables (the ones every agent binary shares are `adam-service`'s) |
//! | [`serve()`] | the whole process: the agent files, the model, GitHub, the workspaces and the MCP servers, then `adam_service::serve` for the store and the A2A server and workers its `ROLE` runs (through `adam_host::Host`), until a shutdown future resolves |
//!
//! # Composition
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use adam_coder::*;
//! # use adam_coder::opencode::OpenCodeLaunch;
//! # async fn demo(
//! #     store: adam_core::DynStore,
//! #     model: adam_model::DynModel,
//! #     workspaces: adam_workspace::Workspaces,
//! #     code_host: adam_workspace::DynCodeHost,
//! # ) {
//! let launch = OpenCodeLaunch::opencode("https://gateway.example/v1", "coder-large");
//! let env = Arc::new(ToolEnv::new(workspaces, code_host, CoderSettings::new(launch)));
//! let agent = CoderAgent::new(model, "coder-large", env);
//! let coder = Coder::new(store, agent, &RuntimeOptions::default());
//! # let _ = coder;
//! # }
//! ```
//!
//! A process that only serves A2A needs no model, credentials or workspaces:
//! starting a run needs only the agent's name and its `init`, which
//! [`CoderStarter`] provides.
//!
//! ```no_run
//! # use adam_coder::*;
//! # fn demo(store: adam_core::DynStore) {
//! let front = Coder::control_plane(store, &RuntimeOptions::default());
//! # let _ = front;
//! # }
//! ```
//!
//! By default a process's live events stay in the process and other processes are found by
//! polling the store. [`Coder::new_with`] and [`Coder::control_plane_with`] take
//! [`LiveSignals`] to change that; the binary passes the Postgres `LISTEN`/`NOTIFY` ones
//! (`adam-notify-postgres`), so a worker wakes at once for a run another process started and a
//! front streams the progress of a run a worker steps.
//!
//! Every infrastructure piece is a trait object handed in from outside: the
//! store, the model, the code host and the git credentials. The binary
//! (`adam-coder`) is only [`serve`] over [`Config::from_env`]: a composition
//! of the Postgres store and, for the roles that run workers, the
//! OpenAI-compatible model and GitHub. Which halves it runs (`all`,
//! `control-plane` or `worker`) is the `ROLE` variable, an [`adam_host::Role`];
//! see [`config`].

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod agent;
pub mod app;
pub mod config;
pub mod exit;
pub mod files;
pub mod github_mcp;
pub use adam_service::harden;
pub mod janitor;
pub mod mcp_secrets;
pub mod opencode;
pub mod redact;
mod repos;
mod serve;
pub mod tools;

pub use agent::{AGENT_NAME, CoderAgent, CoderStarter};
pub use app::{
    BUILD_REVISION, Coder, LiveSignals, RuntimeOptions, agent_card, agent_card_from, build_version,
};
pub use config::{
    AppInstallations, Config, ConfigError, GitHubAppConfig, GitHubAuth, McpSettings,
    RunEnvironment, RunPodsConfig, WorkerConfig,
};
pub use exit::exit_code;
pub use files::{AgentFiles, AgentFilesError};
pub use github_mcp::GitHubReadBearer;
pub use janitor::Janitor;
pub use redact::{RedactingCredentials, Redactor};
pub use repos::workspaces_for;
pub use serve::{environment_for, serve};
pub use tools::{CoderSettings, ToolEnv, coder_tools};
