//! The provider against a real API server: the conformance suite of `adam-operator-ports`, and what only a
//! cluster can show (what a controller makes of the objects, the kubelet's words about a pod, the
//! garbage collector, the API server's refusals).
//!
//! **Opt in.** The tests create namespace `adam-operator-test` and objects in it; they run only when
//! `ADAM_OPERATOR_TEST_KUBECONFIG` names a kubeconfig file (never the default context: a developer's cluster is
//! not a test fixture by accident). Without it they skip, and with `ADAM_OPERATOR_TEST_REQUIRE_CLUSTER=1`
//! (CI) skipping is a failure. A throwaway cluster is the point: nothing is cleaned up but what a
//! case deletes itself, and the claims of `Retain` stay.
//!
//! ```sh
//! kind create cluster --name adam-operator
//! ADAM_OPERATOR_TEST_KUBECONFIG=$HOME/.kube/config cargo test -p adam-operator-runtime-kubernetes --test cluster
//! ```
//!
//! The pods are real: the agent's image and command are replaced by a tiny pinned image (busybox)
//! serving `/healthz`, because the suite's spec names an image that does not exist. The harness also
//! makes the Secrets the spec references (with a dummy value), as a deployment's operator of Secrets
//! would, so that pods can start. Cases that are about a missing Secret or image use the provider
//! directly.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::future::Future;
use std::time::Duration;

use adam_operator_ports::testkit::{RuntimeUnderTest, SENTINEL, sample_spec, unique};
use adam_operator_ports::{
    Capabilities, Classify, DeleteOutcome, DeletionPolicy, Endpoint, EnvValue, ErrorClass,
    IssueReason, Phase, RuntimeError, RuntimeId, RuntimeProvider, RuntimeSpec, RuntimeStatus,
    Surface, VolumeSource,
};
use adam_operator_runtime_kubernetes::{KubernetesRuntime, owner_handle};
use futures::stream::BoxStream;
use k8s_openapi::api::apps::v1::{Deployment, StatefulSet};
use k8s_openapi::api::core::v1::{ConfigMap, Namespace, PersistentVolumeClaim, Secret, Service};
use kube::api::{DeleteParams, ListParams, Patch, PatchParams};
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::{Api, Client, Config};
use serde_json::json;

const KUBECONFIG_VAR: &str = "ADAM_OPERATOR_TEST_KUBECONFIG";
const REQUIRE_VAR: &str = "ADAM_OPERATOR_TEST_REQUIRE_CLUSTER";

/// The namespace the suite's ids live in (`adam_operator_ports::testkit` uses it for every id).
const NAMESPACE: &str = "adam-operator-test";

/// `busybox:1.37.0` from the public ECR mirror of Docker Official Images, which has no pull limit for
/// a CI runner (Docker Hub has one). The digest is the index of the tag (amd64, arm64 and others),
/// the same on Docker Hub. *Verified 2026-10-05*: the `Docker-Content-Digest` of
/// `registry-1.docker.io/v2/library/busybox/manifests/1.37.0` and of
/// `public.ecr.aws/v2/docker/library/busybox/manifests/1.37.0`, an OCI image index.
const IMAGE: &str = "public.ecr.aws/docker/library/busybox:1.37.0@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e";

fn required() -> bool {
    matches!(std::env::var(REQUIRE_VAR).as_deref(), Ok("1" | "true"))
}

/// The cluster of `ADAM_OPERATOR_TEST_KUBECONFIG`, or `None` to skip.
async fn connect() -> Option<Client> {
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
    // A variable that is set names a cluster that must answer.
    client
        .apiserver_version()
        .await
        .unwrap_or_else(|e| panic!("the cluster of {path} does not answer: {e}"));
    Some(client)
}

async fn namespace(client: &Client) {
    let ns: Namespace = serde_json::from_value(json!({
        "apiVersion": "v1", "kind": "Namespace", "metadata": {"name": NAMESPACE}
    }))
    .unwrap();
    Api::<Namespace>::all(client.clone())
        .patch(
            NAMESPACE,
            &PatchParams::apply("adam-operator-test-harness"),
            &Patch::Apply(&ns),
        )
        .await
        .unwrap_or_else(|e| panic!("namespace {NAMESPACE}: {e}"));
}

// ---------------------------------------------------------------- the harness

/// The provider, with the spec made runnable on any cluster: see the module's documentation.
#[derive(Clone)]
struct Harness {
    runtime: KubernetesRuntime,
    client: Client,
}

/// A spec whose containers run busybox: the agent serves `/healthz` on its port, a sidecar sleeps.
/// Everything else is the spec's own, so the objects are the ones the provider makes for it. Pods
/// stop at once (the real grace period would hold the deletion of a claim for minutes).
fn runnable(spec: &RuntimeSpec) -> RuntimeSpec {
    let mut spec = spec.clone();
    for w in &mut spec.workloads {
        w.termination_grace_secs = Some(1);
        for c in std::iter::once(&mut w.container).chain(w.sidecars.iter_mut()) {
            c.image = IMAGE.to_owned();
            c.args.clear();
            c.command = match c.port {
                Some(port) => vec![
                    "sh".to_owned(),
                    "-c".to_owned(),
                    format!(
                        "mkdir -p /tmp/www && echo ok > /tmp/www/healthz && exec httpd -f -p {port} -h /tmp/www"
                    ),
                ],
                None => vec!["sleep".to_owned(), "3600".to_owned()],
            };
        }
    }
    spec
}

/// The Secrets a spec references, with the keys it reads.
fn secrets_of(spec: &RuntimeSpec) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for w in &spec.workloads {
        for c in std::iter::once(&w.container).chain(&w.sidecars) {
            for e in &c.env {
                if let EnvValue::Secret(s) = &e.value {
                    out.entry(s.name.clone()).or_default().push(s.key.clone());
                }
            }
        }
        for v in &w.volumes {
            if let VolumeSource::SecretFile { secret, .. } = &v.source {
                out.entry(secret.name.clone())
                    .or_default()
                    .push(secret.key.clone());
            }
        }
    }
    out
}

async fn make_secrets(client: &Client, spec: &RuntimeSpec) {
    for (name, keys) in secrets_of(spec) {
        let data: BTreeMap<String, String> = keys
            .into_iter()
            .map(|k| (k, "not-a-secret".to_owned()))
            .collect();
        let secret: Secret = serde_json::from_value(json!({
            "apiVersion": "v1", "kind": "Secret",
            "metadata": {"name": name, "namespace": NAMESPACE},
            "stringData": data,
        }))
        .unwrap();
        Api::<Secret>::namespaced(client.clone(), NAMESPACE)
            .patch(
                &name,
                &PatchParams::apply("adam-operator-test-harness").force(),
                &Patch::Apply(&secret),
            )
            .await
            .unwrap_or_else(|e| panic!("secret {name}: {e}"));
    }
}

impl RuntimeProvider for Harness {
    fn name(&self) -> &'static str {
        self.runtime.name()
    }

    fn capabilities(&self) -> Capabilities {
        self.runtime.capabilities()
    }

    async fn ensure(
        &self,
        id: &RuntimeId,
        spec: &RuntimeSpec,
    ) -> Result<RuntimeStatus, RuntimeError> {
        let spec = runnable(spec);
        make_secrets(&self.client, &spec).await;
        self.runtime.ensure(id, &spec).await
    }

    async fn suspend(&self, id: &RuntimeId) -> Result<RuntimeStatus, RuntimeError> {
        self.runtime.suspend(id).await
    }

    async fn delete(&self, id: &RuntimeId) -> Result<DeleteOutcome, RuntimeError> {
        self.runtime.delete(id).await
    }

    async fn status(&self, id: &RuntimeId) -> Result<RuntimeStatus, RuntimeError> {
        self.runtime.status(id).await
    }

    async fn endpoint(&self, id: &RuntimeId, surface: Surface) -> Result<Endpoint, RuntimeError> {
        self.runtime.endpoint(id, surface).await
    }

    fn watch(&self) -> BoxStream<'static, RuntimeId> {
        self.runtime.watch()
    }
}

impl RuntimeUnderTest for Harness {
    async fn materialised(&self, id: &RuntimeId) -> Vec<String> {
        self.runtime.plain_text(id).await.unwrap()
    }
}

async fn make() -> Option<Harness> {
    let client = connect().await?;
    namespace(&client).await;
    Some(Harness {
        runtime: KubernetesRuntime::new(client.clone()),
        client,
    })
}

adam_operator_ports::runtime_provider_conformance!(make);

// ---------------------------------------------------------------- helpers

fn fresh() -> RuntimeId {
    RuntimeId::new(NAMESPACE, unique("rt"))
}

/// Call `f` until it answers, for at most `secs` seconds.
async fn eventually<T, F, Fut>(what: &str, secs: u64, mut f: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(found) = f().await {
            return found;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "gave up after {secs}s waiting for {what}"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn ready(h: &Harness, id: &RuntimeId, secs: u64) -> RuntimeStatus {
    eventually("the runtime to be Ready", secs, || async {
        let s = h.status(id).await.unwrap();
        (s.phase == Phase::Ready).then_some(s)
    })
    .await
}

/// A spec of one Deployment pod: no volume, no sidecar, no Secret. The pod's command is what a case
/// is about.
fn bare(id: &RuntimeId) -> RuntimeSpec {
    let mut spec = sample_spec(id);
    spec.file_sets.clear();
    let w = &mut spec.workloads[0];
    w.replicas = 1;
    w.stable_identity = false;
    w.sidecars.clear();
    w.volumes.clear();
    w.container.mounts.clear();
    w.container
        .env
        .retain(|e| matches!(e.value, EnvValue::Literal(_)));
    spec
}

fn claims(client: &Client) -> Api<PersistentVolumeClaim> {
    Api::namespaced(client.clone(), NAMESPACE)
}

async fn claims_of(client: &Client, id: &RuntimeId) -> Vec<PersistentVolumeClaim> {
    claims(client)
        .list(&ListParams::default().labels(&format!(
            "app.kubernetes.io/managed-by=adam-operator,app.kubernetes.io/instance={}",
            id.name()
        )))
        .await
        .unwrap()
        .items
}

// ---------------------------------------------------------------- no cluster needed

#[tokio::test]
async fn a_tls_client_builds_and_a_refused_connection_is_unavailable() {
    // The TLS stack is built for an https server that is not there: a missing crypto provider
    // would panic here, and a refused connection must be a class that is retried.
    let mut config = Config::new("https://127.0.0.1:1".parse().unwrap());
    config.connect_timeout = Some(Duration::from_secs(2));
    config.accept_invalid_certs = true;
    let runtime = KubernetesRuntime::new(Client::try_from(config).unwrap());
    let err = runtime
        .status(&RuntimeId::new("ns", "svc"))
        .await
        .unwrap_err();
    assert_eq!(err.class(), ErrorClass::Transient, "{err}");
}

// ---------------------------------------------------------------- what only a cluster shows

#[tokio::test]
async fn a_runtime_gets_ready_and_its_claims_outlive_a_retained_delete() {
    let Some(h) = make().await else { return };
    let id = fresh();
    let mut spec = sample_spec(&id);
    spec.workloads[0].replicas = 2;

    h.ensure(&id, &spec).await.unwrap();
    let status = ready(&h, &id, 300).await;
    assert_eq!(status.replicas, 2);
    assert!(status.issues.is_empty());

    // The objects are ours, stamped with the digest, and the claims are the set's, labelled.
    let sts = Api::<StatefulSet>::namespaced(h.client.clone(), NAMESPACE)
        .get(id.name())
        .await
        .unwrap();
    let digest = &spec.digest;
    assert_eq!(
        sts.spec
            .as_ref()
            .unwrap()
            .template
            .metadata
            .as_ref()
            .unwrap()
            .annotations
            .as_ref()
            .unwrap()["agents.vymalo.com/config-digest"],
        *digest
    );
    assert!(
        sts.metadata.owner_references.is_none(),
        "the suite's owner token stands for no owner"
    );
    let before = claims_of(&h.client, &id).await;
    assert_eq!(before.len(), 2, "one claim per replica: {before:?}");
    for c in &before {
        let labels = c.metadata.labels.as_ref().unwrap();
        assert_eq!(labels["agents.vymalo.com/volume"], "work");
        assert_eq!(labels["app.kubernetes.io/managed-by"], "adam-operator");
        assert!(c.metadata.owner_references.is_none(), "a claim is data");
    }
    let service = h.endpoint(&id, Surface::A2a).await.unwrap();
    assert_eq!(
        service.url,
        format!("http://{}.{NAMESPACE}.svc:8080/", id.name())
    );

    // Retain: the compute goes, the claims stay.
    let outcome = h.delete(&id).await.unwrap();
    assert!(outcome.existed);
    assert_eq!(outcome.retained_volumes, ["work"]);
    assert_eq!(h.status(&id).await.unwrap().phase, Phase::Absent);
    tokio::time::sleep(Duration::from_secs(5)).await;
    let kept = claims_of(&h.client, &id).await;
    let uids = |cs: &[PersistentVolumeClaim]| {
        let mut u: Vec<_> = cs.iter().map(|c| c.metadata.uid.clone().unwrap()).collect();
        u.sort();
        u
    };
    assert_eq!(uids(&kept), uids(&before), "the same claims, not new ones");

    // The same service again finds them.
    h.ensure(&id, &spec).await.unwrap();
    ready(&h, &id, 300).await;
    assert_eq!(uids(&claims_of(&h.client, &id).await), uids(&before));

    // Delete: the data goes too, once the pods are gone.
    spec.deletion = DeletionPolicy::Delete;
    h.ensure(&id, &spec).await.unwrap();
    let outcome = h.delete(&id).await.unwrap();
    assert!(outcome.retained_volumes.is_empty());
    eventually("the claims to be deleted", 120, || async {
        claims_of(&h.client, &id).await.is_empty().then_some(())
    })
    .await;
}

#[tokio::test]
async fn an_object_that_is_not_ours_is_never_overwritten() {
    let Some(h) = make().await else { return };
    let id = fresh();
    let foreign: StatefulSet = serde_json::from_value(json!({
        "apiVersion": "apps/v1", "kind": "StatefulSet",
        "metadata": {"name": id.name(), "namespace": NAMESPACE, "labels": {
            "app.kubernetes.io/managed-by": "Helm", "app.kubernetes.io/instance": id.name()}},
        "spec": {"replicas": 0, "serviceName": id.name(),
                 "selector": {"matchLabels": {"app": "helm"}},
                 "template": {"metadata": {"labels": {"app": "helm"}},
                              "spec": {"containers": [{"name": "c", "image": IMAGE}]}}}
    }))
    .unwrap();
    let sts = Api::<StatefulSet>::namespaced(h.client.clone(), NAMESPACE);
    let before = sts.create(&Default::default(), &foreign).await.unwrap();

    let spec = sample_spec(&id);
    let status = h.ensure(&id, &spec).await.unwrap();
    assert!(status.is_name_conflict(), "{status:?}");
    assert_eq!(status.phase, Phase::Absent);
    // Its own controller keeps writing its status, so `resourceVersion` moves; what must not move is
    // anything of the spec, and no field may belong to the operator.
    let after = sts.get(id.name()).await.unwrap();
    assert_eq!(after.spec, before.spec);
    assert_eq!(after.metadata.generation, before.metadata.generation);
    assert_eq!(after.metadata.labels, before.metadata.labels);
    assert!(
        after
            .metadata
            .managed_fields
            .iter()
            .flatten()
            .all(|m| m.manager.as_deref() != Some("adam-operator")),
        "the operator wrote a field of an object that is not its own: {:?}",
        after.metadata.managed_fields
    );
    assert!(
        Api::<Service>::namespaced(h.client.clone(), NAMESPACE)
            .get_opt(id.name())
            .await
            .unwrap()
            .is_none(),
        "nothing else was made either"
    );

    // Its owner removes it, as Argo prunes a release: the runtime is made.
    sts.delete(id.name(), &DeleteParams::background())
        .await
        .unwrap();
    let status = h.ensure(&id, &spec).await.unwrap();
    assert!(!status.is_name_conflict(), "{status:?}");
    let made = sts.get(id.name()).await.unwrap();
    assert_eq!(
        made.metadata.labels.unwrap()["app.kubernetes.io/managed-by"],
        "adam-operator"
    );
    h.delete(&id).await.unwrap();
}

#[tokio::test]
async fn a_missing_secret_is_named_by_the_reason_and_the_runtime_recovers_when_it_exists() {
    let Some(h) = make().await else { return };
    let id = fresh();
    // One reference to a Secret that does not exist. The provider directly: the harness would make it.
    // Its name is this runtime's own: the other tests run at the same time in the namespace and make
    // `{SENTINEL}-secrets` for their pods, which would let this pod start.
    let missing = format!("{SENTINEL}-{}-secrets", id.name());
    let mut spec = runnable(&bare(&id));
    spec.workloads[0]
        .container
        .env
        .push(adam_operator_ports::EnvVar::secret(
            "MODEL_API_KEY",
            adam_operator_ports::SecretRef::new(missing.clone(), "MODEL_API_KEY"),
        ));
    h.runtime.ensure(&id, &spec).await.unwrap();

    let status = eventually("the missing Secret to be reported", 180, || async {
        let s = h.status(&id).await.unwrap();
        s.issues
            .iter()
            .any(|i| matches!(&i.reason, IssueReason::MissingSecret { name } if name == &missing))
            .then_some(s)
    })
    .await;
    assert_eq!(status.phase, Phase::Failed);
    for issue in &status.issues {
        assert!(!issue.message.contains(SENTINEL), "{}", issue.message);
    }

    // The Secret appears: the kubelet retries and the pod starts, with no change to the spec.
    make_secrets(&h.client, &spec).await;
    ready(&h, &id, 240).await;
    h.delete(&id).await.unwrap();
}

#[tokio::test]
async fn pods_that_fail_in_the_ways_adam_and_kubernetes_do_are_read() {
    let Some(h) = make().await else { return };
    let case = |image: Option<&str>, script: &str| {
        let id = fresh();
        let mut spec = runnable(&bare(&id));
        let c = &mut spec.workloads[0].container;
        c.command = vec!["sh".to_owned(), "-c".to_owned(), script.to_owned()];
        c.probes = adam_operator_ports::Probes::default();
        if let Some(image) = image {
            c.image = image.to_owned();
        }
        (id, spec)
    };
    let cases = [
        (
            case(Some("registry.invalid/none:1"), "true"),
            IssueReason::ImagePull,
        ),
        (case(None, "exit 78"), IssueReason::ConfigRejected),
        (case(None, "exit 69"), IssueReason::DependencyUnavailable),
        (case(None, "exit 1"), IssueReason::CrashLoop),
    ];
    for ((id, spec), _) in &cases {
        h.runtime.ensure(id, spec).await.unwrap();
    }
    for ((id, _), want) in &cases {
        let status = eventually(&format!("{want:?}"), 240, || async {
            let s = h.status(id).await.unwrap();
            s.issues.iter().any(|i| &i.reason == want).then_some(s)
        })
        .await;
        assert_eq!(status.phase, Phase::Failed, "{want:?}");
    }
    for ((id, _), _) in &cases {
        h.delete(id).await.unwrap();
    }
}

#[tokio::test]
async fn the_owner_collects_the_compute_and_never_the_claims() {
    let Some(h) = make().await else { return };
    let id = fresh();
    // Any object can own: a ConfigMap stands for the AgentService.
    let owners = Api::<ConfigMap>::namespaced(h.client.clone(), NAMESPACE);
    let owner: ConfigMap = serde_json::from_value(json!({
        "apiVersion": "v1", "kind": "ConfigMap",
        "metadata": {"name": format!("owner-{}", id.name()), "namespace": NAMESPACE}
    }))
    .unwrap();
    let owner = owners.create(&Default::default(), &owner).await.unwrap();
    let mut spec = sample_spec(&id);
    spec.workloads[0].replicas = 1;
    spec.owner = owner_handle(
        "v1",
        "ConfigMap",
        owner.metadata.name.clone().unwrap(),
        owner.metadata.uid.clone().unwrap(),
    );

    h.ensure(&id, &spec).await.unwrap();
    let sts = Api::<StatefulSet>::namespaced(h.client.clone(), NAMESPACE);
    let made = sts.get(id.name()).await.unwrap();
    assert_eq!(
        made.metadata.owner_references.as_ref().unwrap()[0].uid,
        owner.metadata.uid.clone().unwrap()
    );
    let service = Api::<Service>::namespaced(h.client.clone(), NAMESPACE)
        .get(id.name())
        .await
        .unwrap();
    assert!(service.metadata.owner_references.is_some());
    eventually("the claim to exist", 120, || async {
        (!claims_of(&h.client, &id).await.is_empty()).then_some(())
    })
    .await;

    owners
        .delete(
            owner.metadata.name.as_deref().unwrap(),
            &DeleteParams::background(),
        )
        .await
        .unwrap();
    eventually("the garbage collector to take the compute", 180, || async {
        sts.get_opt(id.name())
            .await
            .unwrap()
            .is_none()
            .then_some(())
    })
    .await;
    let kept = claims_of(&h.client, &id).await;
    assert_eq!(
        kept.len(),
        1,
        "the claim is data: it has no owner to be collected with"
    );
    for c in kept {
        claims(&h.client)
            .delete(
                c.metadata.name.as_deref().unwrap(),
                &DeleteParams::default(),
            )
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn a_claim_template_cannot_change_and_the_refusal_is_a_class() {
    let Some(h) = make().await else { return };
    let id = fresh();
    let mut spec = sample_spec(&id);
    spec.workloads[0].replicas = 1;
    h.ensure(&id, &spec).await.unwrap();
    if let VolumeSource::Persistent(p) = &mut spec.workloads[0].volumes[0].source {
        p.size = "30Gi".to_owned();
    }
    // Kubernetes does not resize a StatefulSet's claims; the operator reports it and deletes nothing.
    let err = h.ensure(&id, &spec).await.unwrap_err();
    assert_eq!(err.class(), ErrorClass::Invalid, "{err}");
    assert_ne!(
        h.status(&id).await.unwrap().phase,
        Phase::Absent,
        "nothing was deleted"
    );
    h.delete(&id).await.unwrap();
}

#[tokio::test]
async fn a_superseded_file_set_goes_after_the_rollout() {
    let Some(h) = make().await else { return };
    let id = fresh();
    let mut spec = sample_spec(&id);
    spec.workloads[0].replicas = 1;
    h.ensure(&id, &spec).await.unwrap();
    ready(&h, &id, 240).await;

    // A new folder: a new set under a new name, and a new digest.
    let old = spec.file_sets[0].name.clone();
    let new = format!("{}-agent-ffffffff", id.name());
    spec.file_sets[0].name.clone_from(&new);
    spec.file_sets[0].files.insert(
        "instructions.md".to_owned(),
        "Your name is Changed.\n".to_owned(),
    );
    if let VolumeSource::Files { file_set, .. } = &mut spec.workloads[0].volumes[1].source {
        file_set.clone_from(&new);
    }
    spec.digest =
        "sha256:3333333333333333333333333333333333333333333333333333333333333333".to_owned();
    let maps = Api::<ConfigMap>::namespaced(h.client.clone(), NAMESPACE);
    h.ensure(&id, &spec).await.unwrap();
    assert!(maps.get_opt(&new).await.unwrap().is_some());
    // The controller reconciles again and again: once the rollout is done an ensure removes the old one.
    eventually("the superseded file set to be deleted", 240, || async {
        h.ensure(&id, &spec).await.unwrap();
        maps.get_opt(&old).await.unwrap().is_none().then_some(())
    })
    .await;
    assert!(maps.get_opt(&new).await.unwrap().is_some());
    h.delete(&id).await.unwrap();
}

#[tokio::test]
async fn the_deployment_of_a_folder_agent_becomes_ready() {
    let Some(h) = make().await else { return };
    let id = fresh();
    let mut spec = bare(&id);
    spec.workloads[0].replicas = 2;
    h.ensure(&id, &spec).await.unwrap();
    let status = ready(&h, &id, 240).await;
    assert_eq!(status.replicas, 2);
    let deploy = Api::<Deployment>::namespaced(h.client.clone(), NAMESPACE)
        .get(id.name())
        .await
        .unwrap();
    assert_eq!(
        deploy.metadata.labels.unwrap()["app.kubernetes.io/component"],
        "agent"
    );
    // Suspend scales to zero, and ensure wakes it.
    assert_eq!(h.suspend(&id).await.unwrap().phase, Phase::Suspended);
    ready_after_wake(&h, &id, &spec).await;
    h.delete(&id).await.unwrap();
}

async fn ready_after_wake(h: &Harness, id: &RuntimeId, spec: &RuntimeSpec) {
    h.ensure(id, spec).await.unwrap();
    ready(h, id, 240).await;
}
