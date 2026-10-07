//! The `Cluster` this provisioner makes: its names, its labels, the object that is applied, and the
//! reading of its status. Pure, so tests and tooling can read it.

use adam_operator_ports::{CnpgSpec, DeletionPolicy, StoreId, StoreState};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{ApiResource, GroupVersionKind};
use serde_json::{Value, json};

/// The field manager of every apply and patch: the runtime provider's, so one operator is one manager.
pub const FIELD_MANAGER: &str = "adam-operator";
/// `app.kubernetes.io/managed-by` on the Cluster, and its value: the adoption guard's test.
pub const MANAGED_BY_LABEL: &str = "app.kubernetes.io/managed-by";
/// The value of [`MANAGED_BY_LABEL`] on every object of the operator.
pub const MANAGED_BY_VALUE: &str = "adam-operator";
/// `app.kubernetes.io/instance`: the service the Cluster belongs to.
pub const INSTANCE_LABEL: &str = "app.kubernetes.io/instance";
/// `app.kubernetes.io/name`.
pub const NAME_LABEL: &str = "app.kubernetes.io/name";
/// What a release does to the data (`Retain` or `Delete`), remembered from the last `ensure`: the
/// same annotation the runtime provider writes on its workloads and claims.
pub const DELETION_POLICY_ANNOTATION: &str = "agents.vymalo.com/deletion-policy";

/// The group of CloudNativePG's API.
pub const GROUP: &str = "postgresql.cnpg.io";
/// Its version.
pub const VERSION: &str = "v1";
/// `group/version`, as a discovery request names it.
pub const API_VERSION: &str = "postgresql.cnpg.io/v1";
/// The kind.
pub const KIND: &str = "Cluster";
/// The plural of the kind.
pub const PLURAL: &str = "clusters";

/// `status.phase` of a healthy cluster. *Verified 2026-10-05* as what `kubectl get cluster` shows
/// ("Cluster in healthy state") in the documentation's own examples; its constant in the CloudNativePG
/// source was not read. The cluster test of CI is what holds it against a real operator.
pub const PHASE_HEALTHY: &str = "Cluster in healthy state";

/// The Cluster's name for a service: `<service>-db`, and CloudNativePG names the Secret of its
/// application user `<cluster>-app` ([`adam_operator_ports::cnpg_connection`]).
pub fn cluster_name(service: &str) -> String {
    format!("{service}-db")
}

/// How the dynamic client addresses a `Cluster`.
pub fn api_resource() -> ApiResource {
    ApiResource::from_gvk_with_plural(&GroupVersionKind::gvk(GROUP, VERSION, KIND), PLURAL)
}

/// The Cluster to apply for a service: its instances and storage, and the labels and the annotation
/// that make it ours. **No owner reference**: it is data.
pub fn render(id: &StoreId, deletion: DeletionPolicy, spec: &CnpgSpec) -> Value {
    let name = cluster_name(id.name());
    let mut storage = json!({"size": spec.size});
    if let Some(class) = &spec.storage_class {
        storage["storageClass"] = json!(class);
    }
    json!({
        "apiVersion": API_VERSION,
        "kind": KIND,
        "metadata": {
            "name": name,
            "namespace": id.scope(),
            "labels": {
                NAME_LABEL: name,
                INSTANCE_LABEL: id.name(),
                MANAGED_BY_LABEL: MANAGED_BY_VALUE,
            },
            "annotations": {
                DELETION_POLICY_ANNOTATION: match deletion {
                    DeletionPolicy::Retain => "Retain",
                    DeletionPolicy::Delete => "Delete",
                },
            },
        },
        "spec": {
            "instances": spec.instances,
            "storage": storage,
        },
    })
}

/// Whether a Cluster is this service's and ours: the adoption guard's test.
pub fn is_ours(meta: &ObjectMeta, id: &StoreId) -> bool {
    let label = |k: &str| {
        meta.labels
            .as_ref()
            .and_then(|l| l.get(k))
            .map(String::as_str)
    };
    label(MANAGED_BY_LABEL) == Some(MANAGED_BY_VALUE) && label(INSTANCE_LABEL) == Some(id.name())
}

/// The deletion policy remembered on a Cluster. `Retain` when there is none or it is not read:
/// data stays unless the policy clearly says otherwise.
pub fn policy_of(meta: &ObjectMeta) -> DeletionPolicy {
    match meta
        .annotations
        .as_ref()
        .and_then(|a| a.get(DELETION_POLICY_ANNOTATION))
        .map(String::as_str)
    {
        Some("Delete") => DeletionPolicy::Delete,
        _ => DeletionPolicy::Retain,
    }
}

/// How the Cluster is doing, from the part of the object a `DynamicObject` calls `data` (everything
/// but `metadata`): ready when CloudNativePG says the phase is healthy **and** every instance it was
/// asked for is ready. A Cluster with no status yet (just made) is not ready.
pub fn state_of(data: &Value) -> StoreState {
    let wanted = data["spec"]["instances"].as_u64().unwrap_or(1);
    let ready = data["status"]["readyInstances"].as_u64().unwrap_or(0);
    if data["status"]["phase"].as_str() == Some(PHASE_HEALTHY) && ready >= wanted {
        StoreState::ClusterReady
    } else {
        StoreState::ClusterNotReady
    }
}

/// Every string of a document, keys included.
pub fn collect_strings(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Array(items) => items.iter().for_each(|i| collect_strings(i, out)),
        Value::Object(map) => {
            for (k, child) in map {
                out.push(k.clone());
                collect_strings(child, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> CnpgSpec {
        CnpgSpec {
            instances: 2,
            size: "5Gi".to_owned(),
            storage_class: Some("longhorn".to_owned()),
        }
    }

    #[test]
    fn the_cluster_is_named_after_the_service_and_the_secret_follows_cloudnativepg() {
        assert_eq!(cluster_name("coder"), "coder-db");
        // `<cluster>-app`: the one definition of it is `adam_operator_ports::cnpg_connection`.
        assert_eq!(
            adam_operator_ports::cnpg_connection("coder").name,
            "coder-db-app"
        );
    }

    #[test]
    fn the_rendered_cluster_is_ours_and_has_no_owner() {
        let id = StoreId::new("ns", "coder");
        let object = render(&id, DeletionPolicy::Delete, &spec());
        assert_eq!(object["apiVersion"], "postgresql.cnpg.io/v1");
        assert_eq!(object["kind"], "Cluster");
        assert_eq!(object["metadata"]["name"], "coder-db");
        assert_eq!(object["metadata"]["namespace"], "ns");
        assert_eq!(object["spec"]["instances"], 2);
        assert_eq!(object["spec"]["storage"]["size"], "5Gi");
        assert_eq!(object["spec"]["storage"]["storageClass"], "longhorn");
        assert!(object["metadata"].get("ownerReferences").is_none());
        let meta: ObjectMeta = serde_json::from_value(object["metadata"].clone()).unwrap();
        assert!(is_ours(&meta, &id));
        assert!(!is_ours(&meta, &StoreId::new("ns", "other")));
        assert_eq!(policy_of(&meta), DeletionPolicy::Delete);
    }

    #[test]
    fn no_storage_class_is_the_clusters_default() {
        let mut s = spec();
        s.storage_class = None;
        let object = render(&StoreId::new("ns", "a"), DeletionPolicy::Retain, &s);
        assert!(object["spec"]["storage"].get("storageClass").is_none());
    }

    #[test]
    fn only_both_labels_make_a_cluster_ours_and_the_default_policy_keeps_data() {
        let id = StoreId::new("ns", "svc");
        let meta = |labels: &[(&str, &str)]| ObjectMeta {
            labels: Some(
                labels
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect(),
            ),
            ..ObjectMeta::default()
        };
        assert!(is_ours(
            &meta(&[(MANAGED_BY_LABEL, "adam-operator"), (INSTANCE_LABEL, "svc")]),
            &id
        ));
        assert!(!is_ours(
            &meta(&[(MANAGED_BY_LABEL, "Helm"), (INSTANCE_LABEL, "svc")]),
            &id
        ));
        assert!(!is_ours(&meta(&[(MANAGED_BY_LABEL, "adam-operator")]), &id));
        assert!(!is_ours(&ObjectMeta::default(), &id));
        assert_eq!(policy_of(&ObjectMeta::default()), DeletionPolicy::Retain);
    }

    #[test]
    fn ready_means_healthy_and_every_instance_ready() {
        let status = |phase: &str, ready: u64, wanted: u64| {
            state_of(&json!({"spec": {"instances": wanted},
                             "status": {"phase": phase, "readyInstances": ready}}))
        };
        assert_eq!(status(PHASE_HEALTHY, 2, 2), StoreState::ClusterReady);
        assert_eq!(status(PHASE_HEALTHY, 1, 2), StoreState::ClusterNotReady);
        assert_eq!(
            status("Setting up primary", 0, 1),
            StoreState::ClusterNotReady
        );
        assert_eq!(
            status("Upgrading cluster", 1, 1),
            StoreState::ClusterNotReady
        );
        assert_eq!(
            state_of(&json!({"spec": {"instances": 1}})),
            StoreState::ClusterNotReady,
            "no status yet"
        );
    }
}
