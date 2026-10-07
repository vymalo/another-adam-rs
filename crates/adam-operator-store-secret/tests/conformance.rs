//! The conformance suite of `adam-operator-ports` against `SecretStore`. It needs no backend (the provisioner has
//! none), so nothing here skips.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use adam_operator_ports::{
    Classify, DeletionPolicy, ErrorClass, OwnerHandle, SecretRef, StoreId, StoreKind,
    StoreProvisioner, StoreSpec, StoreState,
};
use adam_operator_store_secret::SecretStore;

async fn make() -> Option<SecretStore> {
    Some(SecretStore::new())
}

adam_operator_ports::store_provisioner_conformance!(make);

fn spec(name: &str, key: &str, deletion: DeletionPolicy) -> StoreSpec {
    StoreSpec {
        owner: OwnerHandle::none(),
        deletion,
        kind: StoreKind::Secret(SecretRef::new(name, key)),
    }
}

#[tokio::test]
async fn it_reports_the_reference_it_was_given_under_both_policies() {
    let store = SecretStore::new();
    for deletion in [DeletionPolicy::Retain, DeletionPolicy::Delete] {
        let id = StoreId::new("ns", "coder");
        let status = store
            .ensure(&id, &spec("coder-db-uri", "uri", deletion))
            .await
            .unwrap();
        assert_eq!(status.state, StoreState::SecretReferenced);
        assert_eq!(status.connection, SecretRef::new("coder-db-uri", "uri"));
        let outcome = store.release(&id).await.unwrap();
        assert!(outcome.existed);
        assert!(
            !outcome.retained,
            "a Secret someone else owns is never ours to retain or delete"
        );
    }
}

#[tokio::test]
async fn it_does_not_serve_clusters() {
    let store = SecretStore::new();
    assert!(!store.capabilities().cnpg);
    let cluster = StoreSpec {
        owner: OwnerHandle::none(),
        deletion: DeletionPolicy::Retain,
        kind: StoreKind::Cnpg(adam_operator_ports::CnpgSpec {
            instances: 1,
            size: "5Gi".to_owned(),
            storage_class: None,
        }),
    };
    let err = store
        .ensure(&StoreId::new("ns", "coder"), &cluster)
        .await
        .unwrap_err();
    assert_eq!(err.class(), ErrorClass::Unsupported, "{err}");
}

#[tokio::test]
async fn two_services_are_two_stores() {
    let store = SecretStore::new();
    let (a, b) = (StoreId::new("ns", "a"), StoreId::new("ns", "b"));
    store
        .ensure(&a, &spec("s", "uri", DeletionPolicy::Retain))
        .await
        .unwrap();
    assert!(!store.release(&b).await.unwrap().existed);
    assert!(store.release(&a).await.unwrap().existed);
}
