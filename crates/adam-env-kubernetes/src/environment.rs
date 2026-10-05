//! [`KubeEnvironment`]: the [`Environment`] that runs a run's processes in a pod of its own.
//!
//! The lifecycle of a run pod, which the cluster keeps (there is no state file: the pod, its labels
//! and its annotations are the state):
//!
//! ```text
//! first need ─▶ Requested ─▶ Starting ─▶ Ready ◀─┐   every ensure finds a Ready pod and reuses it
//!                  │            │           │ exec │
//!                  │ quota      │ timeout   └──────┘
//!                  ▼            ▼
//!              Unavailable    Deleted (unschedulable, image refused) ─▶ Requested   (the next ensure)
//! Ready ─▶ Deleted     the pod ended (OOM, eviction), or no command used it for `idle`
//! Ready, Starting ─▶ Deleted   the run ended: the janitor's `release`
//! ```

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use adam_workspace::{
    EnvError, EnvProgress, EnvSession, EnvStep, EnvStepState, Environment, RunWorkspace,
};
use async_trait::async_trait;
use k8s_openapi::api::core::v1::Pod;
use kube::Api;
use kube::Client;
use kube::api::{DeleteParams, ListParams, PostParams};
use tokio::sync::Mutex;

use crate::cluster::{clip, env_error, is_already_exists, is_not_found};
use crate::exec::{ClusterExec, ExitStatus, PodExec};
use crate::names::{RUN_ID_ANNOTATION, WORKER_ANNOTATION, pod_name, selector};
use crate::session::KubeSession;
use crate::settings::Settings;
use crate::template::{Placement, PodTemplate};

/// The id of the one step of making a run pod.
const STEP: &str = "run-pod";

/// How long a pod gets to end before it is gone, when it is deleted: the commands in it are
/// stopped with a short notice, because the quota counts a pod until it is gone.
const DELETE_GRACE_SECS: u32 = 2;

/// Waiting reasons of a container that no waiting will fix: the image or the pod's own
/// configuration is wrong.
const BLOCKED: &[&str] = &[
    "ErrImagePull",
    "ImagePullBackOff",
    "InvalidImageName",
    "CreateContainerConfigError",
    "CreateContainerError",
    "ErrImageNeverPull",
];

/// What the process holds: the settings, the API, and the per-run locks.
pub(crate) struct Inner {
    pub(crate) settings: Settings,
    pub(crate) template: PodTemplate,
    pub(crate) pods: Api<Pod>,
    pub(crate) exec: Arc<dyn PodExec>,
    pub(crate) counter: AtomicU64,
    /// Makes the ids of commands unique across restarts of the coder.
    pub(crate) epoch: String,
    locks: StdMutex<HashMap<String, Arc<Mutex<()>>>>,
    /// When a command last used the pod of a run, in this process.
    used: StdMutex<HashMap<String, Instant>>,
}

impl Inner {
    /// Note that the pod of `run` is in use now.
    pub(crate) fn touch(&self, run: &str) {
        self.used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(run.to_owned(), Instant::now());
    }

    fn lock_of(&self, run: &str) -> Arc<Mutex<()>> {
        Arc::clone(
            self.locks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(run.to_owned())
                .or_default(),
        )
    }

    fn forget(&self, run: &str) {
        self.locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(run);
        self.used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(run);
    }

    /// How long the pod of `run` has not been used, counting from now when it was never seen.
    fn idle_for(&self, run: &str) -> Duration {
        self.used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(run.to_owned())
            .or_insert_with(Instant::now)
            .elapsed()
    }

    async fn delete(&self, name: &str) -> Result<(), EnvError> {
        let params = DeleteParams {
            grace_period_seconds: Some(DELETE_GRACE_SECS),
            ..DeleteParams::default()
        };
        match self.pods.delete(name, &params).await {
            Ok(_) => Ok(()),
            Err(e) if is_not_found(&e) => Ok(()),
            Err(e) => Err(env_error(&e, "deleting the run pod")),
        }
    }
}

/// Name the process's TLS provider (rustls' default, aws-lc-rs) unless one is named already.
///
/// `kube` builds its TLS configuration with the process default, and rustls refuses to guess one when a
/// binary's dependency tree enables more than one provider, which the coder's does (an A2A client of the tree
/// enables `ring`): without this the first client panics. [`KubeEnvironment::connect`] and `adam-kube-exec`
/// call it; a caller that makes its own `kube::Client` calls it first. Idempotent.
pub fn install_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        // Another thread may have installed one in between: that is as good.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }
}

/// The Kubernetes environment. Cheap to clone: the clones are one environment.
#[derive(Clone)]
pub struct KubeEnvironment {
    pub(crate) inner: Arc<Inner>,
}

impl std::fmt::Debug for KubeEnvironment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KubeEnvironment")
            .field("namespace", &self.inner.settings.namespace)
            .field("instance", &self.inner.settings.instance)
            .finish_non_exhaustive()
    }
}

/// Reports the one step of a pod being made, once there is something to say: a pod that is there
/// and ready is reused with no step, so that a command does not announce itself.
struct Reporter<'a> {
    progress: &'a dyn EnvProgress,
    started: Instant,
    shown: bool,
    last: Option<String>,
}

impl<'a> Reporter<'a> {
    fn new(progress: &'a dyn EnvProgress) -> Self {
        Self {
            progress,
            started: Instant::now(),
            shown: false,
            last: None,
        }
    }

    fn send(&mut self, state: EnvStepState, detail: String) {
        self.shown = true;
        self.progress.step(
            EnvStep::new(STEP, "Run pod: a pod of its own for this run", state).with_detail(detail),
        );
    }

    /// A running step, only when what it says changed.
    fn running(&mut self, detail: String) {
        if self.last.as_deref() != Some(detail.as_str()) {
            self.last = Some(detail.clone());
            self.send(EnvStepState::Running, detail);
        }
    }

    fn completed(&mut self, detail: String) {
        if self.shown {
            self.send(EnvStepState::Completed, detail);
        }
    }

    fn failed(&mut self, detail: String) {
        self.send(EnvStepState::Failed, detail);
    }
}

impl KubeEnvironment {
    /// An environment over `client`, for the pods `template` describes. Nothing is made and nothing
    /// is read: the first `ensure` talks to the cluster.
    pub fn new(client: Client, settings: Settings, template: PodTemplate) -> Self {
        let pods: Api<Pod> = Api::namespaced(client, &settings.namespace);
        let exec = Arc::new(ClusterExec {
            api: pods.clone(),
            container: settings.container.clone(),
        });
        Self::over(pods, settings, template, exec)
    }

    /// An environment over the cluster this process runs in (the ServiceAccount's token and
    /// `KUBERNETES_SERVICE_HOST`), or `KUBECONFIG`'s when it is not in one.
    ///
    /// # Errors
    ///
    /// [`EnvError::Unavailable`] when no configuration is found or the client cannot be made.
    pub async fn connect(settings: Settings, template: PodTemplate) -> Result<Self, EnvError> {
        install_crypto_provider();
        let client = Client::try_default()
            .await
            .map_err(|e| env_error(&e, "connecting to the cluster"))?;
        Ok(Self::new(client, settings, template))
    }

    /// Sweep the idle pods in the background, for as long as the runtime lives
    /// ([`reap_loop`](Self::reap_loop)). A deployment with no idle timeout spawns nothing useful:
    /// the task ends at once.
    pub fn spawn_reaper(&self) -> tokio::task::JoinHandle<()> {
        let environment = self.clone();
        tokio::spawn(async move { environment.reap_loop().await })
    }

    /// The same over any [`PodExec`] (tests).
    pub(crate) fn over(
        pods: Api<Pod>,
        settings: Settings,
        template: PodTemplate,
        exec: Arc<dyn PodExec>,
    ) -> Self {
        let epoch = format!(
            "{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs())
        );
        Self {
            inner: Arc::new(Inner {
                settings,
                template,
                pods,
                exec,
                counter: AtomicU64::new(1),
                epoch,
                locks: StdMutex::default(),
                used: StdMutex::default(),
            }),
        }
    }

    /// The settings.
    pub fn settings(&self) -> &Settings {
        &self.inner.settings
    }

    /// The name of the pod of `run`.
    pub fn pod_name(run: &str) -> String {
        pod_name(run)
    }

    /// The pods of this deployment, by label.
    async fn list(&self) -> Result<Vec<Pod>, EnvError> {
        let params = ListParams::default().labels(&selector(&self.inner.settings.instance));
        self.inner
            .pods
            .list(&params)
            .await
            .map(|list| list.items)
            .map_err(|e| env_error(&e, "listing the run pods"))
    }

    /// A ready pod for `run`: the one that is there, or a new one.
    async fn ready_pod(
        &self,
        run: &str,
        name: &str,
        progress: &dyn EnvProgress,
    ) -> Result<Pod, EnvError> {
        let inner = &self.inner;
        let settings = &inner.settings;
        let mut reporter = Reporter::new(progress);
        let deadline = Instant::now() + settings.ready_timeout;
        loop {
            let found = inner
                .pods
                .get_opt(name)
                .await
                .map_err(|e| env_error(&e, "looking for the run pod"))?;
            match found {
                Some(pod) => {
                    if annotation(&pod, RUN_ID_ANNOTATION) != Some(run) {
                        return Err(EnvError::Refused(format!(
                            "the pod {name} belongs to another run (the names of two runs collided); release it first"
                        )));
                    }
                    if pod.metadata.deletion_timestamp.is_some() {
                        reporter.running("the run's earlier pod is going away".to_owned());
                    } else if ended(&pod) {
                        reporter.running("the pod ended; making another".to_owned());
                        inner.delete(name).await?;
                    } else if is_ready(&pod) {
                        reporter.completed(format!(
                            "ready in {} s{}",
                            reporter.started.elapsed().as_secs(),
                            inner
                                .template
                                .image()
                                .map(|image| format!(", image {image}"))
                                .unwrap_or_default()
                        ));
                        return Ok(pod);
                    } else {
                        reporter.running(waiting_for(&pod));
                        if Instant::now() >= deadline {
                            return Err(self.give_up(name, &pod, &mut reporter).await);
                        }
                    }
                }
                None => {
                    reporter.running(format!("requesting the pod {name}"));
                    let wanted = inner.template.render(
                        run,
                        name,
                        Placement {
                            namespace: &settings.namespace,
                            instance: &settings.instance,
                            worker: &settings.worker,
                        },
                    );
                    match inner.pods.create(&PostParams::default(), &wanted).await {
                        Ok(_) => {}
                        // Another worker, or an earlier call whose future was dropped, made it.
                        Err(e) if is_already_exists(&e) => {}
                        Err(e) => {
                            let error = env_error(&e, "making the run pod");
                            reporter.failed(error.to_string());
                            return Err(error);
                        }
                    }
                }
            }
            if Instant::now() >= deadline {
                // A pod that is going away, or one that never showed up: the deadline is the same.
                reporter.failed("the pod did not come up".to_owned());
                return Err(EnvError::Timeout {
                    phase: "start",
                    secs: settings.ready_timeout.as_secs(),
                });
            }
            tokio::time::sleep(settings.ready_poll).await;
        }
    }

    /// The pod did not become ready in time: say why, and delete it when waiting for it again would
    /// not help (the error is then the caller's to retry, or the person's to decide).
    async fn give_up(&self, name: &str, pod: &Pod, reporter: &mut Reporter<'_>) -> EnvError {
        let secs = self.inner.settings.ready_timeout.as_secs();
        let error = if let Some((reason, message)) = blocked(pod) {
            let _ = self.inner.delete(name).await;
            EnvError::Build {
                reason: format!("the run pod did not start: {reason}: {message}"),
                log_tail: String::new(),
            }
        } else if let Some(message) = unschedulable(pod) {
            let _ = self.inner.delete(name).await;
            EnvError::Unavailable(format!("no node has room for a run pod: {message}"))
        } else {
            EnvError::Timeout {
                phase: "start",
                secs,
            }
        };
        reporter.failed(error.to_string());
        error
    }

    /// One sweep of idle pods: the pods of this worker that no command has used for
    /// [`Settings::idle`] and that run nothing are deleted. The files are on the volume, so the next
    /// [`ensure`](Environment::ensure) of the run makes a new pod and nothing is lost.
    ///
    /// A pod that runs a command (`adam-exec active` counts the processes `adam-exec` started that are
    /// still alive: a long build, OpenCode) is never idle, however long ago the command was
    /// started. A pod that cannot be asked is left for the next sweep. Returns the run ids whose pod
    /// was deleted.
    ///
    /// # Errors
    ///
    /// [`EnvError`] when the pods cannot be listed.
    pub async fn reap_idle(&self) -> Result<Vec<String>, EnvError> {
        let inner = &self.inner;
        let Some(idle) = inner.settings.idle else {
            return Ok(Vec::new());
        };
        let mut deleted = Vec::new();
        for pod in self.list().await? {
            let (Some(run), Some(name)) = (
                annotation(&pod, RUN_ID_ANNOTATION).map(str::to_owned),
                pod.metadata.name.clone(),
            ) else {
                continue;
            };
            // Another worker's pod is its own to sweep (it holds the clock of its commands).
            if annotation(&pod, WORKER_ANNOTATION) != Some(inner.settings.worker.as_str())
                || pod.metadata.deletion_timestamp.is_some()
            {
                continue;
            }
            if inner.idle_for(&run) < idle {
                continue;
            }
            // An ensure in flight for the run is a command about to use it.
            let lock = inner.lock_of(&run);
            let Ok(_held) = lock.try_lock() else {
                continue;
            };
            if is_ready(&pod) {
                match self.busy(&name).await {
                    Ok(false) => {}
                    Ok(true) => {
                        inner.touch(&run);
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(%run, error = %adam_error::report(&e), "cannot ask a run pod whether it is busy; it stays");
                        continue;
                    }
                }
            }
            match inner.delete(&name).await {
                Ok(()) => {
                    tracing::info!(%run, pod = %name, idle_secs = idle.as_secs(), "deleted an idle run pod; the next command makes another");
                    inner.forget(&run);
                    deleted.push(run);
                }
                Err(e) => {
                    tracing::warn!(%run, error = %adam_error::report(&e), "cannot delete an idle run pod");
                }
            }
        }
        Ok(deleted)
    }

    /// Whether a command is alive in the pod.
    async fn busy(&self, pod: &str) -> Result<bool, EnvError> {
        let settings = &self.inner.settings;
        let argv = vec![settings.adam_exec.clone(), "active".to_owned()];
        let answer = self
            .inner
            .exec
            .capture(pod, argv, settings.exec_timeout)
            .await
            .map_err(|e| EnvError::Unavailable(e.to_string()))?;
        match (&answer.status, answer.stdout.trim().parse::<u64>()) {
            (ExitStatus::Code(0), Ok(n)) => Ok(n > 0),
            _ => Err(EnvError::Unavailable(format!(
                "adam-exec active answered {:?} {}",
                answer.status,
                clip(&answer.stderr)
            ))),
        }
    }

    /// Sweep the idle pods now and then, for as long as the process lives. Never returns; a failed
    /// sweep is logged. Spawn it on the runtime.
    pub async fn reap_loop(&self) {
        let every = self.inner.settings.reap_every;
        if self.inner.settings.idle.is_none() {
            return;
        }
        loop {
            tokio::time::sleep(every).await;
            if let Err(e) = self.reap_idle().await {
                tracing::warn!(error = %adam_error::report(&e), "cannot sweep the idle run pods");
            }
        }
    }
}

/// An annotation of a pod.
fn annotation<'a>(pod: &'a Pod, key: &str) -> Option<&'a str> {
    pod.metadata
        .annotations
        .as_ref()?
        .get(key)
        .map(String::as_str)
}

/// The phase of a pod.
fn phase(pod: &Pod) -> Option<&str> {
    pod.status.as_ref()?.phase.as_deref()
}

/// Whether the pod is over: it will not run again (a container that fails restarts, so this is a
/// pod the cluster evicted or one whose restart policy gave up).
fn ended(pod: &Pod) -> bool {
    matches!(phase(pod), Some("Failed" | "Succeeded"))
}

/// Whether the pod runs and says it is ready.
pub(crate) fn is_ready(pod: &Pod) -> bool {
    phase(pod) == Some("Running")
        && pod
            .status
            .as_ref()
            .and_then(|s| s.conditions.as_ref())
            .is_some_and(|c| c.iter().any(|c| c.type_ == "Ready" && c.status == "True"))
}

/// What a pod that is not ready is waiting for, in one line.
fn waiting_for(pod: &Pod) -> String {
    if let Some((reason, message)) = blocked(pod) {
        return format!("{reason}: {message}");
    }
    if let Some(message) = unschedulable(pod) {
        return format!("waiting for a node: {message}");
    }
    let status = pod.status.as_ref();
    let waiting = |statuses: Option<&Vec<k8s_openapi::api::core::v1::ContainerStatus>>,
                   prefix: &str| {
        statuses?.iter().find_map(|s| {
            let waiting = s.state.as_ref()?.waiting.as_ref()?;
            let reason = waiting.reason.as_deref()?;
            Some(format!("{prefix}{reason}"))
        })
    };
    waiting(
        status.and_then(|s| s.init_container_statuses.as_ref()),
        "init: ",
    )
    .or_else(|| waiting(status.and_then(|s| s.container_statuses.as_ref()), ""))
    .unwrap_or_else(|| format!("the pod is {}", phase(pod).unwrap_or("pending")))
}

/// A container that waits for a reason waiting does not fix: its reason and message.
fn blocked(pod: &Pod) -> Option<(String, String)> {
    let status = pod.status.as_ref()?;
    let mut all = status
        .init_container_statuses
        .iter()
        .flatten()
        .chain(status.container_statuses.iter().flatten());
    all.find_map(|s| {
        let waiting = s.state.as_ref()?.waiting.as_ref()?;
        let reason = waiting.reason.as_deref()?;
        BLOCKED.contains(&reason).then(|| {
            (
                reason.to_owned(),
                clip(waiting.message.as_deref().unwrap_or("")),
            )
        })
    })
}

/// The scheduler's message when no node fits the pod.
fn unschedulable(pod: &Pod) -> Option<String> {
    pod.status
        .as_ref()?
        .conditions
        .iter()
        .flatten()
        .find(|c| c.type_ == "PodScheduled" && c.status == "False")
        .map(|c| {
            clip(&format!(
                "{}: {}",
                c.reason.as_deref().unwrap_or("Unschedulable"),
                c.message.as_deref().unwrap_or("")
            ))
        })
}

#[async_trait]
impl Environment for KubeEnvironment {
    async fn ensure(
        &self,
        workspace: &RunWorkspace,
        progress: &dyn EnvProgress,
    ) -> Result<Arc<dyn EnvSession>, EnvError> {
        let run = workspace.run().to_owned();
        let name = pod_name(&run);
        // Single-flight per run in this process; across workers, the pod's name is the lock (a
        // create that finds the pod there reuses it), and a dropped future leaves nothing but a pod
        // that the next call finds.
        let lock = self.inner.lock_of(&run);
        let _held = lock.lock().await;
        self.ready_pod(&run, &name, progress).await?;
        self.inner.touch(&run);
        Ok(Arc::new(KubeSession {
            inner: Arc::clone(&self.inner),
            run,
            pod: name,
            run_dir: PathBuf::from(workspace.path()),
        }))
    }

    async fn release(&self, run: &str) -> Result<(), EnvError> {
        let lock = self.inner.lock_of(run);
        let _held = lock.lock().await;
        self.inner.delete(&pod_name(run)).await?;
        self.inner.forget(run);
        Ok(())
    }

    async fn held_runs(&self) -> Result<Vec<String>, EnvError> {
        let mut runs: Vec<String> = self
            .list()
            .await?
            .iter()
            .filter_map(|pod| annotation(pod, RUN_ID_ANNOTATION).map(str::to_owned))
            .collect();
        runs.sort();
        runs.dedup();
        Ok(runs)
    }

    /// Delete the run's pod: the next `ensure` makes a new one from the template. There is no
    /// repository file to ignore (the template is the deployment's), so `use_default` changes
    /// nothing.
    async fn rebuild(&self, run: &str, use_default: bool) -> Result<bool, EnvError> {
        let _ = use_default;
        self.release(run).await?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_crypto_provider_is_named_once_and_naming_it_again_is_harmless() {
        install_crypto_provider();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
        install_crypto_provider();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }
}
