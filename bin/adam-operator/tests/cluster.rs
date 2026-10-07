//! The operator binary against a real cluster.
//!
//! Each case runs `adam-operator run` (the binary Cargo built for this test) against the cluster of
//! `ADAM_OPERATOR_TEST_KUBECONFIG`, in a namespace of its own, with its own Secrets and objects named for the case:
//! cases run in parallel and share nothing. They apply an `AgentConfig` and an `AgentService`, and check what
//! the cluster ends up with: the workload, the Service, the ConfigMap, the status the operator writes, the
//! Events, and that deleting the service completes its finalizer.
//!
//! **No real adam image is needed.** The pods run `stub/Dockerfile`: busybox that answers `/healthz` on 8080,
//! with `adam-coder` as its entrypoint and `tini` and `adam-agent` where the operator expects them. CI builds
//! it and loads it into kind (`ADAM_OPERATOR_TEST_STUB_IMAGE`, default `adam-operator-stub:ci`).
//!
//! **Opt in.** Without `ADAM_OPERATOR_TEST_KUBECONFIG` the cases skip, and with `ADAM_OPERATOR_TEST_REQUIRE_CLUSTER=1` (CI) a skip
//! is a failure. The cases create namespaces `adam-operator-e2e-*` and leave them: use a cluster you can throw away.
//!
//! ```sh
//! kind create cluster --name adam-operator
//! docker build -t adam-operator-stub:ci bin/adam-operator/tests/stub && kind load docker-image adam-operator-stub:ci --name adam-operator
//! ADAM_OPERATOR_TEST_KUBECONFIG=$HOME/.kube/config cargo test -p adam-operator --test cluster
//! ```
//!
//! `ADAM_OPERATOR_TEST_NO_WORKLOADS=1` is for a cluster that is only an API server and etcd (no controller manager, no
//! kubelet): there nothing ever rolls out by itself, so the test plays the controller manager, patching the
//! status of every Deployment and StatefulSet to "rolled out", and skips the cases that need a kubelet (a missing
//! Secret, a claim made by a StatefulSet). It proves the operator against a real API server and nothing more.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::future::Future;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use adam_operator_api::AgentService;
use k8s_openapi::api::apps::v1::{Deployment, StatefulSet};
use k8s_openapi::api::core::v1::{
    ConfigMap, Namespace, PersistentVolumeClaim, Pod, Secret, Service,
};
use k8s_openapi::api::events::v1::Event;
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::api::{DeleteParams, ListParams, LogParams, Patch, PatchParams, PostParams};
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::{Api, Client, Config};
use serde_json::{Value, json};

const KUBECONFIG_VAR: &str = "ADAM_OPERATOR_TEST_KUBECONFIG";
const REQUIRE_VAR: &str = "ADAM_OPERATOR_TEST_REQUIRE_CLUSTER";
const IMAGE_VAR: &str = "ADAM_OPERATOR_TEST_STUB_IMAGE";
const NO_WORKLOADS_VAR: &str = "ADAM_OPERATOR_TEST_NO_WORKLOADS";
/// The address of this machine as the pods of the cluster reach it (kind: the gateway of the `kind` docker
/// network). Set, the registry case also reads the registry, and the card it lists, from a pod.
const HOST_ADDR_VAR: &str = "ADAM_OPERATOR_TEST_HOST_ADDR";
const FINALIZER: &str = "agents.vymalo.com/runtime";

// ---------------------------------------------------------------- the manifests

/// The folder agent: `adam-agent` with its folder inline, so a Deployment, a Service and a ConfigMap.
fn chat(name: &str, image: &str) -> (Value, Value) {
    (
        service(name, name),
        json!({
            "apiVersion": "agents.vymalo.com/v1alpha1", "kind": "AgentConfig",
            "metadata": {"name": name},
            "spec": {
                "harness": {"type": "adam-rs", "adam": {
                    "binary": "adam-agent",
                    "agent": {"folder": {"files": {
                        "instructions.md": format!("---\nname: {name}\ndescription: A stand-in.\ncard:\n  name: {name}\n---\nYou are {name}.\n")
                    }}},
                }},
                "model": model(name),
                "environment": environment(image),
                "security": {"runAsUser": 10001, "runAsGroup": 10001, "fsGroup": 10001},
            },
        }),
    )
}

/// The coder: `adam-coder`, a token, no sidecar, and one claim per replica, so a StatefulSet.
fn coder(name: &str, image: &str, policy: &str) -> (Value, Value) {
    let mut svc = service(name, name);
    svc["spec"]["deletionPolicy"] = json!(policy);
    let mut env = environment(image);
    env["volumes"] = json!([{
        "name": "work", "scope": "agent", "mountPath": "/work",
        "source": {"persistent": {"size": "1Gi", "perReplica": true}}
    }]);
    (
        svc,
        json!({
            "apiVersion": "agents.vymalo.com/v1alpha1", "kind": "AgentConfig",
            "metadata": {"name": name},
            "spec": {
                "harness": {"type": "adam-rs", "adam": {
                    "binary": "adam-coder",
                    "agent": {"embedded": {}},
                    "coder": {
                        "workers": 1,
                        "allowedRepoHosts": ["github.com"],
                        "githubApiUrl": "https://api.github.com",
                        "prDraft": true,
                        "gitAuthor": {"name": "stub", "email": "stub@example.invalid"},
                        "opencodeModel": "coding-model",
                        "github": {"token": {"secretRef": {"name": format!("{name}-secrets"), "key": "GITHUB_TOKEN"}}},
                    },
                }},
                "model": model(name),
                "tools": {"githubMcp": {"sidecar": false}},
                "environment": env,
                "security": {"runAsUser": 10001, "runAsGroup": 10001, "fsGroup": 10001},
            },
        }),
    )
}

fn service(name: &str, config: &str) -> Value {
    json!({
        "apiVersion": "agents.vymalo.com/v1alpha1", "kind": "AgentService",
        "metadata": {"name": name},
        "spec": {
            "description": "An end-to-end stand-in.",
            "configRef": {"name": config},
            "interfaces": {"a2a": {"enabled": true, "bearerTokensSecretRef": {"name": format!("{name}-secrets"), "key": "A2A_BEARER_TOKENS"}}},
            "scaling": {"topology": "combined", "workers": 1},
            "store": {"postgres": {"secretRef": {"name": format!("{name}-db"), "key": "uri"}}},
            "registry": {"title": name, "tags": ["e2e"]},
        },
    })
}

fn model(name: &str) -> Value {
    json!({
        "model": "stub-model",
        "baseUrl": {"value": "http://model.invalid/v1"},
        "apiKeySecretRef": {"name": format!("{name}-secrets"), "key": "MODEL_API_KEY"},
    })
}

fn environment(image: &str) -> Value {
    json!({
        "image": {"ref": image},
        "resources": {"requests": {"cpu": "10m", "memory": "16Mi"}, "limits": {"memory": "64Mi"}},
        "terminationGracePeriodSeconds": 1,
    })
}

/// What the manifests put in their Secrets, by name: the keys the pods read.
fn secrets_of(name: &str) -> [(String, Vec<&'static str>); 2] {
    [
        (
            format!("{name}-secrets"),
            vec!["A2A_BEARER_TOKENS", "MODEL_API_KEY", "GITHUB_TOKEN"],
        ),
        (format!("{name}-db"), vec!["uri"]),
    ]
}

/// The manifests above are valid, and resolve: run without a cluster, so a broken fixture is not found by CI
/// minutes into a kind job.
#[test]
fn the_manifests_are_valid_and_resolve() {
    let namespace = "ns";
    for (svc, cfg) in [
        chat("chat", "adam-operator-stub:ci"),
        coder("coder", "adam-operator-stub:ci", "Retain"),
        coder("coder", "adam-operator-stub:ci", "Delete"),
    ] {
        let mut svc = svc;
        let mut cfg = cfg;
        svc["metadata"]["namespace"] = json!(namespace);
        cfg["metadata"]["namespace"] = json!(namespace);
        let svc: AgentService = serde_json::from_value(svc).unwrap();
        let cfg: adam_operator_api::AgentConfig = serde_json::from_value(cfg).unwrap();
        adam_operator_domain::resolve(&svc, &cfg, adam_operator_ports::OwnerHandle::none())
            .unwrap_or_else(|issues| panic!("{issues:?}"));
    }
}

// -------------------------------------------------------------------- the cluster

struct Cluster {
    client: Client,
    kubeconfig: String,
    image: String,
    no_workloads: bool,
}

fn required() -> bool {
    matches!(std::env::var(REQUIRE_VAR).as_deref(), Ok("1" | "true"))
}

async fn connect() -> Option<Cluster> {
    let Some(path) = std::env::var(KUBECONFIG_VAR).ok().filter(|p| !p.is_empty()) else {
        assert!(
            !required(),
            "{KUBECONFIG_VAR} is not set, but {REQUIRE_VAR}=1 forbids skipping"
        );
        eprintln!("skipped: {KUBECONFIG_VAR} is not set");
        return None;
    };
    adam_operator_runtime_kubernetes::install_crypto_provider();
    let kubeconfig = Kubeconfig::read_from(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let config = Config::from_custom_kubeconfig(kubeconfig, &KubeConfigOptions::default())
        .await
        .unwrap_or_else(|e| panic!("{path}: {e}"));
    let client = Client::try_from(config).unwrap_or_else(|e| panic!("{path}: {e}"));
    client
        .apiserver_version()
        .await
        .unwrap_or_else(|e| panic!("the cluster of {path} does not answer: {e}"));
    install_crds(&client).await;
    Some(Cluster {
        client,
        kubeconfig: path,
        image: std::env::var(IMAGE_VAR).unwrap_or_else(|_| "adam-operator-stub:ci".to_owned()),
        no_workloads: matches!(std::env::var(NO_WORKLOADS_VAR).as_deref(), Ok("1" | "true")),
    })
}

/// Install the CRDs the way a deployment would (`crdgen` is what `deploy/crds` holds, byte for byte: the
/// `crds` job and `tests/cli.rs` say so), and wait until the API serves them.
async fn install_crds(client: &Client) {
    let crds: Api<CustomResourceDefinition> = Api::all(client.clone());
    for crd in adam_operator_api::crds() {
        let name = crd.metadata.name.clone().unwrap();
        crds.patch(
            &name,
            &PatchParams::apply("adam-operator-e2e").force(),
            &Patch::Apply(&crd),
        )
        .await
        .unwrap_or_else(|e| panic!("installing {name}: {e}"));
        poll(&format!("{name} to be Established"), 60, || async {
            let crd = crds.get(&name).await.ok()?;
            crd.status?
                .conditions?
                .iter()
                .any(|c| c.type_ == "Established" && c.status == "True")
                .then_some(())
        })
        .await;
    }
}

/// Poll `check` every half second until it gives something, or panic after `secs` saying what was awaited.
async fn poll<T, F, Fut>(what: &str, secs: u64, mut check: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let until = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(v) = check().await {
            return v;
        }
        assert!(
            Instant::now() < until,
            "timed out after {secs}s waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// The operator process.
struct OperatorProc {
    child: Child,
    log: std::path::PathBuf,
    health: u16,
    metrics: u16,
    registry: u16,
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

impl OperatorProc {
    /// `registry_token`: the token the registry demands, written to a file the operator reads. `None`: no
    /// `REGISTRY_TOKEN_FILE`, so no registry (fail closed), which is what most cases run with.
    fn spawn(cluster: &Cluster, namespace: &str, registry_token: Option<&str>) -> Self {
        let log =
            std::env::temp_dir().join(format!("adam-operator-e2e-{namespace}-{}.log", free_port()));
        let (health, metrics, registry) = (free_port(), free_port(), free_port());
        let mut command = Command::new(env!("CARGO_BIN_EXE_adam-operator"));
        if let Some(token) = registry_token {
            let file = std::env::temp_dir().join(format!("adam-operator-e2e-{namespace}.token"));
            // A mounted Secret ends its value with a newline as often as not.
            std::fs::write(&file, format!("{token}\n")).unwrap();
            command.env("REGISTRY_TOKEN_FILE", file);
        }
        let child = command
            .arg("run")
            .env("KUBECONFIG", &cluster.kubeconfig)
            .env("WATCH_NAMESPACE", namespace)
            .env("HEALTH_ADDR", format!("127.0.0.1:{health}"))
            .env("METRICS_ADDR", format!("127.0.0.1:{metrics}"))
            // Every interface, so that a pod can reach it (the registry case); the port is this case's own.
            .env("REGISTRY_ADDR", format!("0.0.0.0:{registry}"))
            .env("POD_NAME", "operator-under-test")
            // The pending timer is short, so what a signal missed is found soon.
            .env("ADAM_OPERATOR_RESYNC_PENDING_SECS", "5")
            .env("RUST_LOG", "info,kube=warn")
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .expect("starts the operator");
        Self {
            child,
            log,
            health,
            metrics,
            registry,
        }
    }

    /// The process has ended (a crash is a failed test, not a hang).
    fn exited(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().unwrap()
    }

    fn stop(&mut self) {
        // SIGKILL is the unkind end: the finalizer must hold when the operator does not get to say goodbye.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn log(&self) -> String {
        let mut s = String::new();
        if let Ok(mut f) = std::fs::File::open(&self.log) {
            let _ = f.read_to_string(&mut s);
        }
        s
    }
}

impl Drop for OperatorProc {
    fn drop(&mut self) {
        self.stop();
        if std::thread::panicking() {
            eprintln!(
                "---- the operator's log ({}) ----\n{}",
                self.log.display(),
                self.log()
            );
        }
    }
}

/// One case: a namespace of its own, an operator watching only it, and (without workloads) a stand-in for
/// the controller manager.
struct Case<'a> {
    cluster: &'a Cluster,
    ns: String,
    operator: Option<OperatorProc>,
    emulator: Option<tokio::task::JoinHandle<()>>,
}

impl<'a> Case<'a> {
    async fn begin(cluster: &'a Cluster, prefix: &str) -> Case<'a> {
        Self::begin_with(cluster, prefix, None).await
    }

    async fn begin_with(
        cluster: &'a Cluster,
        prefix: &str,
        registry_token: Option<&str>,
    ) -> Case<'a> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let ns = format!("adam-operator-e2e-{prefix}-{:x}", nanos & 0xff_ffff);
        let namespaces: Api<Namespace> = Api::all(cluster.client.clone());
        let namespace: Namespace = serde_json::from_value(json!({
            "apiVersion": "v1", "kind": "Namespace", "metadata": {"name": ns}
        }))
        .unwrap();
        namespaces
            .patch(
                &ns,
                &PatchParams::apply("adam-operator-e2e"),
                &Patch::Apply(&namespace),
            )
            .await
            .unwrap_or_else(|e| panic!("namespace {ns}: {e}"));

        let emulator = cluster
            .no_workloads
            .then(|| tokio::spawn(emulate_workloads(cluster.client.clone(), ns.clone())));
        let operator = Some(OperatorProc::spawn(cluster, &ns, registry_token));
        Case {
            cluster,
            ns,
            operator,
            emulator,
        }
    }

    fn api<K>(&self) -> Api<K>
    where
        K: kube::Resource<Scope = kube::core::NamespaceResourceScope, DynamicType = ()>
            + Clone
            + serde::de::DeserializeOwned
            + std::fmt::Debug,
    {
        Api::namespaced(self.cluster.client.clone(), &self.ns)
    }

    /// Apply a custom resource (or anything with a kind) the way `kubectl apply --server-side` does.
    async fn apply<K>(&self, object: &Value)
    where
        K: kube::Resource<Scope = kube::core::NamespaceResourceScope, DynamicType = ()>
            + Clone
            + serde::de::DeserializeOwned
            + std::fmt::Debug,
    {
        let name = object["metadata"]["name"].as_str().unwrap();
        self.api::<K>()
            .patch(
                name,
                &PatchParams::apply("adam-operator-e2e").force(),
                &Patch::Apply(object),
            )
            .await
            .unwrap_or_else(|e| panic!("applying {name}: {e}"));
    }

    /// The Secrets a pod of `name` reads, with a dummy value (the harness plays the deployment).
    async fn secrets(&self, name: &str) {
        for (secret, keys) in secrets_of(name) {
            let data: serde_json::Map<String, Value> = keys
                .iter()
                .map(|k| ((*k).to_owned(), json!("dummy")))
                .collect();
            let s: Secret = serde_json::from_value(json!({
                "apiVersion": "v1", "kind": "Secret",
                "metadata": {"name": secret}, "stringData": data,
            }))
            .unwrap();
            self.api::<Secret>()
                .patch(
                    &secret,
                    &PatchParams::apply("adam-operator-e2e").force(),
                    &Patch::Apply(&s),
                )
                .await
                .unwrap_or_else(|e| panic!("secret {secret}: {e}"));
        }
    }

    async fn service(&self, name: &str) -> Option<Value> {
        let svc = self.api::<AgentService>().get_opt(name).await.unwrap()?;
        Some(serde_json::to_value(svc).unwrap())
    }

    /// Wait until the service's status satisfies `ok`; panic with the last status it had.
    async fn until(&self, name: &str, what: &str, secs: u64, ok: impl Fn(&Value) -> bool) -> Value {
        let mut last = Value::Null;
        let until = Instant::now() + Duration::from_secs(secs);
        loop {
            if let Some(svc) = self.service(name).await {
                if ok(&svc["status"]) {
                    return svc;
                }
                last = svc["status"].clone();
            }
            assert!(
                Instant::now() < until,
                "timed out after {secs}s waiting for {what}; the last status was {last:#}"
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn gone<K>(&self, name: &str, secs: u64)
    where
        K: kube::Resource<Scope = kube::core::NamespaceResourceScope, DynamicType = ()>
            + Clone
            + serde::de::DeserializeOwned
            + std::fmt::Debug,
    {
        let api = self.api::<K>();
        poll(&format!("{name} to be gone"), secs, || async {
            api.get_opt(name).await.unwrap().is_none().then_some(())
        })
        .await;
    }

    async fn finish(mut self) {
        if let Some(e) = self.emulator.take() {
            e.abort();
        }
        if let Some(mut op) = self.operator.take() {
            assert!(
                op.exited().is_none(),
                "the operator died during the case:\n{}",
                op.log()
            );
        }
        // Best effort: the cluster is a throwaway one, and some have no controller to finish a namespace.
        let _ = Api::<Namespace>::all(self.cluster.client.clone())
            .delete(&self.ns, &DeleteParams::default())
            .await;
    }
}

/// A cluster that is only an API server has no controller manager: say every Deployment and StatefulSet
/// is rolled out, as the real one would once the pods are ready. (Pods themselves are not made.)
async fn emulate_workloads(client: Client, ns: String) {
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), &ns);
    let sets: Api<StatefulSet> = Api::namespaced(client, &ns);
    loop {
        for d in deployments
            .list(&ListParams::default())
            .await
            .map(|l| l.items)
            .unwrap_or_default()
        {
            let n = d.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
            let patch = json!({"status": {
                "observedGeneration": d.metadata.generation, "replicas": n, "updatedReplicas": n,
                "readyReplicas": n, "availableReplicas": n,
            }});
            let _ = deployments
                .patch_status(
                    &d.metadata.name.clone().unwrap_or_default(),
                    &PatchParams::default(),
                    &Patch::Merge(&patch),
                )
                .await;
        }
        for s in sets
            .list(&ListParams::default())
            .await
            .map(|l| l.items)
            .unwrap_or_default()
        {
            let n = s.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
            let patch = json!({"status": {
                "observedGeneration": s.metadata.generation, "replicas": n, "updatedReplicas": n,
                "readyReplicas": n, "currentRevision": "r1", "updateRevision": "r1",
            }});
            let _ = sets
                .patch_status(
                    &s.metadata.name.clone().unwrap_or_default(),
                    &PatchParams::default(),
                    &Patch::Merge(&patch),
                )
                .await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// `(status, reason)` of a condition of a status.
fn cond(status: &Value, type_: &str) -> Option<(String, String)> {
    let c = status["conditions"]
        .as_array()?
        .iter()
        .find(|c| c["type"] == type_)?;
    Some((
        c["status"].as_str()?.to_owned(),
        c["reason"].as_str()?.to_owned(),
    ))
}

fn is(status: &Value, type_: &str, truth: &str, reason: &str) -> bool {
    cond(status, type_) == Some((truth.to_owned(), reason.to_owned()))
}

fn state(status: &Value) -> &str {
    status["state"].as_str().unwrap_or_default()
}

/// A bare HTTP/1.1 GET: status code and body.
async fn http_get(port: u16, path: &str) -> Option<(u16, String)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .ok()?;
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .ok()?;
    let mut out = String::new();
    stream.read_to_string(&mut out).await.ok()?;
    let code = out.split_whitespace().nth(1)?.parse().ok()?;
    Some((
        code,
        out.split("\r\n\r\n").nth(1).unwrap_or_default().to_owned(),
    ))
}

// ---------------------------------------------------------------------- the cases

#[tokio::test]
async fn a_folder_agent_is_made_becomes_ready_and_its_deletion_completes() {
    let Some(cluster) = connect().await else {
        return;
    };
    let case = Case::begin(&cluster, "chat").await;
    let ns = &case.ns;
    case.secrets("chat").await;
    let (svc, cfg) = chat("chat", &cluster.image);
    case.apply::<adam_operator_api::AgentConfig>(&cfg).await;
    case.apply::<AgentService>(&svc).await;

    // Ready, with every condition the status table has.
    let ready = case
        .until("chat", "the service to be Ready", 240, |s| {
            state(s) == "Ready"
        })
        .await;
    let status = &ready["status"];
    assert!(
        is(status, "ConfigResolved", "True", "Resolved"),
        "{status:#}"
    );
    assert!(
        is(status, "StoreReady", "True", "SecretReferenced"),
        "{status:#}"
    );
    assert!(is(status, "RuntimeReady", "True", "Ready"), "{status:#}");
    assert!(is(status, "Ready", "True", "Reconciled"), "{status:#}");
    assert!(
        is(status, "Listed", "False", "RegistryDisabled"),
        "{status:#}"
    );
    assert_eq!(status["runtime"]["provider"], "kubernetes");
    assert_eq!(status["runtime"]["phase"], "Ready");
    assert_eq!(status["runtime"]["replicas"], 1);
    assert_eq!(
        status["observedGeneration"],
        ready["metadata"]["generation"]
    );
    assert_eq!(
        status["endpoints"]["a2a"],
        format!("http://chat.{ns}.svc:8080/")
    );
    assert_eq!(
        status["endpoints"]["agentCard"],
        format!("http://chat.{ns}.svc:8080/.well-known/agent-card.json")
    );
    assert_eq!(ready["metadata"]["finalizers"], json!([FINALIZER]));

    // What the cluster was given: a Deployment (no volume), its Service and the folder's ConfigMap.
    let deployment = case.api::<Deployment>().get("chat").await.unwrap();
    let labels = deployment.metadata.labels.clone().unwrap();
    assert_eq!(labels["app.kubernetes.io/managed-by"], "adam-operator");
    assert_eq!(labels["app.kubernetes.io/instance"], "chat");
    let owner = &deployment.metadata.owner_references.clone().unwrap()[0];
    assert_eq!(owner.kind, "AgentService");
    assert_eq!(
        Some(&owner.uid),
        ready["metadata"]["uid"]
            .as_str()
            .map(str::to_owned)
            .as_ref()
    );
    // The pods carry the digest the status reports (adam reads its files at startup: a new digest is a rollout).
    let template = deployment
        .spec
        .as_ref()
        .unwrap()
        .template
        .metadata
        .as_ref()
        .unwrap();
    assert_eq!(
        template.annotations.as_ref().unwrap()["agents.vymalo.com/config-digest"],
        status["config"]["digest"].as_str().unwrap()
    );
    case.api::<Service>().get("chat").await.unwrap();
    let configmaps = case
        .api::<ConfigMap>()
        .list(&ListParams::default())
        .await
        .unwrap();
    assert!(
        configmaps.items.iter().any(|c| c
            .metadata
            .name
            .as_deref()
            .is_some_and(|n| n.starts_with("chat-agent-"))),
        "the folder's ConfigMap is there"
    );

    // The config says it is valid.
    let config = case
        .api::<adam_operator_api::AgentConfig>()
        .get("chat")
        .await
        .unwrap();
    let valid = serde_json::to_value(config.status.unwrap()).unwrap();
    assert!(is(&valid, "Valid", "True", "Valid"), "{valid:#}");

    // The Events say it too (they are a courtesy, but the operator is supposed to write them).
    let events = poll("the Event of the first Ready", 30, || async {
        let events: Vec<Event> = Api::<Event>::namespaced(cluster.client.clone(), ns)
            .list(&ListParams::default())
            .await
            .ok()?
            .items;
        events
            .iter()
            .any(|e| e.reason.as_deref() == Some("Reconciled"))
            .then_some(events)
    })
    .await;
    let e = events
        .iter()
        .find(|e| e.reason.as_deref() == Some("Reconciled"))
        .unwrap();
    assert_eq!(e.type_.as_deref(), Some("Normal"));
    assert_eq!(e.regarding.as_ref().unwrap().name.as_deref(), Some("chat"));

    // Delete: the finalizer runs the provider's delete and lets the object go.
    case.api::<AgentService>()
        .delete("chat", &DeleteParams::default())
        .await
        .unwrap();
    case.gone::<AgentService>("chat", 120).await;
    case.gone::<Deployment>("chat", 60).await;
    case.gone::<Service>("chat", 60).await;
    let left = case
        .api::<ConfigMap>()
        .list(&ListParams::default())
        .await
        .unwrap();
    assert!(
        !left.items.iter().any(|c| c
            .metadata
            .name
            .as_deref()
            .is_some_and(|n| n.starts_with("chat-agent-"))),
        "the folder's ConfigMap went with the compute"
    );
    // The config is not the service's to delete.
    case.api::<adam_operator_api::AgentConfig>()
        .get("chat")
        .await
        .unwrap();
    case.finish().await;
}

#[tokio::test]
async fn a_coder_keeps_its_volume_under_retain_and_loses_it_under_delete() {
    let Some(cluster) = connect().await else {
        return;
    };
    let case = Case::begin(&cluster, "coder").await;
    for (name, policy) in [("keep", "Retain"), ("drop", "Delete")] {
        case.secrets(name).await;
        let (svc, cfg) = coder(name, &cluster.image, policy);
        case.apply::<adam_operator_api::AgentConfig>(&cfg).await;
        case.apply::<AgentService>(&svc).await;
    }
    for name in ["keep", "drop"] {
        case.until(name, "Ready", 240, |s| state(s) == "Ready")
            .await;
        // A per-replica claim is a StatefulSet, with the claim template named after the volume.
        let set = case.api::<StatefulSet>().get(name).await.unwrap();
        let templates = set.spec.unwrap().volume_claim_templates.unwrap();
        assert_eq!(templates[0].metadata.name.as_deref(), Some("work"));
    }
    if !cluster.no_workloads {
        // The StatefulSet controller makes the claims.
        for name in ["keep", "drop"] {
            let claim = format!("work-{name}-0");
            poll(&format!("the claim {claim}"), 120, || async {
                case.api::<PersistentVolumeClaim>()
                    .get_opt(&claim)
                    .await
                    .unwrap()
                    .map(|_| ())
            })
            .await;
        }
    }

    for name in ["keep", "drop"] {
        case.api::<AgentService>()
            .delete(name, &DeleteParams::default())
            .await
            .unwrap();
    }
    for name in ["keep", "drop"] {
        case.gone::<AgentService>(name, 180).await;
        case.gone::<StatefulSet>(name, 60).await;
    }
    if !cluster.no_workloads {
        let claims = case.api::<PersistentVolumeClaim>();
        // Retain: the data stays, for a service of the same name to find again. Delete: it goes.
        assert!(
            claims.get_opt("work-keep-0").await.unwrap().is_some(),
            "Retain keeps the claim"
        );
        poll("the claim of the Delete policy to go", 120, || async {
            claims
                .get_opt("work-drop-0")
                .await
                .unwrap()
                .is_none()
                .then_some(())
        })
        .await;
        // The claim of Retain loses its owner, so nothing garbage-collects it later.
        let kept = claims.get("work-keep-0").await.unwrap();
        assert!(
            kept.metadata
                .owner_references
                .unwrap_or_default()
                .is_empty(),
            "a retained claim has no owner left to collect it"
        );
    }
    case.finish().await;
}

#[tokio::test]
async fn a_missing_secret_is_a_condition_until_the_secret_exists() {
    let Some(cluster) = connect().await else {
        return;
    };
    if cluster.no_workloads {
        eprintln!("skipped: {NO_WORKLOADS_VAR}: no kubelet to say a Secret is missing");
        return;
    }
    let case = Case::begin(&cluster, "secret").await;
    // The service first, the Secrets after: the pod cannot start, and the kubelet says why.
    let (svc, cfg) = chat("chat", &cluster.image);
    case.apply::<adam_operator_api::AgentConfig>(&cfg).await;
    case.apply::<AgentService>(&svc).await;
    let blocked = case
        .until("chat", "RuntimeReady: MissingSecret", 180, |s| {
            is(s, "RuntimeReady", "False", "MissingSecret")
        })
        .await;
    assert_eq!(state(&blocked["status"]), "Degraded");
    assert!(is(&blocked["status"], "Ready", "False", "MissingSecret"));
    let message = serde_json::to_string(&blocked["status"]["conditions"]).unwrap();
    assert!(
        message.contains("chat-secrets") || message.contains("chat-db"),
        "{message}"
    );
    assert!(!message.contains("dummy"), "no value is ever in a status");

    // The Secrets appear: the pod starts, and no object of ours changed (the runtime's watch told us).
    case.secrets("chat").await;
    case.until("chat", "Ready after the Secrets exist", 240, |s| {
        state(s) == "Ready"
    })
    .await;
    case.finish().await;
}

#[tokio::test]
async fn a_missing_or_invalid_config_blocks_and_a_fixed_one_unblocks() {
    let Some(cluster) = connect().await else {
        return;
    };
    let case = Case::begin(&cluster, "config").await;
    case.secrets("chat").await;
    let (svc, cfg) = chat("chat", &cluster.image);

    // No config yet.
    case.apply::<AgentService>(&svc).await;
    let blocked = case
        .until("chat", "ConfigNotFound", 60, |s| {
            is(s, "ConfigResolved", "False", "ConfigNotFound")
        })
        .await;
    assert_eq!(state(&blocked["status"]), "Blocked");
    assert!(is(&blocked["status"], "Ready", "False", "ConfigNotFound"));
    assert!(
        case.api::<Deployment>()
            .get_opt("chat")
            .await
            .unwrap()
            .is_none(),
        "nothing is made without a config"
    );

    // A config the schema accepts and the reconciler does not: a folder with no instructions.
    let mut invalid = cfg.clone();
    invalid["spec"]["harness"]["adam"]["agent"]["folder"]["files"] =
        json!({"README.md": "no instructions here"});
    case.apply::<adam_operator_api::AgentConfig>(&invalid).await;
    let blocked = case
        .until("chat", "ConfigInvalid", 60, |s| {
            is(s, "ConfigResolved", "False", "ConfigInvalid")
        })
        .await;
    let why = blocked["status"]["conditions"][0]["message"]
        .as_str()
        .unwrap();
    assert!(why.contains("instructions.md"), "{why}");
    let config = case
        .api::<adam_operator_api::AgentConfig>()
        .get("chat")
        .await
        .unwrap();
    let valid = serde_json::to_value(config.status.unwrap()).unwrap();
    assert!(is(&valid, "Valid", "False", "ConfigInvalid"), "{valid:#}");
    assert!(
        case.api::<Deployment>()
            .get_opt("chat")
            .await
            .unwrap()
            .is_none()
    );

    // Fixed: the service follows its config with nobody touching it.
    case.apply::<adam_operator_api::AgentConfig>(&cfg).await;
    case.until("chat", "Ready", 240, |s| state(s) == "Ready")
        .await;

    // Broken again while it runs: what runs is left alone.
    let before = case.api::<Deployment>().get("chat").await.unwrap();
    case.apply::<adam_operator_api::AgentConfig>(&invalid).await;
    let blocked = case
        .until("chat", "Blocked again", 60, |s| {
            is(s, "ConfigResolved", "False", "ConfigInvalid")
        })
        .await;
    assert_eq!(state(&blocked["status"]), "Blocked");
    let after = case.api::<Deployment>().get("chat").await.unwrap();
    assert_eq!(
        before.metadata.generation, after.metadata.generation,
        "a blocked service leaves its workload untouched"
    );
    case.finish().await;
}

#[tokio::test]
async fn suspend_scales_to_zero_and_resume_wakes() {
    let Some(cluster) = connect().await else {
        return;
    };
    let case = Case::begin(&cluster, "suspend").await;
    case.secrets("chat").await;
    let (svc, cfg) = chat("chat", &cluster.image);
    case.apply::<adam_operator_api::AgentConfig>(&cfg).await;
    case.apply::<AgentService>(&svc).await;
    case.until("chat", "Ready", 240, |s| state(s) == "Ready")
        .await;

    let mut suspended = svc.clone();
    suspended["spec"]["suspend"] = json!(true);
    case.apply::<AgentService>(&suspended).await;
    let s = case
        .until("chat", "Suspended", 120, |s| state(s) == "Suspended")
        .await;
    assert!(is(&s["status"], "RuntimeReady", "False", "Suspended"));
    assert_eq!(s["status"]["runtime"]["phase"], "Suspended");
    assert_eq!(
        case.api::<Deployment>()
            .get("chat")
            .await
            .unwrap()
            .spec
            .unwrap()
            .replicas,
        Some(0)
    );

    case.apply::<AgentService>(&svc).await;
    case.until("chat", "Ready again", 240, |s| state(s) == "Ready")
        .await;
    assert_eq!(
        case.api::<Deployment>()
            .get("chat")
            .await
            .unwrap()
            .spec
            .unwrap()
            .replicas,
        Some(1)
    );
    case.finish().await;
}

#[tokio::test]
async fn an_object_that_is_not_ours_with_the_name_is_a_conflict_that_changes_nothing() {
    let Some(cluster) = connect().await else {
        return;
    };
    let case = Case::begin(&cluster, "conflict").await;
    case.secrets("chat").await;

    // A Deployment named `chat`, as a Helm release would have made it: no managed-by label of ours.
    let foreign: Deployment = serde_json::from_value(json!({
        "apiVersion": "apps/v1", "kind": "Deployment",
        "metadata": {"name": "chat", "labels": {"app.kubernetes.io/managed-by": "Helm"}},
        "spec": {
            "replicas": 1,
            "selector": {"matchLabels": {"app": "foreign"}},
            "template": {
                "metadata": {"labels": {"app": "foreign"}},
                "spec": {"containers": [{"name": "c", "image": cluster.image, "command": ["sleep", "3600"]}]},
            },
        },
    }))
    .unwrap();
    case.api::<Deployment>()
        .create(&PostParams::default(), &foreign)
        .await
        .unwrap();
    let before = case.api::<Deployment>().get("chat").await.unwrap();

    let (svc, cfg) = chat("chat", &cluster.image);
    case.apply::<adam_operator_api::AgentConfig>(&cfg).await;
    case.apply::<AgentService>(&svc).await;
    let blocked = case
        .until("chat", "NameConflict", 120, |s| {
            is(s, "RuntimeReady", "False", "NameConflict")
        })
        .await;
    assert_eq!(state(&blocked["status"]), "Blocked");
    assert!(is(&blocked["status"], "Ready", "False", "NameConflict"));
    let after = case.api::<Deployment>().get("chat").await.unwrap();
    // Not its resourceVersion: a controller manager moves that when pods change. What the operator must not do is
    // write the object, and a write by it would show as a generation, or as a field of its own manager.
    assert_eq!(
        before.metadata.generation, after.metadata.generation,
        "the foreign object was not changed"
    );
    assert!(
        after
            .metadata
            .managed_fields
            .clone()
            .unwrap_or_default()
            .iter()
            .all(|f| f.manager.as_deref() != Some("adam-operator")),
        "no field of the foreign object belongs to the operator"
    );
    assert_eq!(
        after.metadata.labels.unwrap()["app.kubernetes.io/managed-by"],
        "Helm"
    );

    // Its owner removes it (the Helm release is pruned): the operator makes its own, with no help.
    case.api::<Deployment>()
        .delete("chat", &DeleteParams::default())
        .await
        .unwrap();
    case.until("chat", "Ready once the name is free", 240, |s| {
        state(s) == "Ready"
    })
    .await;
    let ours = case.api::<Deployment>().get("chat").await.unwrap();
    assert_eq!(
        ours.metadata.labels.unwrap()["app.kubernetes.io/managed-by"],
        "adam-operator"
    );
    case.finish().await;
}

#[tokio::test]
async fn a_deletion_that_happens_while_the_operator_is_down_completes_when_it_returns() {
    let Some(cluster) = connect().await else {
        return;
    };
    let mut case = Case::begin(&cluster, "restart").await;
    case.secrets("chat").await;
    let (svc, cfg) = chat("chat", &cluster.image);
    case.apply::<adam_operator_api::AgentConfig>(&cfg).await;
    case.apply::<AgentService>(&svc).await;
    case.until("chat", "Ready", 240, |s| state(s) == "Ready")
        .await;

    // Killed, not asked: it does not get to say goodbye.
    case.operator.as_mut().unwrap().stop();
    case.api::<AgentService>()
        .delete("chat", &DeleteParams::default())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let held = case
        .service("chat")
        .await
        .expect("the finalizer holds the object");
    assert!(held["metadata"]["deletionTimestamp"].is_string());
    assert!(
        case.api::<Deployment>()
            .get_opt("chat")
            .await
            .unwrap()
            .is_some(),
        "nothing has run the delete yet"
    );

    case.operator = Some(OperatorProc::spawn(&cluster, &case.ns, None));
    case.gone::<AgentService>("chat", 120).await;
    case.gone::<Deployment>("chat", 60).await;
    case.finish().await;
}

#[tokio::test]
async fn health_readiness_and_metrics_are_served() {
    let Some(cluster) = connect().await else {
        return;
    };
    let case = Case::begin(&cluster, "ports").await;
    let (health, metrics) = {
        let op = case.operator.as_ref().unwrap();
        (op.health, op.metrics)
    };
    poll("/healthz", 60, || async {
        (http_get(health, "/healthz").await?.0 == 200).then_some(())
    })
    .await;
    // Ready once the caches have listed the cluster.
    poll("/readyz", 60, || async {
        (http_get(health, "/readyz").await?.0 == 200).then_some(())
    })
    .await;

    case.secrets("chat").await;
    let (svc, cfg) = chat("chat", &cluster.image);
    case.apply::<adam_operator_api::AgentConfig>(&cfg).await;
    case.apply::<AgentService>(&svc).await;
    case.until("chat", "Ready", 240, |s| state(s) == "Ready")
        .await;

    let (code, body) = http_get(metrics, "/metrics")
        .await
        .expect("the metrics server answers");
    assert_eq!(code, 200);
    assert!(
        body.contains("# TYPE adam_operator_reconcile_total counter"),
        "{body}"
    );
    assert!(
        body.contains(r#"adam_operator_reconcile_total{controller="agentservice",result="ok"}"#),
        "{body}"
    );
    assert!(
        body.contains(r#"adam_operator_service_state_changes_total{state="Ready"}"#),
        "{body}"
    );
    case.finish().await;
}

/// Whether the API server serves CloudNativePG's `Cluster`.
async fn cnpg_installed(client: &Client) -> bool {
    match client
        .list_api_group_resources("postgresql.cnpg.io/v1")
        .await
    {
        Ok(list) => list.resources.iter().any(|r| r.name == "clusters"),
        Err(kube::Error::Api(status)) if status.code == 404 => false,
        Err(e) => panic!("discovering postgresql.cnpg.io/v1: {e}"),
    }
}

/// A service that asks for an operator-owned cluster, on a cluster without CloudNativePG. (With it
/// installed, `crates/adam-operator-store-cnpg/tests/cluster.rs` and the `store-cnpg` job of `operator.yml` are the proof.)
#[tokio::test]
async fn a_service_that_asks_for_a_cluster_is_cnpg_not_installed_without_cloudnativepg() {
    let Some(cluster) = connect().await else {
        return;
    };
    if cnpg_installed(&cluster.client).await {
        assert!(
            !required(),
            "this case needs a cluster without CloudNativePG, and {REQUIRE_VAR}=1 forbids skipping"
        );
        eprintln!("skipped: CloudNativePG is installed in this cluster");
        return;
    }
    let case = Case::begin(&cluster, "cnpg").await;
    case.secrets("chat").await;
    let (mut svc, cfg) = chat("chat", &cluster.image);
    svc["spec"]["store"] =
        json!({"postgres": {"cnpg": {"instances": 1, "storage": {"size": "1Gi"}}}});
    case.apply::<adam_operator_api::AgentConfig>(&cfg).await;
    case.apply::<AgentService>(&svc).await;

    let blocked = case
        .until("chat", "StoreReady False, CNPGNotInstalled", 60, |s| {
            is(s, "StoreReady", "False", "CNPGNotInstalled")
        })
        .await;
    assert_eq!(state(&blocked["status"]), "Blocked");
    assert!(is(&blocked["status"], "Ready", "False", "CNPGNotInstalled"));
    assert!(
        case.api::<Deployment>()
            .get_opt("chat")
            .await
            .unwrap()
            .is_none(),
        "no agent is made against a database that cannot exist"
    );

    // Deleting a service that never got its cluster completes its finalizer: the release finds no API.
    case.api::<AgentService>()
        .delete("chat", &DeleteParams::default())
        .await
        .unwrap();
    case.gone::<AgentService>("chat", 60).await;
    case.finish().await;
}

// ---------------------------------------------------------------------- the registry

/// A bare HTTP/1.1 request: status code, headers (names lower-cased) and body.
async fn http_request(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> Option<(u16, Vec<(String, String)>, String)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .ok()?;
    let mut request =
        format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
    for (k, v) in headers {
        request.push_str(&format!("{k}: {v}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await.ok()?;
    let mut out = String::new();
    stream.read_to_string(&mut out).await.ok()?;
    let (head, body) = out.split_once("\r\n\r\n")?;
    let mut lines = head.lines();
    let code = lines.next()?.split_whitespace().nth(1)?.parse().ok()?;
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    Some((code, headers, body.to_owned()))
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> &'a str {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map_or("", |(_, v)| v.as_str())
}

/// Run `script` in a pod of the stand-in image (busybox, so `wget`) and return what it printed.
async fn run_in_pod(case: &Case<'_>, name: &str, script: &str) -> String {
    use kube::api::PostParams;
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1", "kind": "Pod", "metadata": {"name": name},
        "spec": {
            "restartPolicy": "Never",
            "automountServiceAccountToken": false,
            "securityContext": {"runAsUser": 10001, "runAsGroup": 10001, "runAsNonRoot": true,
                                "seccompProfile": {"type": "RuntimeDefault"}},
            "containers": [{
                "name": "probe", "image": case.cluster.image,
                "command": ["sh", "-c", script],
                "securityContext": {"allowPrivilegeEscalation": false, "capabilities": {"drop": ["ALL"]}},
            }],
        },
    }))
    .unwrap();
    let pods = case.api::<Pod>();
    pods.create(&PostParams::default(), &pod)
        .await
        .unwrap_or_else(|e| panic!("pod {name}: {e}"));
    let phase = poll(&format!("pod {name} to finish"), 180, || async {
        let p = pods.get(name).await.ok()?;
        let phase = p.status?.phase?;
        matches!(phase.as_str(), "Succeeded" | "Failed").then_some(phase)
    })
    .await;
    let logs = pods
        .logs(name, &LogParams::default())
        .await
        .unwrap_or_else(|e| format!("(no logs: {e})"));
    assert_eq!(
        phase, "Succeeded",
        "pod {name} ended {phase}; it printed:\n{logs}"
    );
    logs
}

#[tokio::test]
async fn the_registry_lists_a_ready_agent_to_whoever_holds_the_token_and_nobody_else() {
    let Some(cluster) = connect().await else {
        return;
    };
    const TOKEN: &str = "e2e-registry-token-6f1d0c9a";
    let case = Case::begin_with(&cluster, "registry", Some(TOKEN)).await;
    let port = case.operator.as_ref().unwrap().registry;
    let ns = case.ns.clone();

    case.secrets("chat").await;
    let (svc, cfg) = chat("chat", &cluster.image);
    case.apply::<adam_operator_api::AgentConfig>(&cfg).await;
    case.apply::<AgentService>(&svc).await;
    // A service whose config does not exist is Blocked, and a registry never lists what is not applied.
    let (mut orphan, _) = chat("orphan", &cluster.image);
    orphan["spec"]["configRef"] = json!({"name": "nobody-made-this"});
    case.apply::<AgentService>(&orphan).await;

    let listed = case
        .until("chat", "Listed: True", 240, |s| {
            state(s) == "Ready" && is(s, "Listed", "True", "Listed")
        })
        .await;
    assert!(is(&listed["status"], "Ready", "True", "Reconciled"));
    let blocked = case
        .until("orphan", "Listed: False, ServiceBlocked", 60, |s| {
            is(s, "Listed", "False", "ServiceBlocked")
        })
        .await;
    assert_eq!(state(&blocked["status"]), "Blocked");

    // No token, or a wrong one: 401, and nothing in the body.
    for headers in [
        vec![],
        vec![("Authorization", "Bearer wrong")],
        vec![("Authorization", "Basic ZTJlOnRva2Vu")],
    ] {
        let (code, _, body) = poll("the registry to answer", 60, || async {
            http_request(port, "GET", "/registry/v1/agents", &headers).await
        })
        .await;
        assert_eq!(code, 401, "{headers:?}");
        assert!(body.is_empty(), "{body}");
    }

    // The token: the linkset, with the cache headers of the contract.
    let auth = format!("Bearer {TOKEN}");
    let (code, headers, body) = http_request(
        port,
        "GET",
        "/registry/v1/agents",
        &[
            ("Authorization", &auth),
            ("Accept", "application/linkset+json"),
        ],
    )
    .await
    .expect("the registry answers");
    assert_eq!(code, 200, "{body}");
    assert!(header(&headers, "content-type").starts_with("application/linkset+json"));
    assert_eq!(header(&headers, "cache-control"), "private, max-age=30");
    assert_eq!(header(&headers, "vary"), "Authorization");
    let etag = header(&headers, "etag").to_owned();
    assert!(etag.starts_with('"'), "{etag}");
    let doc: Value = serde_json::from_str(&body).unwrap();
    let context = &doc["linkset"][0];
    assert_eq!(
        context["profile"][0]["href"],
        "https://agents.vymalo.com/registry/v1"
    );
    let card = format!("http://chat.{ns}.svc:8080/.well-known/agent-card.json");
    assert_eq!(
        context["item"],
        json!([{"href": card, "type": "application/json", "service": ["chat"], "title": "chat", "tags": ["e2e"]}]),
        "the agent that is Ready is listed, the blocked one is not: {body}"
    );
    assert!(!body.contains(TOKEN));

    // The validator: 304 on a match, and HEAD has the headers and no body.
    let (code, headers, body) = http_request(
        port,
        "GET",
        "/registry/v1/agents",
        &[("Authorization", &auth), ("If-None-Match", &etag)],
    )
    .await
    .unwrap();
    assert_eq!(code, 304);
    assert!(body.is_empty());
    assert_eq!(header(&headers, "etag"), etag);
    let (code, _, body) = http_request(
        port,
        "HEAD",
        "/registry/v1/agents",
        &[("Authorization", &auth)],
    )
    .await
    .unwrap();
    assert_eq!(code, 200);
    assert!(body.is_empty());

    // From inside the cluster: the registry over the network a pod has, and the card it points at.
    match std::env::var(HOST_ADDR_VAR).ok().filter(|h| !h.is_empty()) {
        Some(host) => {
            let script = format!(
                "wget -q -O - --header 'Authorization: Bearer {TOKEN}' http://{host}:{port}/registry/v1/agents && echo && echo ---- && wget -q -O - {card}"
            );
            let logs = run_in_pod(&case, "registry-reader", &script).await;
            assert!(logs.contains(r#""service":["chat"]"#), "{logs}");
            assert!(!logs.contains("orphan"), "{logs}");
            let (_, card_text) = logs.split_once("----").unwrap();
            assert!(
                card_text.contains("a stand-in for the adam-rs image"),
                "the card the registry lists answers in the cluster: {logs}"
            );
            // And without the token the same pod is refused.
            let refused = run_in_pod(
                &case,
                "registry-refused",
                &format!(
                    "wget -q -O - http://{host}:{port}/registry/v1/agents 2>&1; echo \"exit=$?\"; true"
                ),
            )
            .await;
            assert!(
                refused.contains("401") || refused.contains("Unauthorized"),
                "{refused}"
            );
            assert!(!refused.contains("linkset"), "{refused}");
        }
        None => {
            eprintln!("skipped: the in-cluster read, {HOST_ADDR_VAR} is not set");
        }
    }

    // Deleting the service takes it out of the list.
    case.api::<AgentService>()
        .delete("chat", &DeleteParams::default())
        .await
        .unwrap();
    case.gone::<AgentService>("chat", 120).await;
    let (code, _, body) = http_request(
        port,
        "GET",
        "/registry/v1/agents",
        &[("Authorization", &auth)],
    )
    .await
    .unwrap();
    assert_eq!(code, 200);
    assert!(!body.contains(r#""service":["chat"]"#), "{body}");
    case.finish().await;
}

#[tokio::test]
async fn without_a_token_no_registry_is_served_and_nothing_is_listed() {
    let Some(cluster) = connect().await else {
        return;
    };
    let case = Case::begin(&cluster, "noregistry").await;
    let port = case.operator.as_ref().unwrap().registry;
    case.secrets("chat").await;
    let (svc, cfg) = chat("chat", &cluster.image);
    case.apply::<adam_operator_api::AgentConfig>(&cfg).await;
    case.apply::<AgentService>(&svc).await;
    let ready = case
        .until("chat", "Ready", 240, |s| state(s) == "Ready")
        .await;
    assert!(
        is(&ready["status"], "Listed", "False", "RegistryDisabled"),
        "{:#}",
        ready["status"]
    );
    // Nothing listens on the registry's port: fail closed.
    assert!(
        http_request(
            port,
            "GET",
            "/registry/v1/agents",
            &[("Authorization", "Bearer x")]
        )
        .await
        .is_none()
    );
    case.finish().await;
}
