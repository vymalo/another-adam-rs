//! A run's processes in its repository's devcontainer, on a rootless Podman service.
//!
//! [`DevContainer`] is an [`Environment`](adam_workspace::Environment) of `adam-workspace`: the
//! coder asks it for the session of a run, has the session prepare each command
//! (`run_command`, `run_checks`, OpenCode and every command OpenCode starts), and spawns what comes
//! back. The files and the paths are the same inside the container as in the coder, so the file
//! tools and all git work stay in the coder, which is also where the credentials are.
//!
//! * **The file is the environment.** The first slot of the run (a scratch project counts) decides,
//!   once, for the whole run: its `.devcontainer/devcontainer.json`, or `.devcontainer.json`, or the
//!   first of `.devcontainer/<folder>/devcontainer.json`. A slot with none gets
//!   [`Settings::default_image`]. Later repositories are mounted into the same container.
//! * **The file is untrusted.** It is checked three times: the file, the configuration
//!   the CLI merges from features and the image's metadata, and the container that was made, which
//!   has the last word. Nothing privileged, no bind mount, no Docker Compose, a short list of
//!   `runArgs`, `initializeCommand` removed, nothing published.
//! * **The CLI is the official one**, run with an empty environment and an allow-list, against
//!   Podman's remote client and the service's socket: never the host's Docker socket. Nothing of the
//!   coder's environment (the GitHub token, `DATABASE_URL`, the bearer tokens) is in a container; the
//!   model key is a read-only file.
//! * **Nothing is silent.** Making the environment is reported as a step ([`EnvStep`](adam_workspace::EnvStep));
//!   a broken file is an error that names the file and the problem, and stays one until the file
//!   changes or [`DevContainer::rebuild`] says otherwise; with no runtime the run goes on in the
//!   coder's own environment and a step says so.
//! * **Teardown.** [`release`](adam_workspace::Environment::release) removes the container, the
//!   images the run built and the run's files here, and checks by label that no container is left.
//!
//! The decision, the facts it rests on and the lifecycle are
//! [ADR 0010](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0010-a-run-works-in-its-repositorys-devcontainer.md).
//!
//! ```no_run
//! # async fn demo(workspaces: adam_workspace::Workspaces) -> Result<(), Box<dyn std::error::Error>> {
//! use adam_devcontainer::{DevContainer, Network, Runtime, Settings};
//! use adam_workspace::{Environment, ExecSpec, NoProgress};
//!
//! let mut settings = Settings::new(workspaces.root().to_owned());
//! settings.runtime = Runtime::Podman;
//! settings.container_host = "unix:///run/podman/podman.sock".to_owned();
//! settings.default_image = "mcr.microsoft.com/devcontainers/base:2.2.1-trixie".to_owned();
//! settings.network = Network::Inherit;
//! let environment = DevContainer::new(settings);
//!
//! let ws = workspaces.run("018f3a2b-7c1d-7000-8000-000000000001")?;
//! let session = environment.ensure(&ws, &NoProgress).await?;
//! let slot = &ws.slots_in_join_order().await?[0];
//! let command = session.prepare(&ExecSpec::shell("devbox-tool --version", slot.path()))?;
//! let output = command.command().output().await?;
//! # let _ = output;
//! # Ok(()) }
//! ```

#![warn(missing_docs)]

mod cli;
mod config;
mod environment;
mod error;
mod override_file;
mod podman;
mod policy;
mod session;
mod state;
mod tools;

use std::path::PathBuf;
use std::time::Duration;

use secrecy::SecretString;

pub use environment::DevContainer;

/// Whether devcontainers are used at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Runtime {
    /// No container runtime: every run is in the coder's own environment (a step says so when the
    /// repository has a devcontainer).
    #[default]
    Off,
    /// A rootless Podman service, reached through `podman-remote` and [`Settings::container_host`].
    Podman,
}

/// The network of a devcontainer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Network {
    /// The network of the Podman service (`--network=host`, which in the service is the service
    /// container's own), which the deployment limits.
    #[default]
    Inherit,
    /// None. OpenCode cannot reach its model.
    None,
}

/// How the environment is made. [`Settings::new`] has the defaults of the coder's configuration.
#[derive(Debug, Clone)]
pub struct Settings {
    /// The workspace root (`WORKSPACE_ROOT`). The Podman service must see it at the **same path**.
    pub root: PathBuf,
    /// Whether devcontainers are used.
    pub runtime: Runtime,
    /// The devcontainer CLI (`DEVCONTAINER_CLI`).
    pub cli: PathBuf,
    /// Podman's client, `--docker-path` of the CLI (`DEVCONTAINER_PODMAN`).
    pub podman: PathBuf,
    /// `CONTAINER_HOST` of the client: `unix:///run/podman/podman.sock`.
    pub container_host: String,
    /// The image of a repository that has no devcontainer file (`DEVCONTAINER_DEFAULT_IMAGE`),
    /// pinned by tag and digest.
    pub default_image: String,
    /// The network of the containers (`DEVCONTAINER_NETWORK`).
    pub network: Network,
    /// Labels the containers this deployment makes, so that a sweep finds them
    /// (`DEVCONTAINER_DEPLOYMENT_ID`).
    pub deployment: String,
    /// How long pulling, building and creating may take (`DEVCONTAINER_UP_TIMEOUT_SECS`, 1200).
    pub up_timeout: Duration,
    /// How long the repository's lifecycle commands may take (`DEVCONTAINER_SETUP_TIMEOUT_SECS`, 900).
    pub setup_timeout: Duration,
    /// How long reading the configuration may take (120 seconds).
    pub read_timeout: Duration,
    /// How long the probe of the service may take (10 seconds).
    pub probe_timeout: Duration,
    /// How long a probe's answer is reused (30 seconds).
    pub probe_interval: Duration,
    /// How long releasing a run may take (60 seconds).
    pub release_timeout: Duration,
    /// The OpenCode binary to mount in every container (a native ELF), if OpenCode is to run inside.
    pub opencode: Option<PathBuf>,
    /// The model key, written as a read-only file for OpenCode's `{file:...}`: never an argument or a variable.
    pub model_key: Option<SecretString>,
}

impl Settings {
    /// The defaults, for the workspace root `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            runtime: Runtime::Off,
            cli: PathBuf::from("devcontainer"),
            podman: PathBuf::from("podman-remote"),
            container_host: String::new(),
            default_image: String::new(),
            network: Network::Inherit,
            deployment: "adam-coder".to_owned(),
            up_timeout: Duration::from_secs(1200),
            setup_timeout: Duration::from_secs(900),
            read_timeout: Duration::from_secs(120),
            probe_timeout: Duration::from_secs(10),
            probe_interval: Duration::from_secs(30),
            release_timeout: Duration::from_secs(60),
            opencode: None,
            model_key: None,
        }
    }
}
