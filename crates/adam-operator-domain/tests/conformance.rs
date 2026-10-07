//! What `resolve` makes is accepted by a provider and a provisioner, and the controller can do its
//! work against the `Memory` implementations: the specs are the neutral types the ports take.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use adam_operator_ports::memory::{MemoryRuntime, MemoryStore};
use adam_operator_ports::{
    Phase, RuntimeProvider, StoreProvisioner, StoreState, Surface, cnpg_connection,
};
use common::{must_resolve, remove, set};
use serde_json::json;

fn variants() -> Vec<(&'static str, adam_operator_domain::ResolvedAgent)> {
    vec![
        ("coder", must_resolve("coder", |_, _| {})),
        ("chat", must_resolve("chat", |_, _| {})),
        (
            "coder split",
            must_resolve("coder", |s, c| {
                set(s, "/spec/scaling/topology", json!("split"));
                set(s, "/spec/scaling/workers", json!(2));
                set(s, "/spec/scaling/front", json!({"replicas": 2}));
                set(
                    c,
                    "/spec/harness/adam/coder/workspacePlacement",
                    json!("isolated"),
                );
            }),
        ),
        (
            "coder affinity",
            must_resolve("coder", |s, c| {
                set(s, "/spec/scaling/workers", json!(2));
                set(
                    c,
                    "/spec/harness/adam/coder/workspacePlacement",
                    json!("affinity"),
                );
                set(
                    c,
                    "/spec/environment/volumes/0/source/persistent/perReplica",
                    json!(false),
                );
            }),
        ),
        (
            "chat split",
            must_resolve("chat", |s, _| {
                set(s, "/spec/scaling/topology", json!("split"))
            }),
        ),
        (
            "coder token",
            must_resolve("coder", |_, c| {
                remove(c, "/spec/harness/adam/coder/github/app");
                set(
                    c,
                    "/spec/harness/adam/coder/github/token",
                    json!({"secretRef": {"name": "s", "key": "GITHUB_TOKEN"}}),
                );
            }),
        ),
    ]
}

#[test]
fn every_resolved_spec_passes_the_checks_of_the_ports() {
    for (name, r) in variants() {
        r.runtime.check().unwrap_or_else(|e| panic!("{name}: {e}"));
        r.store.check().unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}

#[tokio::test]
async fn a_provider_makes_every_resolved_runtime_and_a_provisioner_every_store() {
    let provider = MemoryRuntime::new();
    let store = MemoryStore::new();
    for (name, r) in variants() {
        let status = provider
            .ensure(&r.id, &r.runtime)
            .await
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(status.phase, Phase::Ready, "{name}");
        assert_eq!(status.replicas, r.runtime.worker_replicas(), "{name}");
        let card = provider.endpoint(&r.id, Surface::AgentCard).await.unwrap();
        assert!(card.url.ends_with("/.well-known/agent-card.json"));
        let s = store.ensure(&r.store_id, &r.store).await.unwrap();
        assert_eq!(s.state, StoreState::SecretReferenced, "{name}");
        // The connection the provisioner reports is the one DATABASE_URL reads.
        let db = r.runtime.workloads[0]
            .container
            .env
            .iter()
            .find(|e| e.name == "DATABASE_URL")
            .unwrap();
        assert_eq!(
            db.value,
            adam_operator_ports::EnvValue::Secret(s.connection),
            "{name}"
        );
    }
}

#[tokio::test]
async fn a_cluster_store_and_the_database_variable_agree() {
    let r = must_resolve("coder", |s, _| {
        remove(s, "/spec/store/postgres/secretRef");
        set(
            s,
            "/spec/store/postgres/cnpg",
            json!({"instances": 1, "storage": {"size": "5Gi"}}),
        );
    });
    let status = MemoryStore::new()
        .ensure(&r.store_id, &r.store)
        .await
        .unwrap();
    assert_eq!(status.connection, cnpg_connection("coder"));
    let db = r.runtime.workloads[0]
        .container
        .env
        .iter()
        .find(|e| e.name == "DATABASE_URL")
        .unwrap();
    assert_eq!(
        db.value,
        adam_operator_ports::EnvValue::Secret(status.connection)
    );
}

#[tokio::test]
async fn a_changed_config_is_a_new_digest_on_the_same_runtime() {
    let provider = MemoryRuntime::new();
    let a = must_resolve("coder", |_, _| {});
    let b = must_resolve("coder", |_, c| {
        set(c, "/spec/harness/adam/coder/maxCheckCycles", json!(5))
    });
    assert_eq!(a.id, b.id);
    provider.ensure(&a.id, &a.runtime).await.unwrap();
    provider.ensure(&b.id, &b.runtime).await.unwrap();
    assert_eq!(provider.spec(&b.id).unwrap().digest, b.digest);
    assert_ne!(a.digest, b.digest);
}
