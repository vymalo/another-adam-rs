//! The provider against a fake API server: the requests it makes, in order, and what it does with
//! the answers. What the fake cannot show (a controller making pods, the garbage collector, the
//! kubelet's words) is `tests/cluster.rs`'s.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use adam_operator_ports::testkit::sample_spec;
use adam_operator_ports::{
    Classify, ErrorClass, IssueReason, Phase, Role, RuntimeId, RuntimeProvider, RuntimeSpec,
    Surface,
};
use adam_operator_runtime_kubernetes::KubernetesRuntime;
use serde_json::{Value, json};
use support::{Call, Fake, K};

const NS: &str = "adam-operator-test";

fn id() -> RuntimeId {
    RuntimeId::new(NS, "rt")
}

fn setup() -> (Fake, KubernetesRuntime) {
    let fake = Fake::new();
    let runtime = KubernetesRuntime::new(fake.client());
    (fake, runtime)
}

fn ours(instance: &str, extra: Value) -> Value {
    let mut o = json!({"metadata": {"labels": {
        "app.kubernetes.io/managed-by": "adam-operator",
        "app.kubernetes.io/instance": instance,
        "app.kubernetes.io/name": instance,
    }}});
    merge(&mut o, &extra);
    o
}

fn merge(target: &mut Value, extra: &Value) {
    if let (Some(t), Some(e)) = (target.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            match t.get_mut(k) {
                Some(existing) if existing.is_object() && v.is_object() => merge(existing, v),
                _ => {
                    t.insert(k.clone(), v.clone());
                }
            }
        }
    }
}

fn workload(role: &str, replicas: u32, annotations: Value, status: Value) -> Value {
    ours(
        "rt",
        json!({
            "metadata": {"labels": {"app.kubernetes.io/component": role}, "annotations": annotations},
            "spec": {"replicas": replicas, "selector": {}, "template": {"spec": {"containers": []}},
                     "serviceName": "rt"},
            "status": status,
        }),
    )
}

fn claim(name: &str, owners: Value) -> Value {
    ours(
        "rt",
        json!({"metadata": {
            "name": name,
            "labels": {"agents.vymalo.com/volume": "work"},
            "ownerReferences": owners,
        }}),
    )
}

fn kinds_written(calls: &[Call], method: &str) -> Vec<(K, String)> {
    calls
        .iter()
        .filter(|c| c.method == method)
        .map(|c| (c.kind(), c.name().to_owned()))
        .collect()
}

// ---------------------------------------------------------------- ensure

#[tokio::test]
async fn ensure_asks_before_it_writes_and_applies_with_the_field_manager() {
    let (fake, runtime) = setup();
    let spec = sample_spec(&id());
    let status = runtime.ensure(&id(), &spec).await.unwrap();
    assert_eq!(
        status.phase,
        Phase::Provisioning,
        "no controller has made pods"
    );

    let calls = fake.calls();
    let first_write = calls.iter().position(Call::writes).unwrap();
    assert!(
        calls[..first_write].iter().all(|c| c.method == "GET"),
        "the adoption guard reads before anything is written"
    );
    let patches = kinds_written(&calls, "PATCH");
    assert_eq!(
        patches,
        [
            (K::ConfigMap, "rt-agent-0a1b2c3d".to_owned()),
            (K::Service, "rt".to_owned()),
            (K::StatefulSet, "rt".to_owned()),
        ],
        "file sets before the pods that mount them, the Service before the set it governs"
    );
    for call in calls.iter().filter(|c| c.method == "PATCH") {
        assert!(
            call.query.contains("fieldManager=adam-operator"),
            "{}",
            call.query
        );
        assert!(call.query.contains("force=true"), "{}", call.query);
        assert!(
            call.content_type.contains("apply-patch"),
            "{}",
            call.content_type
        );
    }
    assert!(!calls.iter().any(|c| c.method == "DELETE"));

    let sts = fake.get(K::StatefulSet, NS, "rt").unwrap();
    assert_eq!(sts["spec"]["replicas"], 2);
    assert_eq!(
        sts["metadata"]["labels"]["app.kubernetes.io/managed-by"],
        "adam-operator"
    );
    assert_eq!(
        sts["spec"]["template"]["metadata"]["annotations"]["agents.vymalo.com/config-digest"],
        spec.digest
    );
}

#[tokio::test]
async fn ensure_twice_applies_the_same_objects() {
    let (fake, runtime) = setup();
    let spec = sample_spec(&id());
    runtime.ensure(&id(), &spec).await.unwrap();
    let first = fake.writes();
    fake.clear_calls();
    runtime.ensure(&id(), &spec).await.unwrap();
    let second = fake.writes();
    let bodies = |calls: &[Call]| calls.iter().map(|c| c.body.clone()).collect::<Vec<_>>();
    assert_eq!(bodies(&first), bodies(&second));
    assert_eq!(fake.all(K::StatefulSet, NS).len(), 1);
}

#[tokio::test]
async fn a_malformed_spec_makes_no_request() {
    let (fake, runtime) = setup();
    let mut spec = sample_spec(&id());
    spec.network.selects = "nothing".to_owned();
    let err = runtime.ensure(&id(), &spec).await.unwrap_err();
    assert_eq!(err.class(), ErrorClass::Invalid);
    assert!(fake.calls().is_empty());

    // The provider's own rules, which `check` does not have, are refused before any request too.
    let mut spec = sample_spec(&id());
    spec.workloads[0].stable_identity = false;
    let err = runtime.ensure(&id(), &spec).await.unwrap_err();
    assert_eq!(err.class(), ErrorClass::Invalid, "{err}");
    assert!(fake.calls().is_empty());
}

// ---------------------------------------------------------------- the adoption guard

fn helm(name: &str) -> Value {
    json!({"metadata": {"labels": {
        "app.kubernetes.io/managed-by": "Helm",
        "app.kubernetes.io/instance": name,
        "app.kubernetes.io/name": name,
    }}, "spec": {"keep": "me"}})
}

#[tokio::test]
async fn an_object_that_is_not_ours_is_a_name_conflict_and_nothing_is_changed() {
    for (kind, desired) in [
        (K::Service, "Service rt"),
        (K::StatefulSet, "StatefulSet rt"),
        (K::ConfigMap, "ConfigMap rt-agent-0a1b2c3d"),
    ] {
        let (fake, runtime) = setup();
        let name = if kind == K::ConfigMap {
            "rt-agent-0a1b2c3d"
        } else {
            "rt"
        };
        fake.put(kind, NS, name, helm("rt"));
        let before = fake.get(kind, NS, name).unwrap();

        let status = runtime.ensure(&id(), &sample_spec(&id())).await.unwrap();
        assert!(status.is_name_conflict(), "{kind:?}: {status:?}");
        assert_eq!(status.phase, Phase::Absent, "nothing of ours runs");
        assert!(
            status.issues.iter().any(|i| i.message.contains(desired)),
            "{kind:?} {desired}: {:?}",
            status.issues
        );
        assert!(fake.writes().is_empty(), "{kind:?}: {:?}", fake.writes());
        assert_eq!(
            fake.get(kind, NS, name).unwrap(),
            before,
            "the foreign object is untouched"
        );
        // And later `status` calls keep saying so while the name is taken.
        if kind != K::ConfigMap {
            assert!(runtime.status(&id()).await.unwrap().is_name_conflict());
        }
    }
}

#[tokio::test]
async fn an_object_with_our_label_for_another_service_is_not_this_services() {
    // `coder-front` the service and the front of `coder` would name the same Deployment.
    let (fake, runtime) = setup();
    fake.put(K::Service, NS, "rt", ours("someone-else", json!({})));
    let status = runtime.ensure(&id(), &sample_spec(&id())).await.unwrap();
    assert!(status.is_name_conflict());
    assert!(fake.writes().is_empty());
}

#[tokio::test]
async fn a_conflict_keeps_reporting_what_of_ours_runs() {
    let (fake, runtime) = setup();
    fake.put(
        K::StatefulSet,
        NS,
        "rt",
        workload("agent", 2, json!({}), json!(null)),
    );
    fake.put(K::Service, NS, "rt", helm("rt"));
    let status = runtime.ensure(&id(), &sample_spec(&id())).await.unwrap();
    assert_eq!(
        status.phase,
        Phase::Provisioning,
        "what is ours is still reported"
    );
    assert!(status.is_name_conflict());
    assert!(fake.writes().is_empty());
}

#[tokio::test]
async fn once_the_name_is_free_the_runtime_is_made() {
    let (fake, runtime) = setup();
    fake.put(K::Service, NS, "rt", helm("rt"));
    assert!(
        runtime
            .ensure(&id(), &sample_spec(&id()))
            .await
            .unwrap()
            .is_name_conflict()
    );
    // Argo prunes the release's objects: its owner deletes it, through the API.
    kube::Api::<k8s_openapi::api::core::v1::Service>::namespaced(fake.client(), NS)
        .delete("rt", &kube::api::DeleteParams::default())
        .await
        .unwrap();
    let status = runtime.ensure(&id(), &sample_spec(&id())).await.unwrap();
    assert!(!status.is_name_conflict());
    assert_eq!(status.phase, Phase::Provisioning);
    assert_eq!(
        fake.get(K::Service, NS, "rt").unwrap()["metadata"]["labels"]["app.kubernetes.io/managed-by"],
        "adam-operator"
    );
}

// ---------------------------------------------------------------- stale objects

fn ready_status(replicas: u32) -> Value {
    json!({"observedGeneration": 1, "replicas": replicas, "readyReplicas": replicas,
           "updatedReplicas": replicas, "currentRevision": "a", "updateRevision": "a"})
}

#[tokio::test]
async fn what_the_spec_no_longer_asks_for_is_deleted_and_file_sets_wait_for_the_rollout() {
    let (fake, runtime) = setup();
    // An earlier spec had a policy, a budget, a front and an older folder.
    fake.put(K::NetworkPolicy, NS, "rt", ours("rt", json!({})));
    fake.put(K::Budget, NS, "rt-front", ours("rt", json!({})));
    fake.put(
        K::Deployment,
        NS,
        "rt-front",
        workload("front", 2, json!({}), json!(null)),
    );
    fake.put(K::ConfigMap, NS, "rt-agent-00000000", ours("rt", json!({})));
    fake.put(K::ConfigMap, NS, "unrelated", ours("other", json!({})));

    // Not rolled out yet: the old pods may still mount the old file set.
    let spec = sample_spec(&id());
    let status = runtime.ensure(&id(), &spec).await.unwrap();
    assert_eq!(status.phase, Phase::Provisioning);
    let deleted = kinds_written(&fake.calls(), "DELETE");
    assert!(
        deleted.contains(&(K::NetworkPolicy, "rt".to_owned())),
        "{deleted:?}"
    );
    assert!(
        deleted.contains(&(K::Budget, "rt-front".to_owned())),
        "{deleted:?}"
    );
    assert!(
        deleted.contains(&(K::Deployment, "rt-front".to_owned())),
        "{deleted:?}"
    );
    assert!(
        !deleted.iter().any(|(k, _)| *k == K::ConfigMap),
        "a file set a pod may still mount stays until the rollout is done: {deleted:?}"
    );

    // The set rolls out: the next ensure removes the superseded file set, and only ours.
    fake.set_status(K::StatefulSet, NS, "rt", ready_status(2));
    let status = runtime.ensure(&id(), &spec).await.unwrap();
    assert_eq!(status.phase, Phase::Ready);
    assert_eq!(status.replicas, 2);
    assert!(fake.get(K::ConfigMap, NS, "rt-agent-00000000").is_none());
    assert!(fake.get(K::ConfigMap, NS, "rt-agent-0a1b2c3d").is_some());
    assert!(
        fake.get(K::ConfigMap, NS, "unrelated").is_some(),
        "another service's object"
    );
}

#[tokio::test]
async fn a_workload_of_the_other_kind_is_replaced() {
    let (fake, runtime) = setup();
    fake.put(
        K::Deployment,
        NS,
        "rt",
        workload("agent", 2, json!({}), json!(null)),
    );
    runtime.ensure(&id(), &sample_spec(&id())).await.unwrap();
    assert!(
        fake.get(K::StatefulSet, NS, "rt").is_some(),
        "the spec has a stable identity"
    );
    assert!(fake.get(K::Deployment, NS, "rt").is_none());
}

// ---------------------------------------------------------------- suspend

#[tokio::test]
async fn suspend_patches_replicas_alone_and_ensure_wakes() {
    let (fake, runtime) = setup();
    let spec = sample_spec(&id());
    runtime.ensure(&id(), &spec).await.unwrap();
    fake.clear_calls();

    let status = runtime.suspend(&id()).await.unwrap();
    assert_eq!(status.phase, Phase::Suspended);
    assert_eq!(status.replicas, 0);
    let patches: Vec<Call> = fake
        .calls()
        .into_iter()
        .filter(|c| c.method == "PATCH")
        .collect();
    assert_eq!(patches.len(), 1);
    assert!(patches[0].content_type.contains("merge-patch"));
    assert_eq!(patches[0].body, Some(json!({"spec": {"replicas": 0}})));
    assert!(patches[0].query.contains("fieldManager=adam-operator"));
    let sts = fake.get(K::StatefulSet, NS, "rt").unwrap();
    assert_eq!(sts["spec"]["replicas"], 2 - 2, "scaled to zero");
    assert!(
        sts["spec"]["template"]["spec"]["containers"].is_array(),
        "the rest is kept"
    );

    // Waking is an ensure of the same spec.
    runtime.ensure(&id(), &spec).await.unwrap();
    assert_eq!(
        fake.get(K::StatefulSet, NS, "rt").unwrap()["spec"]["replicas"],
        2
    );
}

#[tokio::test]
async fn a_suspended_spec_applies_zero_replicas() {
    let (fake, runtime) = setup();
    let mut spec = sample_spec(&id());
    spec.suspend = true;
    let status = runtime.ensure(&id(), &spec).await.unwrap();
    assert_eq!(status.phase, Phase::Suspended);
    assert_eq!(
        fake.get(K::StatefulSet, NS, "rt").unwrap()["spec"]["replicas"],
        0
    );
}

#[tokio::test]
async fn suspending_what_does_not_exist_is_not_found() {
    let (fake, runtime) = setup();
    let err = runtime.suspend(&id()).await.unwrap_err();
    assert_eq!(err.class(), ErrorClass::NotFound);
    assert!(fake.writes().is_empty());
}

// ---------------------------------------------------------------- delete

fn seed_runtime(fake: &Fake, policy: &str) {
    let owner = json!([{"apiVersion": "apps/v1", "kind": "StatefulSet", "name": "rt", "uid": "u",
                        "controller": true}]);
    let mut sts = workload(
        "agent",
        2,
        json!({"agents.vymalo.com/deletion-policy": policy}),
        json!(null),
    );
    sts["spec"]["volumeClaimTemplates"] = json!([{"metadata": {"name": "work"}}]);
    fake.put(K::StatefulSet, NS, "rt", sts);
    fake.put(K::Service, NS, "rt", ours("rt", json!({})));
    fake.put(K::NetworkPolicy, NS, "rt", ours("rt", json!({})));
    fake.put(K::ConfigMap, NS, "rt-mcp", ours("rt", json!({})));
    fake.put(K::Claim, NS, "work-rt-0", claim("work-rt-0", owner.clone()));
    fake.put(K::Claim, NS, "work-rt-1", claim("work-rt-1", owner));
    // Not this service's: never touched.
    fake.put(
        K::Claim,
        NS,
        "work-other-0",
        claim("work-other-0", json!([])),
    );
    fake.all(K::Claim, NS);
    let mut other = fake.get(K::Claim, NS, "work-other-0").unwrap();
    other["metadata"]["labels"]["app.kubernetes.io/instance"] = json!("other");
    fake.put(K::Claim, NS, "work-other-0", other);
}

#[tokio::test]
async fn retain_removes_the_compute_keeps_the_claims_and_lets_the_garbage_collector_go_by_them() {
    let (fake, runtime) = setup();
    seed_runtime(&fake, "Retain");
    let outcome = runtime.delete(&id()).await.unwrap();
    assert!(outcome.existed);
    assert_eq!(outcome.retained_volumes, ["work"]);

    let deleted = kinds_written(&fake.calls(), "DELETE");
    for compute in [
        (K::StatefulSet, "rt"),
        (K::Service, "rt"),
        (K::NetworkPolicy, "rt"),
        (K::ConfigMap, "rt-mcp"),
    ] {
        assert!(
            deleted.contains(&(compute.0, compute.1.to_owned())),
            "{compute:?} in {deleted:?}"
        );
    }
    assert!(
        !deleted.iter().any(|(k, _)| *k == K::Claim),
        "Retain keeps every claim: {deleted:?}"
    );
    for claim in ["work-rt-0", "work-rt-1"] {
        let c = fake.get(K::Claim, NS, claim).unwrap();
        assert_eq!(
            c["metadata"]["ownerReferences"],
            json!([]),
            "the set's owner reference is stripped so nothing collects the claim: {c}"
        );
    }
    assert!(fake.all(K::StatefulSet, NS).is_empty());
    assert_eq!(status_of(&runtime).await, Phase::Absent);

    // The second delete finds nothing and says so.
    let again = runtime.delete(&id()).await.unwrap();
    assert!(!again.existed);
    assert!(again.retained_volumes.is_empty());
}

#[tokio::test]
async fn delete_removes_the_claims_after_the_compute_and_only_this_services() {
    let (fake, runtime) = setup();
    seed_runtime(&fake, "Delete");
    let outcome = runtime.delete(&id()).await.unwrap();
    assert!(outcome.existed);
    assert!(outcome.retained_volumes.is_empty());

    let deleted = kinds_written(&fake.calls(), "DELETE");
    let at = |what: (K, &str)| {
        deleted
            .iter()
            .position(|(k, n)| *k == what.0 && n == what.1)
            .unwrap_or_else(|| panic!("{what:?} not deleted: {deleted:?}"))
    };
    assert!(
        at((K::StatefulSet, "rt")) < at((K::Claim, "work-rt-0")),
        "nothing is left to make a claim again: {deleted:?}"
    );
    assert!(fake.get(K::Claim, NS, "work-rt-0").is_none());
    assert!(fake.get(K::Claim, NS, "work-rt-1").is_none());
    assert!(
        fake.get(K::Claim, NS, "work-other-0").is_some(),
        "another service's claim"
    );
}

#[tokio::test]
async fn an_unannotated_runtime_is_deleted_as_retain() {
    // Data stays unless the policy clearly says otherwise.
    let (fake, runtime) = setup();
    fake.put(
        K::StatefulSet,
        NS,
        "rt",
        workload("agent", 1, json!({}), json!(null)),
    );
    fake.put(K::Claim, NS, "work-rt-0", claim("work-rt-0", json!([])));
    runtime.delete(&id()).await.unwrap();
    assert!(fake.get(K::Claim, NS, "work-rt-0").is_some());
}

async fn status_of(runtime: &KubernetesRuntime) -> Phase {
    runtime.status(&id()).await.unwrap().phase
}

#[tokio::test]
async fn deleting_what_is_not_there_asks_and_deletes_nothing() {
    let (fake, runtime) = setup();
    let outcome = runtime.delete(&id()).await.unwrap();
    assert!(!outcome.existed);
    assert!(fake.writes().is_empty());
}

// ---------------------------------------------------------------- status, endpoint

#[tokio::test]
async fn the_status_reads_the_pods_of_a_set_that_is_not_ready() {
    let (fake, runtime) = setup();
    fake.put(
        K::Deployment,
        NS,
        "rt",
        workload(
            "agent",
            1,
            json!({}),
            json!({"observedGeneration": 1, "replicas": 1}),
        ),
    );
    let pod = |name: &str, state: Value| {
        ours(
            "rt",
            json!({
                "metadata": {"labels": {"app.kubernetes.io/component": "agent"}},
                "spec": {"containers": [{"name": "agent", "image": "i"}]},
                "status": {"containerStatuses": [{
                    "name": name, "ready": false, "restartCount": 4, "image": "i", "imageID": "",
                    "state": state,
                    "lastState": {"terminated": {"exitCode": 78, "reason": "Error"}},
                }]},
            }),
        )
    };
    fake.put(
        K::Pod,
        NS,
        "rt-1",
        pod(
            "agent",
            json!({"waiting": {"reason": "CrashLoopBackOff", "message": "back-off"}}),
        ),
    );
    let status = runtime.status(&id()).await.unwrap();
    assert_eq!(status.phase, Phase::Failed);
    assert_eq!(status.issues.len(), 1);
    assert_eq!(status.issues[0].reason, IssueReason::ConfigRejected);
    assert_eq!(status.issues[0].role, Role::All);

    // The same Deployment, rolled out: the pods' past is not read.
    fake.set_status(
        K::Deployment,
        NS,
        "rt",
        json!({"observedGeneration": 1, "replicas": 1, "readyReplicas": 1, "updatedReplicas": 1}),
    );
    let status = runtime.status(&id()).await.unwrap();
    assert_eq!((status.phase, status.replicas), (Phase::Ready, 1));
    assert!(status.issues.is_empty());
}

#[tokio::test]
async fn the_endpoint_is_the_service_of_ours() {
    let (fake, runtime) = setup();
    let err = runtime.endpoint(&id(), Surface::A2a).await.unwrap_err();
    assert_eq!(err.class(), ErrorClass::NotFound);

    fake.put(K::Service, NS, "rt", helm("rt"));
    let err = runtime.endpoint(&id(), Surface::A2a).await.unwrap_err();
    assert_eq!(
        err.class(),
        ErrorClass::NotFound,
        "a Service that is not ours is not the runtime's"
    );

    runtime.delete(&id()).await.unwrap();
    let mut service = ours("rt", json!({}));
    service["spec"] = json!({"ports": [{"name": "http", "port": 9090}]});
    fake.put(K::Service, NS, "rt", service);
    assert_eq!(
        runtime.endpoint(&id(), Surface::A2a).await.unwrap().url,
        "http://rt.adam-operator-test.svc:9090/"
    );
    assert_eq!(
        runtime
            .endpoint(&id(), Surface::AgentCard)
            .await
            .unwrap()
            .url,
        "http://rt.adam-operator-test.svc:9090/.well-known/agent-card.json"
    );
}

// ---------------------------------------------------------------- failures

#[tokio::test]
async fn an_api_server_that_fails_is_unavailable_and_changes_nothing() {
    let (fake, runtime) = setup();
    fake.fail_with(Some(503));
    let spec: RuntimeSpec = sample_spec(&id());
    for err in [
        runtime.ensure(&id(), &spec).await.unwrap_err(),
        runtime.status(&id()).await.unwrap_err(),
        runtime.delete(&id()).await.map(|_| ()).unwrap_err(),
        runtime.suspend(&id()).await.unwrap_err(),
    ] {
        assert_eq!(err.class(), ErrorClass::Transient, "{err}");
    }
    fake.fail_with(Some(403));
    let err = runtime.status(&id()).await.unwrap_err();
    assert_eq!(
        err.class(),
        ErrorClass::Transient,
        "a missing right may be granted: {err}"
    );
}

#[tokio::test]
async fn a_server_that_cannot_be_reached_is_unavailable() {
    // A client over a service that never answers: the connection fails.
    #[derive(Debug)]
    struct Refused;
    impl std::fmt::Display for Refused {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("connection refused")
        }
    }
    impl std::error::Error for Refused {}
    let service = tower::service_fn(|_req: http::Request<kube::client::Body>| async {
        Err::<http::Response<kube::client::Body>, _>(Refused)
    });
    let runtime = KubernetesRuntime::new(kube::Client::new(service, "default"));
    let err = runtime.status(&id()).await.unwrap_err();
    assert_eq!(err.class(), ErrorClass::Transient, "{err}");
}

// ---------------------------------------------------------------- audit

#[tokio::test]
async fn the_plain_text_of_the_objects_holds_no_reference_to_a_secret() {
    let (fake, runtime) = setup();
    runtime.ensure(&id(), &sample_spec(&id())).await.unwrap();
    // The fake keeps what was applied: the references are in the objects.
    let stored = fake.get(K::StatefulSet, NS, "rt").unwrap().to_string();
    assert!(
        stored.contains(adam_operator_ports::testkit::SENTINEL),
        "the reference is applied"
    );
    let text = runtime.plain_text(&id()).await.unwrap();
    assert!(!text.is_empty());
    assert!(
        !text
            .iter()
            .any(|t| t.contains(adam_operator_ports::testkit::SENTINEL)),
        "{:?}",
        text.iter()
            .filter(|t| t.contains(adam_operator_ports::testkit::SENTINEL))
            .collect::<Vec<_>>()
    );
    assert!(
        text.iter().any(|t| t == "Your name is Sample.\n"),
        "file contents are plain text"
    );
}
