//! The provisioner against a fake API server: the requests it makes, in order, and what it does with
//! the answers. What the fake cannot show (CloudNativePG making a database, the garbage collector, the
//! API server's refusals) is `tests/cluster.rs`'s.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use adam_operator_ports::{
    Classify, CnpgSpec, DeletionPolicy, ErrorClass, OwnerHandle, SecretRef, StoreId, StoreKind,
    StoreProvisioner, StoreSpec, StoreState, cnpg_connection,
};
use adam_operator_store_cnpg::CnpgStore;
use serde_json::{Value, json};
use support::Fake;

const NS: &str = "adam-operator-test";

async fn make() -> Option<CnpgStore> {
    // Each case of the suite has a fake of its own: the ids are unique anyway.
    Some(CnpgStore::new(Fake::new().client()))
}

adam_operator_ports::store_provisioner_conformance!(make);

fn id() -> StoreId {
    StoreId::new(NS, "coder")
}

fn cnpg(deletion: DeletionPolicy) -> StoreSpec {
    StoreSpec {
        owner: OwnerHandle::new("an owner this provisioner must not use"),
        deletion,
        kind: StoreKind::Cnpg(CnpgSpec {
            instances: 2,
            size: "5Gi".to_owned(),
            storage_class: Some("longhorn".to_owned()),
        }),
    }
}

fn setup() -> (Fake, CnpgStore) {
    let fake = Fake::new();
    let store = CnpgStore::new(fake.client());
    (fake, store)
}

fn healthy(ready: u64) -> Value {
    json!({"phase": "Cluster in healthy state", "readyInstances": ready})
}

fn ours(instance: &str, policy: &str) -> Value {
    json!({"metadata": {
        "labels": {"app.kubernetes.io/managed-by": "adam-operator", "app.kubernetes.io/instance": instance},
        "annotations": {"agents.vymalo.com/deletion-policy": policy},
    }, "spec": {"instances": 1}})
}

#[tokio::test]
async fn it_applies_the_cluster_by_server_side_apply_after_asking_the_api_and_the_name() {
    let (fake, store) = setup();
    let status = store
        .ensure(&id(), &cnpg(DeletionPolicy::Retain))
        .await
        .unwrap();
    assert_eq!(status.state, StoreState::ClusterNotReady, "no status yet");
    assert_eq!(status.connection, cnpg_connection("coder"));
    assert_eq!(status.connection, SecretRef::new("coder-db-app", "uri"));

    let calls = fake.calls();
    let shape: Vec<(&str, &str)> = calls
        .iter()
        .map(|c| (c.method.as_str(), c.path.as_str()))
        .collect();
    assert_eq!(
        shape,
        [
            ("GET", "/apis/postgresql.cnpg.io/v1"),
            (
                "GET",
                "/apis/postgresql.cnpg.io/v1/namespaces/adam-operator-test/clusters/coder-db"
            ),
            (
                "PATCH",
                "/apis/postgresql.cnpg.io/v1/namespaces/adam-operator-test/clusters/coder-db"
            ),
        ],
        "discovery, the adoption guard's read, then the write"
    );
    let apply = &calls[2];
    assert!(
        apply.content_type.contains("apply-patch"),
        "{}",
        apply.content_type
    );
    assert!(
        apply.query.contains("fieldManager=adam-operator"),
        "{}",
        apply.query
    );
    assert!(apply.query.contains("force=true"), "{}", apply.query);
    let body = apply.body.as_ref().unwrap();
    assert_eq!(body["apiVersion"], "postgresql.cnpg.io/v1");
    assert_eq!(body["kind"], "Cluster");
    assert_eq!(body["spec"]["instances"], 2);
    assert_eq!(
        body["spec"]["storage"],
        json!({"size": "5Gi", "storageClass": "longhorn"})
    );
    assert_eq!(
        body["metadata"]["labels"]["app.kubernetes.io/managed-by"],
        "adam-operator"
    );
    assert_eq!(
        body["metadata"]["labels"]["app.kubernetes.io/instance"],
        "coder"
    );
    assert_eq!(
        body["metadata"]["annotations"]["agents.vymalo.com/deletion-policy"],
        "Retain"
    );
    assert!(
        body["metadata"].get("ownerReferences").is_none(),
        "a database is data: nothing garbage-collects it with the service"
    );
}

#[tokio::test]
async fn readiness_is_the_clusters_own_status() {
    let (fake, store) = setup();
    let spec = cnpg(DeletionPolicy::Retain);
    store.ensure(&id(), &spec).await.unwrap();

    fake.set_status(NS, "coder-db", healthy(1));
    assert_eq!(
        store.ensure(&id(), &spec).await.unwrap().state,
        StoreState::ClusterNotReady,
        "two instances were asked for and one is ready"
    );
    fake.set_status(NS, "coder-db", healthy(2));
    assert_eq!(
        store.ensure(&id(), &spec).await.unwrap().state,
        StoreState::ClusterReady
    );
    fake.set_status(
        NS,
        "coder-db",
        json!({"phase": "Upgrading cluster", "readyInstances": 2}),
    );
    assert_eq!(
        store.ensure(&id(), &spec).await.unwrap().state,
        StoreState::ClusterNotReady
    );
}

#[tokio::test]
async fn a_second_ensure_changes_what_the_spec_changed_and_keeps_the_status() {
    let (fake, store) = setup();
    store
        .ensure(&id(), &cnpg(DeletionPolicy::Retain))
        .await
        .unwrap();
    fake.set_status(NS, "coder-db", healthy(3));
    let mut spec = cnpg(DeletionPolicy::Delete);
    if let StoreKind::Cnpg(c) = &mut spec.kind {
        c.instances = 3;
    }
    let status = store.ensure(&id(), &spec).await.unwrap();
    assert_eq!(status.state, StoreState::ClusterReady);
    let object = fake.get(NS, "coder-db").unwrap();
    assert_eq!(object["spec"]["instances"], 3);
    assert_eq!(
        object["metadata"]["annotations"]["agents.vymalo.com/deletion-policy"], "Delete",
        "the policy of the last ensure is what a release reads back"
    );
}

#[tokio::test]
async fn without_cloudnativepg_it_is_not_installed_and_writes_nothing() {
    let (fake, store) = setup();
    fake.set_installed(false);
    let err = store
        .ensure(&id(), &cnpg(DeletionPolicy::Retain))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            adam_operator_ports::StoreError::NotInstalled {
                what: "CloudNativePG"
            }
        ),
        "{err:?}"
    );
    assert_eq!(err.class(), ErrorClass::Unsupported);
    assert!(fake.writes().is_empty());
    assert_eq!(fake.calls().len(), 1, "only the discovery");
}

#[tokio::test]
async fn an_api_group_without_clusters_is_not_installed_either() {
    // A group that exists but does not serve `clusters` (an older or partial install).
    let (fake, store) = setup();
    fake.set_serves_clusters(false);
    let err = store
        .ensure(&id(), &cnpg(DeletionPolicy::Retain))
        .await
        .unwrap_err();
    assert_eq!(err.class(), ErrorClass::Unsupported);
    assert!(fake.writes().is_empty());
}

#[tokio::test]
async fn a_cluster_that_is_not_ours_is_never_written_to() {
    let (fake, store) = setup();
    fake.put(
        NS,
        "coder-db",
        json!({"metadata": {"labels": {"app.kubernetes.io/managed-by": "Helm"}}, "spec": {"instances": 1}}),
    );
    let before = fake.get(NS, "coder-db").unwrap();
    let err = store
        .ensure(&id(), &cnpg(DeletionPolicy::Delete))
        .await
        .unwrap_err();
    assert_eq!(err.class(), ErrorClass::Invalid, "{err}");
    assert!(err.to_string().contains("coder-db"), "{err}");
    assert!(fake.writes().is_empty());
    assert_eq!(fake.get(NS, "coder-db").unwrap(), before);

    // Nor does a release touch it, whatever the policy.
    let outcome = store.release(&id()).await.unwrap();
    assert!(!outcome.existed && !outcome.retained);
    assert!(fake.writes().is_empty());
    assert!(fake.get(NS, "coder-db").is_some());
}

#[tokio::test]
async fn another_services_cluster_is_not_ours_for_this_one() {
    let (fake, store) = setup();
    fake.put(NS, "coder-db", ours("someone-else", "Retain"));
    let err = store
        .ensure(&id(), &cnpg(DeletionPolicy::Retain))
        .await
        .unwrap_err();
    assert_eq!(err.class(), ErrorClass::Invalid);
    assert!(fake.writes().is_empty());
}

#[tokio::test]
async fn retain_keeps_the_cluster_and_strips_owners_and_delete_removes_it() {
    let (fake, store) = setup();
    // Retain: ours, with an owner reference somebody added.
    let mut kept = ours("coder", "Retain");
    kept["metadata"]["ownerReferences"] = json!([{"apiVersion": "agents.vymalo.com/v1alpha1",
        "kind": "AgentService", "name": "coder", "uid": "u"}]);
    fake.put(NS, "coder-db", kept);
    let outcome = store.release(&id()).await.unwrap();
    assert!(outcome.existed && outcome.retained);
    let object = fake.get(NS, "coder-db").unwrap();
    assert!(
        object["metadata"].get("ownerReferences").is_none(),
        "{object}"
    );
    assert!(fake.calls().iter().all(|c| c.method != "DELETE"));

    // A second release of a retained Cluster says the same and writes nothing more.
    fake.clear_calls();
    let again = store.release(&id()).await.unwrap();
    assert!(again.existed && again.retained);
    assert!(fake.writes().is_empty(), "{:?}", fake.writes());

    // Delete.
    fake.put(NS, "coder-db", ours("coder", "Delete"));
    let outcome = store.release(&id()).await.unwrap();
    assert!(outcome.existed && !outcome.retained);
    assert!(fake.get(NS, "coder-db").is_none());
    assert!(
        !store.release(&id()).await.unwrap().existed,
        "the second release finds nothing"
    );
}

#[tokio::test]
async fn a_policy_that_is_not_read_keeps_the_data() {
    let (fake, store) = setup();
    let mut cluster = ours("coder", "garbage");
    cluster["metadata"]["annotations"] = json!({});
    fake.put(NS, "coder-db", cluster);
    let outcome = store.release(&id()).await.unwrap();
    assert!(outcome.existed && outcome.retained);
    assert!(fake.get(NS, "coder-db").is_some());
}

#[tokio::test]
async fn releasing_without_the_api_is_a_success_with_nothing_there() {
    let (fake, store) = setup();
    fake.set_installed(false);
    let outcome = store.release(&id()).await.unwrap();
    assert!(!outcome.existed && !outcome.retained);
    assert!(fake.writes().is_empty());
}

#[tokio::test]
async fn a_referenced_secret_is_served_with_no_call_at_all() {
    let (fake, store) = setup();
    let spec = StoreSpec {
        owner: OwnerHandle::none(),
        deletion: DeletionPolicy::Delete,
        kind: StoreKind::Secret(SecretRef::new("coder-db-uri", "uri")),
    };
    let status = store.ensure(&id(), &spec).await.unwrap();
    assert_eq!(status.state, StoreState::SecretReferenced);
    assert_eq!(status.connection, SecretRef::new("coder-db-uri", "uri"));
    let outcome = store.release(&id()).await.unwrap();
    assert!(outcome.existed && !outcome.retained);
    // Only the release looks for a Cluster of that name; nothing is written.
    assert!(fake.writes().is_empty());
}

#[tokio::test]
async fn a_failing_api_server_is_transient_and_a_refusal_is_invalid() {
    let (fake, store) = setup();
    fake.fail_with(Some(503));
    let err = store
        .ensure(&id(), &cnpg(DeletionPolicy::Retain))
        .await
        .unwrap_err();
    assert_eq!(err.class(), ErrorClass::Transient, "{err}");
    let err = store.release(&id()).await.unwrap_err();
    assert_eq!(err.class(), ErrorClass::Transient, "{err}");
    fake.fail_with(Some(403));
    let err = store
        .ensure(&id(), &cnpg(DeletionPolicy::Retain))
        .await
        .unwrap_err();
    assert_eq!(
        err.class(),
        ErrorClass::Transient,
        "a missing right is retried like an outage"
    );
}

#[tokio::test]
async fn nothing_it_writes_names_a_secret_value() {
    let (_fake, store) = setup();
    store
        .ensure(&id(), &cnpg(DeletionPolicy::Retain))
        .await
        .unwrap();
    let text = store.plain_text(&id()).await.unwrap();
    assert!(text.contains(&"coder-db".to_owned()));
    // The connection Secret is a reference in the status the controller reports, never in the Cluster.
    assert!(!text.iter().any(|t| t.contains("coder-db-app")), "{text:?}");
    assert!(!text.contains(&"managedFields".to_owned()));
}
