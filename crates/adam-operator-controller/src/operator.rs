//! The two controllers, wired to their triggers.
//!
//! The `AgentService` controller is triggered by
//!
//! * its own object (kube-rs's `Controller`, which also owns the reflector the directory reads);
//! * a change of an `AgentConfig`, mapped to every service in that namespace that names it;
//! * [`RuntimeProvider::watch`]: the ids of runtimes that changed, mapped to the service of the same
//!   scope and name, so the controller never watches a workload itself;
//! * a timer per pass ([`Resync`](crate::Resync)), because a signal is never the truth.
//!
//! Not a trigger: a change of a referenced Secret. The operator has no right on Secrets, so
//! it cannot watch them; the kubelet's word about a pod is how a missing one is learned, and that
//! arrives through the runtime's watch.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use adam_operator_api::{AgentConfig, AgentService};
use adam_operator_ports::{RuntimeProvider, StoreProvisioner};
use futures::StreamExt;
use kube::runtime::controller::Config as ControllerConfig;
use kube::runtime::reflector::{ObjectRef, Store};
use kube::runtime::{Controller, watcher};
use kube::{Api, Client, ResourceExt};
use tokio::sync::watch;

use crate::config::{config_error_policy, reconcile_config};
use crate::context::{ConfigContext, Context, OwnerOf};
use crate::directory::ReflectorDirectory;
use crate::metrics::{Metrics, name};
use crate::options::Options;
use crate::service::{reconcile_service, service_error_policy};

/// A handle that says whether the controllers have listed the cluster. `/readyz` reads it.
#[derive(Clone, Debug, Default)]
pub struct Readiness(Arc<AtomicBool>);

impl Readiness {
    /// Both reflectors have synced.
    pub fn is_ready(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    /// Say it is. [`Operator::run`] does when both caches have synced; a test of something that
    /// reads the handle can do it by hand.
    pub fn mark_ready(&self) {
        self.0.store(true, Ordering::Release);
    }
}

/// The controllers of the v0 operator, over a runtime provider and a store provisioner.
pub struct Operator<R, S> {
    client: Client,
    runtime: R,
    store: S,
    owner: OwnerOf,
    options: Options,
    metrics: Arc<Metrics>,
    readiness: Readiness,
    services: Option<Controller<AgentService>>,
}

impl<R, S> Operator<R, S>
where
    R: RuntimeProvider + 'static,
    S: StoreProvisioner + 'static,
{
    /// Compose the controllers. Nothing runs, and nothing is asked of the cluster, until
    /// [`run`](Self::run).
    pub fn new(client: Client, runtime: R, store: S, owner: OwnerOf, options: Options) -> Self {
        let services = Controller::new(
            api::<AgentService>(&client, &options),
            watcher::Config::default(),
        );
        Self {
            client,
            runtime,
            store,
            owner,
            options,
            metrics: Arc::new(Metrics::new()),
            readiness: Readiness::default(),
            services: Some(services),
        }
    }

    /// The counters, for `/metrics`.
    pub fn metrics(&self) -> Arc<Metrics> {
        self.metrics.clone()
    }

    /// Whether the controllers have synced, for `/readyz`.
    pub fn readiness(&self) -> Readiness {
        self.readiness.clone()
    }

    /// The agents of the cluster as the services controller's cache has them: the
    /// [`AgentDirectory`](adam_operator_ports::AgentDirectory) of the registry.
    pub fn directory(&self) -> ReflectorDirectory {
        ReflectorDirectory::new(self.reader())
    }

    fn reader(&self) -> Store<AgentService> {
        // `services` is only taken by `run`, which consumes `self`.
        self.services
            .as_ref()
            .map(Controller::store)
            .unwrap_or_else(|| kube::runtime::reflector::store().0)
    }

    /// Run both controllers until `shutdown` resolves, then let the passes in flight finish.
    pub async fn run(mut self, shutdown: impl Future<Output = ()> + Send + 'static) {
        let (stop, stopped) = watch::channel(false);
        tokio::spawn(async move {
            shutdown.await;
            // Every receiver may be gone already: nothing is left to stop.
            let _ = stop.send(true);
        });
        let signal = |mut rx: watch::Receiver<bool>| async move {
            // A closed channel is a stopped one.
            let _ = rx.wait_for(|stop| *stop).await;
        };

        let metrics = self.metrics.clone();
        let namespace = self.options.watch_namespace.clone();
        let cfg = ControllerConfig::default().concurrency(self.options.concurrency);

        // The services.
        let Some(services) = self.services.take() else {
            return;
        };
        let reader = services.store();
        let ctx = Arc::new(
            Context::new(
                self.client.clone(),
                self.runtime,
                self.store,
                self.owner,
                self.options.clone(),
            )
            .with_metrics(metrics.clone()),
        );
        let signals = {
            let metrics = metrics.clone();
            let namespace = namespace.clone();
            ctx.runtime
                .watch()
                .filter(move |id| {
                    metrics.inc(name::RUNTIME_SIGNALS, &[]);
                    futures::future::ready(namespace.as_deref().is_none_or(|ns| ns == id.scope()))
                })
                .map(|id| ObjectRef::<AgentService>::new(id.name()).within(id.scope()))
        };
        let mapper_reader = reader.clone();
        let services = services
            .with_config(cfg.clone())
            .graceful_shutdown_on(signal(stopped.clone()))
            .watches(
                api::<AgentConfig>(&self.client, &self.options),
                watcher::Config::default(),
                move |config| services_of(&mapper_reader, &config),
            )
            .reconcile_on(signals)
            .run(reconcile_service, service_error_policy, ctx)
            .for_each(|result| async move { log_plumbing("agentservice", result) });

        // The configs.
        let config_ctx = Arc::new(ConfigContext::new(self.client.clone(), metrics.clone()));
        let configs_controller = Controller::new(
            api::<AgentConfig>(&self.client, &self.options),
            watcher::Config::default(),
        )
        .with_config(cfg)
        .graceful_shutdown_on(signal(stopped));
        let config_reader = configs_controller.store();
        let configs = configs_controller
            .run(reconcile_config, config_error_policy, config_ctx)
            .for_each(|result| async move { log_plumbing("agentconfig", result) });

        // Ready once both caches have listed the cluster.
        let ready = self.readiness.clone();
        let synced = async move {
            if reader.wait_until_ready().await.is_ok()
                && config_reader.wait_until_ready().await.is_ok()
            {
                ready.mark_ready();
                tracing::info!("the caches are synced");
            }
        };
        futures::join!(services, configs, synced);
    }
}

/// What a controller's stream says besides a finished pass. A pass's own failure was logged by the error
/// policy; a request for an object that is gone is expected. A watch that breaks is not silent: with the CRDs
/// not installed, or no right to list them, that is all an operator would ever say.
fn log_plumbing<T>(
    controller: &str,
    result: Result<T, kube::runtime::controller::Error<crate::Error, watcher::Error>>,
) {
    use kube::runtime::controller::Error as Plumbing;
    match result {
        Ok(_) | Err(Plumbing::ReconcilerFailed(..)) => {}
        Err(Plumbing::ObjectNotFound(object)) => tracing::debug!(controller, "{object} is gone"),
        Err(e @ (Plumbing::QueueError(_) | Plumbing::RunnerError(_))) => {
            tracing::warn!(
                controller,
                "{e}: {}",
                std::error::Error::source(&e).map_or_else(String::new, ToString::to_string)
            );
        }
    }
}

fn api<K>(client: &Client, options: &Options) -> Api<K>
where
    K: kube::Resource<Scope = kube::core::NamespaceResourceScope, DynamicType = ()>
        + Clone
        + serde::de::DeserializeOwned
        + std::fmt::Debug,
{
    match &options.watch_namespace {
        Some(ns) => Api::namespaced(client.clone(), ns),
        None => Api::all(client.clone()),
    }
}

/// The services of a namespace that run `config`.
fn services_of(
    services: &Store<AgentService>,
    config: &AgentConfig,
) -> Vec<ObjectRef<AgentService>> {
    let (Some(ns), name) = (config.namespace(), config.name_any()) else {
        return Vec::new();
    };
    services
        .state()
        .iter()
        .filter(|s| s.namespace().as_deref() == Some(ns.as_str()) && s.spec.config_ref.name == name)
        .map(|s| ObjectRef::from_obj(&**s))
        .collect()
}
