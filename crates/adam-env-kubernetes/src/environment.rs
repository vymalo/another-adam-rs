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
//! Ready ─▶ Deleted     a slot was added and the pod lacks its mount, with no command running (else: wait); `ensure`
//!                      does it for the next command, `seal` right after the slot is added
//! Ready, Starting ─▶ Deleted   the run ended: the janitor's `release`
//! ```

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use adam_workspace::{
    EnvError, EnvProgress, EnvSession, EnvStep, EnvStepState, Environment, RunWorkspace,
};
use async_trait::async_trait;
use futures::TryStreamExt;
use k8s_openapi::api::core::v1::Pod;
use kube::Api;
use kube::Client;
use kube::api::{DeleteParams, ListParams, PostParams, WatchEvent, WatchParams};
use tokio::sync::Mutex;

use crate::cluster::{clip, env_error, is_already_exists, is_not_found};
use crate::exec::{ClusterExec, ExitStatus, PodExec};
use crate::mounts::WorkspaceMounts;
use crate::names::{
    MOUNTS_ANNOTATION, REPOSITORY_IMAGE_ANNOTATION, RUN_ID_ANNOTATION, WORKER_ANNOTATION, pod_name,
    selector,
};
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

/// Why [`KubeEnvironment::wait_for_change`] returned.
enum Woke {
    /// The pod changed (or was deleted): read it again.
    Changed,
    /// The pod's deadline passed.
    Deadline,
    /// There is no watch to wait on: fall back to the poll.
    Closed,
}

/// What the process holds: the settings, the API, and the per-run locks.
pub(crate) struct Inner {
    pub(crate) settings: Settings,
    pub(crate) template: PodTemplate,
    pub(crate) pods: Api<Pod>,
    pub(crate) exec: Arc<dyn PodExec>,
    pub(crate) counter: AtomicU64,
    /// Whether a watch that could not be kept has been reported at `warn` already.
    watch_warned: AtomicBool,
    /// Makes the ids of commands unique across restarts of the coder.
    pub(crate) epoch: String,
    locks: StdMutex<HashMap<String, Arc<Mutex<()>>>>,
    /// When a command last used the pod of a run, in this process.
    used: StdMutex<HashMap<String, Instant>>,
    /// The image each run chose, when one did (`use_repository_environment`, ADR 0052): `Some(None)`
    /// is a run sent back to the deployment's image. The pod's annotation is the durable record;
    /// this is what this process has read or been told.
    images: StdMutex<HashMap<String, Option<String>>>,
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

    /// What this process holds of a run that is over: its lock, its clock and its image choice.
    fn forget(&self, run: &str) {
        self.forget_pod(run);
        self.images
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(run);
    }

    /// What this process holds of a run whose pod is gone and whose run is not (the idle sweep):
    /// the image choice is the run's and outlives the pod, so it stays.
    fn forget_pod(&self, run: &str) {
        self.locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(run);
        self.used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(run);
    }

    /// The image this process knows `run` chose: `Some(None)` is the deployment's image, and `None`
    /// is "not read yet" (a run this process has not made a pod for).
    fn wanted_image(&self, run: &str) -> Option<Option<String>> {
        self.images
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(run)
            .cloned()
    }

    fn remember_image(&self, run: &str, image: Option<String>) {
        self.images
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(run.to_owned(), image);
    }

    /// Say that the watch of the run pod cannot be kept: the first one at `warn`, so a missing
    /// `watch` RBAC (or an API that refuses watches) is visible in the logs, the rest at `debug`
    /// (a pod that takes minutes to come up would otherwise repeat it every few seconds).
    fn note_watch_problem(&self, pod: &str, detail: &str) {
        if self.watch_warned.swap(true, Ordering::Relaxed) {
            tracing::debug!(pod = %pod, %detail, "the watch of the run pod cannot be kept; polling it instead");
        } else {
            tracing::warn!(pod = %pod, %detail, "the watch of the run pod cannot be kept; polling it instead");
        }
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
                watch_warned: AtomicBool::new(false),
                epoch,
                locks: StdMutex::default(),
                used: StdMutex::default(),
                images: StdMutex::default(),
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

    /// A ready pod for `run`: the one that is there, or a new one, mounting what `mounts` says.
    ///
    /// A pod whose mounts lack one of `mounts` (a slot added since it was made, or a pod made before
    /// mounts were recorded) is made again, because the mounts of a pod are fixed when it is made. The files
    /// are on the volume, so only what is running in it is lost: a pod that runs a command (or cannot be
    /// asked) is waited for instead, as a quota is ([`EnvError::Unavailable`]).
    async fn ready_pod(
        &self,
        run: &str,
        name: &str,
        mounts: &WorkspaceMounts,
        progress: &dyn EnvProgress,
    ) -> Result<Pod, EnvError> {
        let inner = &self.inner;
        let settings = &inner.settings;
        let mut reporter = Reporter::new(progress);
        let deadline = Instant::now() + settings.ready_timeout;
        // The image the run chose, when this process knows it: `None` is "not read yet", and the
        // pod's annotation is then the record (a coder that restarted finds the choice there).
        let mut wanted = inner.wanted_image(run);
        // The pod's `resourceVersion` as of the last read (or create): the watch resumes from it,
        // so it delivers the *next* change and not the state this loop already has.
        let mut resource_version: Option<String> = None;
        loop {
            let found = inner
                .pods
                .get_opt(name)
                .await
                .map_err(|e| env_error(&e, "looking for the run pod"))?;
            match found {
                Some(pod) => {
                    resource_version = pod.metadata.resource_version.clone();
                    if annotation(&pod, RUN_ID_ANNOTATION) != Some(run) {
                        return Err(EnvError::Refused(format!(
                            "the pod {name} belongs to another run (the names of two runs collided); release it first"
                        )));
                    }
                    let pod_image =
                        annotation(&pod, REPOSITORY_IMAGE_ANNOTATION).map(str::to_owned);
                    if wanted.is_none() {
                        // A pod this process did not make holds the choice: it is the record.
                        inner.remember_image(run, pod_image.clone());
                        wanted = Some(pod_image.clone());
                    }
                    let image_changed = wanted.as_ref().is_some_and(|w| *w != pod_image);
                    if pod.metadata.deletion_timestamp.is_some() {
                        reporter.running("the run's earlier pod is going away".to_owned());
                    } else if ended(&pod) {
                        reporter.running("the pod ended; making another".to_owned());
                        inner.delete(name).await?;
                    } else if image_changed {
                        let why = match wanted.as_ref().and_then(|w| w.as_deref()) {
                            Some(image) => {
                                format!("the run's image is {image}, which its pod does not run")
                            }
                            None => "the run is back on the deployment's image".to_owned(),
                        };
                        if is_ready(&pod) {
                            self.free_to_replace(name, &why).await?;
                        }
                        reporter.running(format!("making the pod again: {why}"));
                        inner.delete(name).await?;
                    } else if let Some(mount) =
                        mounts.missing_from(annotation(&pod, MOUNTS_ANNOTATION))
                    {
                        let missing = mount.path().display().to_string();
                        if is_ready(&pod) {
                            self.free_for_new_mount(name, &missing).await?;
                        }
                        reporter.running(format!(
                            "making the pod again: the run's workspace has {missing} as it is now, which the pod does not mount"
                        ));
                        inner.delete(name).await?;
                    } else if is_ready(&pod) {
                        reporter.completed(format!(
                            "ready in {} s{}",
                            reporter.started.elapsed().as_secs(),
                            pod_image
                                .as_deref()
                                .or_else(|| inner.template.image())
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
                    let chosen = wanted.as_ref().and_then(|w| w.as_deref());
                    let wanted_pod = inner.template.render(
                        run,
                        name,
                        Placement {
                            namespace: &settings.namespace,
                            instance: &settings.instance,
                            worker: &settings.worker,
                        },
                        mounts,
                        chosen,
                    );
                    match inner.pods.create(&PostParams::default(), &wanted_pod).await {
                        Ok(created) => {
                            resource_version = created.metadata.resource_version.clone();
                        }
                        // Another worker, or an earlier call whose future was dropped, made it.
                        Err(e) if is_already_exists(&e) => {}
                        Err(e) => {
                            let error = env_error(&e, "making the run pod");
                            reporter.failed(error.to_string());
                            return Err(error);
                        }
                    }
                    if wanted.is_none() {
                        inner.remember_image(run, None);
                        wanted = Some(None);
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
            // A watch resumes from the version the read returned. Without one (`0`, or nothing)
            // the API would begin with the pod as it is now, which is what this loop already has:
            // every event would be this state again, and the loop would spin on it.
            match resource_version.as_deref().filter(|rv| !rv.is_empty()) {
                Some(rv) => match self.wait_for_change(name, rv, deadline).await {
                    Woke::Changed | Woke::Deadline => {}
                    // The API refused the watch, or closed it: the poll is the fallback it always was.
                    Woke::Closed => tokio::time::sleep(settings.ready_poll).await,
                },
                None => tokio::time::sleep(settings.ready_poll).await,
            }
        }
    }

    /// Wait until the pod `name` changes, the deadline passes, or the watch cannot be kept: one
    /// watch on the pod (`metadata.name`) from `rv`, not a read per [`Settings::ready_poll`].
    ///
    /// `rv` is the `resourceVersion` of the last read: the watch then sends what happened after
    /// it, never the state the caller already has. The caller reads the pod again after any event
    /// and decides what to do with it, as it does today.
    async fn wait_for_change(&self, name: &str, rv: &str, deadline: Instant) -> Woke {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Woke::Deadline;
        }
        // The API refuses a watch of 295 s or more; the deadline still bounds the wait.
        let secs = u32::try_from(left.as_secs())
            .unwrap_or(u32::MAX)
            .clamp(1, 294);
        let params = WatchParams::default()
            .fields(&format!("metadata.name={name}"))
            .timeout(secs);
        // A watch that never answers must not outlive `ready_timeout`; the plain read before it is
        // still unbounded (a pre-existing gap, not this wait's).
        let stream = match tokio::time::timeout_at(
            deadline.into(),
            self.inner.pods.watch(&params, rv),
        )
        .await
        {
            Ok(Ok(stream)) => stream,
            Ok(Err(e)) => {
                self.inner.note_watch_problem(name, &adam_error::report(&e));
                return Woke::Closed;
            }
            Err(_) => return Woke::Deadline,
        };
        tokio::pin!(stream);
        let until = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline));
        tokio::pin!(until);
        loop {
            tokio::select! {
                () = &mut until => return Woke::Deadline,
                item = stream.try_next() => match item {
                    Ok(Some(
                        WatchEvent::Added(_) | WatchEvent::Modified(_) | WatchEvent::Deleted(_),
                    )) => return Woke::Changed,
                    // A bookmark only says the watch is alive.
                    Ok(Some(WatchEvent::Bookmark(_))) => continue,
                    // A 410 (the version is gone) or a 5xx: the next read takes a fresh version.
                    Ok(Some(WatchEvent::Error(status))) => {
                        self.inner.note_watch_problem(
                            name,
                            &format!(
                                "the API's watch reported {}: {}",
                                if status.reason.is_empty() {
                                    "an error"
                                } else {
                                    status.reason.as_str()
                                },
                                status.message
                            ),
                        );
                        return Woke::Closed;
                    }
                    Ok(None) => return Woke::Closed,
                    Err(e) => {
                        self.inner.note_watch_problem(name, &adam_error::report(&e));
                        return Woke::Closed;
                    }
                },
            }
        }
    }

    /// The pod is up and lacks a mount the workspace needs: it may be made again only when nothing runs
    /// in it. Otherwise the caller waits (it is not a failure of the run: the command ends).
    ///
    /// A command that was started to outlive its call (`bg-` ids: a development server) does not hold
    /// the pod: it ends with it. A pod whose `adam-exec` has no `active-fg` (made by an older image)
    /// is asked `active` instead, which counts every command, as it always did.
    async fn free_for_new_mount(&self, name: &str, missing: &str) -> Result<(), EnvError> {
        let why =
            format!("the run's workspace has {missing} as it is now, which its pod does not mount");
        self.free_to_replace(name, &why).await
    }

    /// The pod is up and something the run needs now (a mount, another image) is not in it: it may be
    /// made again only when nothing runs in it. Otherwise the caller waits.
    async fn free_to_replace(&self, name: &str, why: &str) -> Result<(), EnvError> {
        let busy = match self.busy_with(name, "active-fg").await {
            Err(_) => self.busy_with(name, "active").await,
            answered => answered,
        };
        match busy {
            Ok(false) => Ok(()),
            Ok(true) => Err(EnvError::Unavailable(format!(
                "{why}, and a command is running in the pod; the pod is made again when it ends"
            ))),
            Err(e) => Err(EnvError::Unavailable(format!(
                "{why}, and the pod cannot be asked whether a command is running: {e}"
            ))),
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
    /// still alive: a long build) is never idle, however long ago the command was
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
                    inner.forget_pod(&run);
                    deleted.push(run);
                }
                Err(e) => {
                    tracing::warn!(%run, error = %adam_error::report(&e), "cannot delete an idle run pod");
                }
            }
        }
        Ok(deleted)
    }

    /// Whether a command is alive in the pod, one that outlives its call included.
    async fn busy(&self, pod: &str) -> Result<bool, EnvError> {
        self.busy_with(pod, "active").await
    }

    /// Whether `adam-exec <question>` (`active`, or `active-fg`, which leaves out the commands that
    /// outlive their call) counts a command in the pod.
    async fn busy_with(&self, pod: &str, question: &str) -> Result<bool, EnvError> {
        let settings = &self.inner.settings;
        let argv = vec![settings.adam_exec.clone(), question.to_owned()];
        let answer = self
            .inner
            .exec
            .capture(pod, argv, settings.exec_timeout)
            .await
            .map_err(|e| EnvError::Unavailable(e.to_string()))?;
        match (&answer.status, answer.stdout.trim().parse::<u64>()) {
            (ExitStatus::Code(0), Ok(n)) => Ok(n > 0),
            _ => Err(EnvError::Unavailable(format!(
                "adam-exec {question} answered ({}) {}",
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
        let mounts = self.inner.template.mounts_for(workspace).await?;
        let pod = self.ready_pod(&run, &name, &mounts, progress).await?;
        let image = annotation(&pod, REPOSITORY_IMAGE_ANNOTATION).map(str::to_owned);
        self.inner.touch(&run);
        Ok(Arc::new(KubeSession {
            inner: Arc::clone(&self.inner),
            run,
            pod: name,
            image,
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

    /// Record the image the run chose (`use_repository_environment`, ADR 0052): the next `ensure`
    /// makes the pod again when the pod that is there runs another one. The pod's annotation is the
    /// record, so a coder that restarts reads the choice back from it; this process remembers it
    /// until then.
    async fn set_image(&self, run: &str, image: Option<&str>) -> Result<(), EnvError> {
        if image.is_some_and(|image| image.trim().is_empty()) {
            return Err(EnvError::Refused("the image is empty".to_owned()));
        }
        let image = image.map(|image| image.trim().to_owned());
        // The lock of `ensure` and `release`: a pod that is being made is not compared with a
        // choice that changed under it.
        let lock = self.inner.lock_of(run);
        let _held = lock.lock().await;
        self.inner.remember_image(run, image);
        Ok(())
    }

    /// The prefixes the deployment allows a repository image to start with
    /// (`RUN_POD_REPOSITORY_IMAGES`): `Some`, empty when it allows none, because a run pod can move
    /// to another image at all.
    fn repository_image_prefixes(&self) -> Option<&[String]> {
        Some(self.inner.settings.repository_images.as_slice())
    }

    /// Delete the run's pod when it lacks a mount the workspace needs now (a slot added since it was
    /// made) and no command runs in it; the next `ensure` makes a new one. The files are on the volume,
    /// so only what is running in the pod is lost, and nothing is. While a command runs, or when the
    /// pod cannot be asked, this is [`EnvError::Unavailable`], as it is for `ensure`.
    ///
    /// It never makes a pod: a run with no pod, a pod that is going away or is over, a pod that has
    /// every mount, and a pod of another run (the names of two runs collided; `ensure` refuses that) are
    /// left as they are, and the run's directory is not made for a pod that is not there.
    async fn seal(&self, workspace: &RunWorkspace) -> Result<(), EnvError> {
        let run = workspace.run();
        let name = pod_name(run);
        // The lock of `ensure` and `release`: a pod that is being made is not looked at half made.
        let lock = self.inner.lock_of(run);
        let _held = lock.lock().await;
        let found = self
            .inner
            .pods
            .get_opt(&name)
            .await
            .map_err(|e| env_error(&e, "looking for the run pod"))?;
        let Some(pod) = found else {
            return Ok(());
        };
        if annotation(&pod, RUN_ID_ANNOTATION) != Some(run)
            || pod.metadata.deletion_timestamp.is_some()
            || ended(&pod)
        {
            return Ok(());
        }
        let mounts = self.inner.template.mounts_for(workspace).await?;
        let Some(mount) = mounts.missing_from(annotation(&pod, MOUNTS_ANNOTATION)) else {
            return Ok(());
        };
        let missing = mount.path().display().to_string();
        if is_ready(&pod) {
            self.free_for_new_mount(&name, &missing).await?;
        }
        self.inner.delete(&name).await?;
        tracing::info!(%run, pod = %name, mount = %missing, "deleted a run pod that lacks a mount of the workspace; the next command makes another");
        Ok(())
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
