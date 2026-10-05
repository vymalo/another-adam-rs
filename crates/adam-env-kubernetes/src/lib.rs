//! A run's processes in a Kubernetes pod of its own.
//!
//! [`KubeEnvironment`] is an [`Environment`](adam_workspace::Environment) of `adam-workspace`: the coder
//! asks it for the session of a run, has the session prepare each command (`run_command`, `run_checks`,
//! OpenCode and every command OpenCode starts), and spawns what comes back. The files and the paths are
//! the same in the pod as in the coder (the pod mounts the coder's workspace volume at the same path), so
//! the file tools and all git work stay in the coder, which is also where the credentials are.
//!
//! * **One pod per run.** `ensure` makes the pod `adam-run-<hash of the run id>` from the deployment's
//!   pod template ([`PodTemplate`]) and waits for it to be ready, reporting what it waits for as a step.
//!   The pod is found again by its name, so `ensure` is idempotent and a dropped call leaves nothing a
//!   later call cannot use.
//! * **The template is the deployment's.** Image, resources, priority class, security context, volumes,
//!   variables from Secrets and node affinity are in a file the chart mounts; the code fills in a name,
//!   labels and annotations. No GitHub credential is ever in a pod, and the cluster's admission policy
//!   (the chart's) refuses a pod that is anything else.
//! * **A command** is the program `adam-kube-exec`, which runs `adam-exec run|shell` in the pod
//!   (`pods/exec`) and ends with its exit code. [`Invocation`] is its command line.
//! * **Nothing is held for ever.** `release` deletes the pod; `held_runs` lists them for the janitor's
//!   sweep; [`KubeEnvironment::reap_idle`] deletes a pod no command used for [`Settings::idle`], and the
//!   next command makes another (the files are on the volume).
//! * **A quota is not a failure.** A pod the namespace's quota refuses is
//!   [`EnvError::Unavailable`](adam_workspace::EnvError::Unavailable), which the caller waits out.
//!
//! The decision, the facts it rests on and the lifecycle are
//! [ADR 0019](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0019-a-runs-processes-in-a-pod-of-their-own.md).
//!
//! ```no_run
//! # async fn demo(workspaces: adam_workspace::Workspaces) -> Result<(), Box<dyn std::error::Error>> {
//! use adam_env_kubernetes::{KubeEnvironment, PodTemplate, Settings};
//! use adam_workspace::{Environment, ExecSpec, NoProgress};
//!
//! let template = PodTemplate::from_file("/etc/adam/run-pod/pod.yaml".as_ref(), "run")?;
//! let settings = Settings::new("coder", "coder", "coder-0");
//! let environment = KubeEnvironment::new(kube::Client::try_default().await?, settings, template);
//!
//! let ws = workspaces.run("018f3a2b-7c1d-7000-8000-000000000001")?;
//! let session = environment.ensure(&ws, &NoProgress).await?;
//! let command = session.prepare(&ExecSpec::shell("cargo --version", ws.path()))?;
//! let output = command.command().output().await?;
//! # let _ = output;
//! # Ok(()) }
//! ```

#![warn(missing_docs)]

mod cluster;
mod environment;
mod exec;
mod invocation;
mod names;
mod session;
mod settings;
mod template;

#[cfg(test)]
mod fake;

pub use environment::KubeEnvironment;
pub use exec::{ExecError, ExitStatus, Target, exit_status, stdin_is_null, stream};
pub use invocation::{DEFAULT_ADAM_EXEC, Invocation, Mode, UsageError, is_env_name, is_exec_id};
pub use names::{
    INSTANCE_LABEL, MANAGED_BY, MANAGED_BY_LABEL, RUN_ID_ANNOTATION, RUN_LABEL, WORKER_ANNOTATION,
    is_label_value, pod_name, run_hash, selector,
};
pub use settings::{CLIENT_ENV, DEFAULT_CONTAINER, Settings, TOOLS_DIR};
pub use template::{Placement, PodTemplate};
