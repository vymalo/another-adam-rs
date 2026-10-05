//! How the environment is made: what the coder's configuration (`RUN_POD_*`) says.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use crate::invocation::DEFAULT_ADAM_EXEC;

/// The directory the template mounts the tools in (`adam-exec`, `opencode`), read-only, in the run
/// container: the same layout as the devcontainer's.
pub const TOOLS_DIR: &str = "/opt/adam/bin";

/// The name of the container the commands run in unless told otherwise.
pub const DEFAULT_CONTAINER: &str = "run";

/// The names of the coder's own variables that `adam-kube-exec` needs to reach the cluster. The
/// client is started with an empty environment and these (the ones that are set), so that none of the
/// coder's secrets is in its environment.
pub const CLIENT_ENV: &[&str] = &[
    "KUBERNETES_SERVICE_HOST",
    "KUBERNETES_SERVICE_PORT",
    "KUBECONFIG",
    "HOME",
    "PATH",
];

/// How the environment is made. [`Settings::new`] has the defaults of the coder's configuration.
#[derive(Debug, Clone)]
pub struct Settings {
    /// The namespace the run pods are made in (`RUN_POD_NAMESPACE`): the coder's own.
    pub namespace: String,
    /// The release's `app.kubernetes.io/instance` label value (`RUN_POD_INSTANCE`): the pods this
    /// deployment lists, sweeps and counts are the ones that carry it.
    pub instance: String,
    /// The worker making the pods (`WORKER_ID`, else the deployment's id): the idle timeout is that
    /// worker's own.
    pub worker: String,
    /// The name of the container the commands run in (`RUN_POD_CONTAINER`).
    pub container: String,
    /// The program that runs one command in a pod (`RUN_POD_EXEC_BINARY`): `adam-kube-exec`.
    pub exec_client: PathBuf,
    /// Where `adam-exec` is in the run container.
    pub adam_exec: String,
    /// Where the tools are in the run container ([`TOOLS_DIR`]).
    pub tools_dir: String,
    /// The coder has an OpenCode to bring along: the template's init container puts it in the tools
    /// directory, and [`tool_path`](adam_workspace::EnvSession::tool_path) says where.
    pub opencode: bool,
    /// The pod's template gives the commands a model key (`MODEL_API_KEY`, from the deployment's
    /// Secret), so that [`secret_ref`](adam_workspace::EnvSession::secret_ref) can name it.
    pub model_key: bool,
    /// How long a pod may take to be ready (`RUN_POD_READY_TIMEOUT_SECS`, 300): scheduling, pulling
    /// the image, starting.
    pub ready_timeout: Duration,
    /// How often the pod is looked at while waiting (2 seconds).
    pub ready_poll: Duration,
    /// How long no command may have used a pod before it is deleted (`RUN_POD_IDLE_SECS`, 900);
    /// `None` keeps it until the run ends.
    pub idle: Option<Duration>,
    /// How often the idle sweep looks (60 seconds).
    pub reap_every: Duration,
    /// How long stopping a command, or asking whether the pod is busy, may take (15 seconds).
    pub exec_timeout: Duration,
    /// The environment variables `adam-kube-exec` is started with, besides nothing: see
    /// [`CLIENT_ENV`]. [`Settings::new`] reads those names from the process.
    pub client_env: BTreeMap<String, String>,
}

impl Settings {
    /// The defaults, for the pods of the release `instance` in `namespace`, made by `worker`.
    pub fn new(
        namespace: impl Into<String>,
        instance: impl Into<String>,
        worker: impl Into<String>,
    ) -> Self {
        Self {
            namespace: namespace.into(),
            instance: instance.into(),
            worker: worker.into(),
            container: DEFAULT_CONTAINER.to_owned(),
            exec_client: PathBuf::from("adam-kube-exec"),
            adam_exec: DEFAULT_ADAM_EXEC.to_owned(),
            tools_dir: TOOLS_DIR.to_owned(),
            opencode: true,
            model_key: true,
            ready_timeout: Duration::from_secs(300),
            ready_poll: Duration::from_secs(2),
            idle: Some(Duration::from_secs(900)),
            reap_every: Duration::from_secs(60),
            exec_timeout: Duration::from_secs(15),
            client_env: CLIENT_ENV
                .iter()
                .filter_map(|name| {
                    std::env::var(name)
                        .ok()
                        .map(|value| ((*name).to_owned(), value))
                })
                .collect(),
        }
    }
}
