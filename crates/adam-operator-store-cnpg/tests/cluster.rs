//! The provisioner against a real API server with CloudNativePG installed: the conformance suite of
//! `adam-operator-ports`, and what only CloudNativePG shows (the cluster becoming ready, the Secret it makes, a
//! retained cluster found again).
//!
//! **Opt in.** The tests create namespace `adam-operator-test` and Clusters in it; they run only when
//! `ADAM_OPERATOR_TEST_KUBECONFIG` names a kubeconfig file (never the default context). Without it they skip, and
//! with `ADAM_OPERATOR_TEST_REQUIRE_CLUSTER=1` (CI) skipping is a failure. CloudNativePG must be installed
//! (`.github/workflows/operator.yml`, job `store-cnpg`, says how, pinned by version and checksum); a
//! cluster without it fails the first call with `NotInstalled`, which is the message of that failure. A
//! throwaway cluster is the point: Clusters of `Retain` stay.
//!
//! Every case has the Cluster names it made itself (`adam_operator_ports::testkit::unique`), so the cases run
//! side by side in one namespace.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::future::Future;
use std::time::Duration;

use adam_operator_ports::testkit::unique;
use adam_operator_ports::{
    CnpgSpec, DeletionPolicy, OwnerHandle, StoreId, StoreKind, StoreProvisioner, StoreSpec,
    StoreState, cnpg_connection,
};
use adam_operator_store_cnpg::CnpgStore;
use k8s_openapi::api::core::v1::{Namespace, Secret};
use kube::api::{Patch, PatchParams};
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::core::{ApiResource, DynamicObject, GroupVersionKind};
use kube::{Api, Client, Config};
use serde_json::json;

const KUBECONFIG_VAR: &str = "ADAM_OPERATOR_TEST_KUBECONFIG";
const REQUIRE_VAR: &str = "ADAM_OPERATOR_TEST_REQUIRE_CLUSTER";

/// The namespace the suite's ids live in (`adam_operator_ports::testkit` uses it for every id).
const NAMESPACE: &str = "adam-operator-test";

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
    client
        .apiserver_version()
        .await
        .unwrap_or_else(|e| panic!("the cluster of {path} does not answer: {e}"));
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
    Some(client)
}

async fn make() -> Option<CnpgStore> {
    connect().await.map(CnpgStore::new)
}

adam_operator_ports::store_provisioner_conformance!(make);

fn clusters(client: &Client) -> Api<DynamicObject> {
    let ar = ApiResource::from_gvk_with_plural(
        &GroupVersionKind::gvk("postgresql.cnpg.io", "v1", "Cluster"),
        "clusters",
    );
    Api::namespaced_with(client.clone(), NAMESPACE, &ar)
}

fn spec(deletion: DeletionPolicy) -> StoreSpec {
    StoreSpec {
        owner: OwnerHandle::none(),
        deletion,
        kind: StoreKind::Cnpg(CnpgSpec {
            instances: 1,
            size: "1Gi".to_owned(),
            // The cluster's default class (kind's `standard`).
            storage_class: None,
        }),
    }
}

async fn eventually<F, Fut, T>(what: &str, timeout: Duration, mut probe: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(v) = probe().await {
            return v;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out after {timeout:?} waiting for {what}"
        );
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

#[tokio::test]
async fn a_cluster_becomes_ready_and_its_secret_is_the_connection_the_status_names() {
    let Some(client) = connect().await else {
        return;
    };
    let store = CnpgStore::new(client.clone());
    let id = StoreId::new(NAMESPACE, unique("ready"));
    let first = store
        .ensure(&id, &spec(DeletionPolicy::Delete))
        .await
        .unwrap();
    assert_eq!(first.connection, cnpg_connection(id.name()));

    // CloudNativePG pulls a Postgres image and initialises a database: minutes, not seconds.
    eventually(
        "the Cluster to be ready",
        Duration::from_secs(600),
        || async {
            let status = store
                .ensure(&id, &spec(DeletionPolicy::Delete))
                .await
                .unwrap();
            (status.state == StoreState::ClusterReady).then_some(status)
        },
    )
    .await;

    // The harness (an administrator, unlike the operator) reads the Secret the status points at: it
    // exists, its key is there, and it is a Postgres URI. The value is never printed.
    let reference = cnpg_connection(id.name());
    let secret = Api::<Secret>::namespaced(client.clone(), NAMESPACE)
        .get(&reference.name)
        .await
        .unwrap_or_else(|e| panic!("the Secret {} the status names: {e}", reference.name));
    let uri = secret
        .data
        .as_ref()
        .and_then(|d| d.get(&reference.key))
        .unwrap_or_else(|| panic!("the Secret {} has no key {}", reference.name, reference.key));
    assert!(
        std::str::from_utf8(&uri.0).is_ok_and(|u| u.starts_with("postgresql://")),
        "the key {} is not a postgresql:// URI",
        reference.key
    );

    let outcome = store.release(&id).await.unwrap();
    assert!(outcome.existed && !outcome.retained);
    eventually(
        "the Cluster to be gone",
        Duration::from_secs(120),
        || async {
            clusters(&client)
                .get_opt(&format!("{}-db", id.name()))
                .await
                .unwrap()
                .is_none()
                .then_some(())
        },
    )
    .await;
}

#[tokio::test]
async fn a_retained_cluster_is_found_again_by_the_next_ensure() {
    let Some(client) = connect().await else {
        return;
    };
    let store = CnpgStore::new(client.clone());
    let id = StoreId::new(NAMESPACE, unique("again"));
    let name = format!("{}-db", id.name());
    store
        .ensure(&id, &spec(DeletionPolicy::Retain))
        .await
        .unwrap();
    let uid = clusters(&client).get(&name).await.unwrap().metadata.uid;
    assert!(uid.is_some());

    let outcome = store.release(&id).await.unwrap();
    assert!(outcome.existed && outcome.retained);
    let kept = clusters(&client).get(&name).await.unwrap();
    assert_eq!(kept.metadata.uid, uid, "the Cluster is the same object");
    assert!(
        kept.metadata
            .owner_references
            .as_ref()
            .is_none_or(Vec::is_empty),
        "nothing can garbage-collect it"
    );

    // The same service again: the adoption guard accepts its own label, and it is the same Cluster.
    store
        .ensure(&id, &spec(DeletionPolicy::Delete))
        .await
        .unwrap();
    assert_eq!(
        clusters(&client).get(&name).await.unwrap().metadata.uid,
        uid
    );
    store.release(&id).await.unwrap();
}

#[tokio::test]
async fn a_cluster_that_is_not_ours_is_left_alone() {
    let Some(client) = connect().await else {
        return;
    };
    let store = CnpgStore::new(client.clone());
    let id = StoreId::new(NAMESPACE, unique("foreign"));
    let name = format!("{}-db", id.name());
    // Someone else's Cluster (a Helm release's) of the name the operator would use.
    let foreign: DynamicObject = serde_json::from_value(json!({
        "apiVersion": "postgresql.cnpg.io/v1", "kind": "Cluster",
        "metadata": {"name": name, "labels": {"app.kubernetes.io/managed-by": "Helm"}},
        "spec": {"instances": 1, "storage": {"size": "1Gi"}}
    }))
    .unwrap();
    clusters(&client)
        .patch(
            &name,
            &PatchParams::apply("adam-operator-test-harness"),
            &Patch::Apply(&foreign),
        )
        .await
        .unwrap();

    let err = store
        .ensure(&id, &spec(DeletionPolicy::Delete))
        .await
        .unwrap_err();
    assert!(
        matches!(err, adam_operator_ports::StoreError::InvalidSpec(_)),
        "{err}"
    );
    let outcome = store.release(&id).await.unwrap();
    assert!(!outcome.existed);

    let after = clusters(&client).get(&name).await.unwrap();
    assert_eq!(after.data["spec"]["instances"], 1);
    assert_eq!(after.data["spec"]["storage"]["size"], "1Gi");
    assert!(
        after
            .metadata
            .managed_fields
            .iter()
            .flatten()
            .all(|m| m.manager.as_deref() != Some("adam-operator")),
        "no field of it belongs to the operator"
    );
    clusters(&client)
        .delete(&name, &kube::api::DeleteParams::background())
        .await
        .unwrap();
}
