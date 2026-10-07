//! `adam-operator run`: the composition root. The controllers are generic over a runtime provider and a
//! store provisioner; this is the one place that names the types.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Result;
use clap::Args;

/// Settings of `run`. Every one is also an environment variable, which is how the chart sets them.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// Watch one namespace (a namespaced operator). Empty or unset: every namespace.
    #[arg(long, env = "WATCH_NAMESPACE")]
    pub watch_namespace: Option<String>,

    /// Where `/healthz` and `/readyz` are served.
    #[arg(long, env = "HEALTH_ADDR", default_value = "0.0.0.0:8081")]
    pub health_addr: SocketAddr,

    /// Where `/metrics` is served.
    #[arg(long, env = "METRICS_ADDR", default_value = "0.0.0.0:9090")]
    pub metrics_addr: SocketAddr,

    /// The name of this pod, for the `reportingInstance` of the Events.
    #[arg(long, env = "POD_NAME")]
    pub instance: Option<String>,

    /// Services reconciled at the same time.
    #[arg(long, env = "ADAM_OPERATOR_CONCURRENCY", default_value_t = 4)]
    pub concurrency: u16,

    /// Where the agent registry (`GET /registry/v1/agents`) is served. Only with the `registry` feature
    /// and a token.
    #[arg(long, env = "REGISTRY_ADDR", default_value = "0.0.0.0:8080")]
    pub registry_addr: SocketAddr,

    /// The file that holds the registry's bearer token (a mounted Secret). **No token, no registry**: with
    /// this unset, or the file missing or empty, nothing is served on the registry's port and every service
    /// is `Listed: False`, reason `RegistryDisabled` (fail closed). Read once at start.
    #[arg(long, env = "REGISTRY_TOKEN_FILE")]
    pub registry_token_file: Option<PathBuf>,

    /// The URL of the registry document itself, sent as its `anchor` (the contract asks a server to; a
    /// client ignores it). Unset: no `anchor`.
    #[arg(long, env = "REGISTRY_PUBLIC_URL")]
    pub registry_public_url: Option<String>,

    /// Seconds between looks at a service that is Ready or Suspended (the timer that makes a lost
    /// signal harmless).
    #[arg(long, env = "ADAM_OPERATOR_RESYNC_SECS", default_value_t = 300)]
    pub resync_secs: u64,

    /// Seconds between looks at a service that is rolling out, unwell, or held by a foreign object.
    #[arg(long, env = "ADAM_OPERATOR_RESYNC_PENDING_SECS", default_value_t = 15)]
    pub resync_pending_secs: u64,
}

// What `run` composes needs a runtime provider; without one the settings are parsed and unused.
#[cfg(feature = "runtime-kubernetes")]
impl RunArgs {
    /// An empty `WATCH_NAMESPACE` (a chart value left blank) means no namespace.
    fn namespace(&self) -> Option<String> {
        self.watch_namespace
            .clone()
            .filter(|n| !n.trim().is_empty())
    }

    fn options(
        &self,
        registry: bool,
        registry_full: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> adam_operator_controller::Options {
        adam_operator_controller::Options {
            watch_namespace: self.namespace(),
            registry: if registry {
                adam_operator_controller::RegistryMode::Enabled
            } else {
                adam_operator_controller::RegistryMode::Disabled
            },
            registry_full,
            resync: adam_operator_controller::Resync {
                settled: std::time::Duration::from_secs(self.resync_secs),
                pending: std::time::Duration::from_secs(self.resync_pending_secs),
                ..adam_operator_controller::Resync::default()
            },
            concurrency: self.concurrency.max(1),
            instance: self.instance.clone().filter(|i| !i.is_empty()),
        }
    }
}

/// The store provisioner of this build: with `store-cnpg`, the one that makes a CloudNativePG cluster and
/// also serves a referenced Secret (it passes the whole suite of `adam-operator-ports`, so the controller sees one
/// type); without it, referenced Secrets alone, and a cluster is refused as not installed.
#[cfg(all(feature = "runtime-kubernetes", feature = "store-cnpg"))]
fn store(client: &kube::Client) -> adam_operator_store_cnpg::CnpgStore {
    adam_operator_store_cnpg::CnpgStore::new(client.clone())
}

#[cfg(all(feature = "runtime-kubernetes", not(feature = "store-cnpg")))]
fn store(_client: &kube::Client) -> adam_operator_store_secret::SecretStore {
    adam_operator_store_secret::SecretStore::new()
}

/// The registry's token, or why there is none. **No token, no registry** (fail closed): the operator
/// keeps reconciling, says so loudly, and every service is `Listed: False`, reason `RegistryDisabled`.
#[cfg(all(feature = "runtime-kubernetes", feature = "registry"))]
fn registry_token(args: &RunArgs) -> Option<adam_operator_registry::Token> {
    let Some(path) = &args.registry_token_file else {
        tracing::warn!(
            "no REGISTRY_TOKEN_FILE: the agent registry is not served (it has no token to demand)"
        );
        return None;
    };
    match adam_operator_registry::Token::from_file(path) {
        Ok(token) => Some(token),
        Err(e) => {
            tracing::error!(
                "{}: {e}: the agent registry is not served (it has no token to demand)",
                path.display()
            );
            None
        }
    }
}

#[cfg(all(feature = "runtime-kubernetes", not(feature = "registry")))]
fn registry_token(args: &RunArgs) -> Option<()> {
    if args.registry_token_file.is_some() {
        tracing::warn!(
            "REGISTRY_TOKEN_FILE is set, but this build has no `registry` feature: nothing is served on the registry's port"
        );
    }
    None
}

/// How often the registry's document is built when nobody asks, so that `RegistryFull` is true to the
/// directory and not to the last request.
#[cfg(all(feature = "runtime-kubernetes", feature = "registry"))]
const REGISTRY_REFRESH: std::time::Duration = std::time::Duration::from_secs(15);

/// Resolves on SIGTERM or SIGINT.
#[cfg(feature = "runtime-kubernetes")]
async fn termination() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
                return;
            }
            Err(e) => tracing::warn!("no SIGTERM handler: {e}"),
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(feature = "runtime-kubernetes")]
pub async fn run(args: RunArgs) -> Result<()> {
    use std::sync::Arc;

    use adam_operator_api::AgentService;
    use adam_operator_controller::{Operator, OwnerOf};
    use adam_operator_runtime_kubernetes::{KubernetesRuntime, owner_handle};
    use anyhow::Context;
    use kube::{Resource, ResourceExt};
    use tokio::sync::watch;

    let registry_full = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let token = registry_token(&args);
    let options = args.options(token.is_some(), registry_full.clone());
    tracing::info!(
        namespace = options.watch_namespace.as_deref().unwrap_or("(all)"),
        "starting the operator {}",
        env!("CARGO_PKG_VERSION")
    );

    // Everything below is the composition: a Kubernetes runtime, the store, the controllers.
    adam_operator_runtime_kubernetes::install_crypto_provider();
    let client = kube::Client::try_default()
        .await
        .context("connecting to the cluster (in-cluster configuration, or KUBECONFIG)")?;
    let mut runtime = KubernetesRuntime::new(client.clone());
    if let Some(ns) = &options.watch_namespace {
        runtime = runtime.watching_namespace(ns);
    }
    // The encoding of an owner is the runtime provider's, so this is where it is made (the
    // controller never looks inside a handle).
    let owner: OwnerOf = Arc::new(|svc: &AgentService| {
        owner_handle(
            AgentService::api_version(&()),
            AgentService::kind(&()),
            svc.name_any(),
            svc.metadata.uid.clone().unwrap_or_default(),
        )
    });
    let store = store(&client);
    let operator = Operator::new(client, runtime, store, owner, options);

    #[cfg(feature = "registry")]
    let registry_server = token.map(|token| {
        let mut registry = adam_operator_registry::Registry::new(operator.directory(), token)
            .with_full_flag(registry_full.clone());
        if let Some(anchor) = args.registry_public_url.clone().filter(|u| !u.is_empty()) {
            registry = registry.with_anchor(anchor);
        }
        Arc::new(registry)
    });

    let (stop, stopped) = watch::channel(false);
    let signalled = |mut rx: watch::Receiver<bool>| async move {
        // A dropped sender is a stop as well.
        let _ = rx.wait_for(|stop| *stop).await;
    };
    let health = tokio::spawn(crate::serve::serve(
        args.health_addr,
        crate::serve::health(operator.readiness()),
        signalled(stopped.clone()),
    ));
    let metrics = tokio::spawn(crate::serve::serve(
        args.metrics_addr,
        crate::serve::metrics(operator.metrics()),
        signalled(stopped.clone()),
    ));

    // The registry: served on its port, and its document built on a timer besides, so that the flag the
    // controller reads (`RegistryFull`) follows the directory when nobody asks.
    #[cfg(feature = "registry")]
    let registry = tokio::spawn({
        let registry = registry_server.clone();
        let addr = args.registry_addr;
        let stopped = stopped.clone();
        async move {
            let Some(registry) = registry else {
                // Not served: the select below must not end on this arm.
                return std::future::pending::<Result<()>>().await;
            };
            let refresh = {
                let registry = registry.clone();
                tokio::spawn(async move {
                    loop {
                        // The result is the flag's and the log's: nobody is waiting for it.
                        let _ = registry.document().await;
                        tokio::time::sleep(REGISTRY_REFRESH).await;
                    }
                })
            };
            let served = crate::serve::serve(addr, registry.router(), signalled(stopped)).await;
            refresh.abort();
            served
        }
    });
    #[cfg(not(feature = "registry"))]
    let registry = tokio::spawn(std::future::pending::<Result<()>>());

    let asked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let asked_by_signal = asked.clone();
    let controllers = operator.run(async move {
        termination().await;
        asked_by_signal.store(true, std::sync::atomic::Ordering::Release);
        tracing::info!("shutting down");
    });
    tokio::pin!(controllers);

    // The servers fail fast (a port in use is a deployment mistake); the controllers end when told to.
    let result = tokio::select! {
        () = &mut controllers => {
            if asked.load(std::sync::atomic::Ordering::Acquire) {
                Ok(())
            } else {
                Err(anyhow::anyhow!("the controllers stopped without being asked to"))
            }
        }
        r = health => r.context("the health server task").and_then(|r| r).and(Err(anyhow::anyhow!("the health server stopped"))),
        r = metrics => r.context("the metrics server task").and_then(|r| r).and(Err(anyhow::anyhow!("the metrics server stopped"))),
        r = registry => r.context("the registry task").and_then(|r| r).and(Err(anyhow::anyhow!("the registry stopped"))),
    };
    // Stop whatever still serves.
    let _ = stop.send(true);
    result
}

#[cfg(not(feature = "runtime-kubernetes"))]
pub async fn run(args: RunArgs) -> Result<()> {
    let _ = args;
    anyhow::bail!(
        "this build has no runtime provider: rebuild with `--features runtime-kubernetes` (the default)"
    )
}
