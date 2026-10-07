//! The objects of the examples: golden YAML (what the cluster is asked to make), and the rules those
//! goldens could hide behind a bulk update stated one by one.
//!
//! Regenerate the goldens with `ADAM_OPERATOR_UPDATE_GOLDENS=1 cargo test -p adam-operator-runtime-kubernetes --test render`
//! and read the diff: it is the change of what the operator makes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::PathBuf;

use adam_operator_ports::{
    DeletionPolicy, OwnerHandle, RuntimeId, RuntimeSpec, Sharing, VolumeSource,
};
use adam_operator_runtime_kubernetes::names::{
    DELETION_POLICY_ANNOTATION, DIGEST_ANNOTATION, MANAGED_BY_LABEL, MANAGED_BY_VALUE,
};
use adam_operator_runtime_kubernetes::{Rendered, WorkloadObject, render};
use common::{example, resolved, set};
use serde_json::{Value, json};

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.yaml"))
}

fn yaml(r: &Rendered) -> String {
    r.documents()
        .iter()
        .map(|d| serde_yaml::to_string(d).unwrap())
        .collect::<Vec<_>>()
        .join("---\n")
}

fn assert_golden(name: &str, r: &Rendered) {
    let got = yaml(r);
    let path = golden_path(name);
    if std::env::var("ADAM_OPERATOR_UPDATE_GOLDENS").as_deref() == Ok("1") {
        std::fs::write(&path, &got).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{}: {e}; run with ADAM_OPERATOR_UPDATE_GOLDENS=1",
            path.display()
        )
    });
    assert!(
        got == want,
        "{name} differs from {}; run with ADAM_OPERATOR_UPDATE_GOLDENS=1 and read the diff.\n--- got ---\n{got}",
        path.display()
    );
}

fn coder() -> (RuntimeId, RuntimeSpec) {
    let r = resolved("coder", |_, _| {});
    (r.id, r.runtime)
}

fn split() -> (RuntimeId, RuntimeSpec) {
    let r = resolved("coder", |s, c| {
        set(s, "/spec/scaling/topology", json!("split"));
        set(s, "/spec/scaling/workers", json!(2));
        set(s, "/spec/scaling/front", json!({"replicas": 2}));
        set(
            c,
            "/spec/harness/adam/coder/workspacePlacement",
            json!("isolated"),
        );
    });
    (r.id, r.runtime)
}

fn chat() -> (RuntimeId, RuntimeSpec) {
    let r = resolved("chat", |_, _| {});
    (r.id, r.runtime)
}

fn rendered((id, spec): &(RuntimeId, RuntimeSpec)) -> Rendered {
    render(id, spec).unwrap()
}

// ---------------------------------------------------------------- goldens

#[test]
fn the_coder_combined() {
    assert_golden("coder", &rendered(&coder()));
}

#[test]
fn the_coder_split() {
    assert_golden("coder-split", &rendered(&split()));
}

#[test]
fn the_chat_folder_agent() {
    assert_golden("chat", &rendered(&chat()));
}

// ---------------------------------------------------------------- the rules, one by one

fn find<'a>(docs: &'a [Value], kind: &str, name: &str) -> &'a Value {
    docs.iter()
        .find(|d| d["kind"] == kind && d["metadata"]["name"] == name)
        .unwrap_or_else(|| panic!("no {kind} {name} in {:?}", kinds(docs)))
}

fn kinds(docs: &[Value]) -> Vec<String> {
    docs.iter()
        .map(|d| format!("{}/{}", d["kind"], d["metadata"]["name"]))
        .collect()
}

#[test]
fn the_coder_is_a_stateful_set_with_a_claim_per_replica() {
    let spec = coder();
    let docs = rendered(&spec).documents();
    let names = kinds(&docs);
    assert_eq!(
        names,
        [
            "\"ConfigMap\"/\"coder-mcp\"",
            "\"Service\"/\"coder\"",
            "\"StatefulSet\"/\"coder\"",
            "\"NetworkPolicy\"/\"coder\"",
        ]
    );
    let sts = find(&docs, "StatefulSet", "coder");
    let claim = &sts["spec"]["volumeClaimTemplates"][0];
    assert_eq!(claim["metadata"]["name"], "work");
    assert_eq!(claim["spec"]["accessModes"], json!(["ReadWriteOnce"]));
    assert_eq!(claim["spec"]["resources"]["requests"]["storage"], "20Gi");
    assert_eq!(claim["spec"]["storageClassName"], "longhorn");
    // A claim is data: no owner, and nothing in its metadata that moves (a template is immutable).
    assert!(claim["metadata"].get("ownerReferences").is_none());
    assert!(claim["metadata"].get("annotations").is_none());
    assert_eq!(
        sts["spec"]["persistentVolumeClaimRetentionPolicy"]["whenDeleted"],
        "Retain"
    );
    assert_eq!(
        sts["spec"]["persistentVolumeClaimRetentionPolicy"]["whenScaled"],
        "Retain"
    );
    assert_eq!(sts["spec"]["podManagementPolicy"], "OrderedReady");
    assert_eq!(sts["spec"]["updateStrategy"]["type"], "RollingUpdate");
    assert_eq!(sts["spec"]["replicas"], 1);
}

#[test]
fn every_object_carries_the_labels_the_digest_and_the_owner() {
    let (id, spec) = coder();
    let docs = rendered(&(id, spec.clone())).documents();
    for d in &docs {
        let what = format!("{}/{}", d["kind"], d["metadata"]["name"]);
        let labels = &d["metadata"]["labels"];
        assert_eq!(labels[MANAGED_BY_LABEL], MANAGED_BY_VALUE, "{what}");
        assert_eq!(labels["app.kubernetes.io/instance"], "coder", "{what}");
        assert!(labels["app.kubernetes.io/name"].is_string(), "{what}");
        assert_eq!(
            d["metadata"]["annotations"][DIGEST_ANNOTATION], spec.digest,
            "{what}"
        );
        assert_eq!(
            d["metadata"]["namespace"], "another-agentic-system",
            "{what}"
        );
        let owner = &d["metadata"]["ownerReferences"][0];
        assert_eq!(owner["kind"], "AgentService", "{what}");
        assert_eq!(owner["name"], "coder", "{what}");
        assert_eq!(
            owner["uid"], "0b1f3c5e-1111-4222-8333-444455556666",
            "{what}"
        );
        assert_eq!(owner["controller"], true, "{what}");
    }
    let sts = find(&docs, "StatefulSet", "coder");
    let pod = &sts["spec"]["template"];
    assert_eq!(
        pod["metadata"]["annotations"][DIGEST_ANNOTATION],
        spec.digest
    );
    assert_eq!(
        sts["metadata"]["annotations"][DELETION_POLICY_ANNOTATION],
        "Retain"
    );
    // The selector is the name and the service, and the Service selects the same.
    assert_eq!(
        sts["spec"]["selector"]["matchLabels"],
        json!({"app.kubernetes.io/instance": "coder", "app.kubernetes.io/name": "coder"})
    );
    let service = find(&docs, "Service", "coder");
    assert_eq!(
        service["spec"]["selector"],
        sts["spec"]["selector"]["matchLabels"]
    );
    assert_eq!(service["spec"]["ports"][0]["port"], 8080);
    assert_eq!(service["spec"]["ports"][0]["targetPort"], "http");
}

#[test]
fn an_owner_the_provider_does_not_understand_is_no_owner() {
    let (id, mut spec) = coder();
    for token in [
        "",
        "sample-owner",
        "{}",
        r#"{"apiVersion":"v1","kind":"X","name":"x","uid":""}"#,
    ] {
        spec.owner = OwnerHandle::new(token);
        for d in rendered(&(id.clone(), spec.clone())).documents() {
            assert!(
                d["metadata"].get("ownerReferences").is_none(),
                "{token}: {d}"
            );
        }
    }
}

#[test]
fn the_pod_template_is_the_charts() {
    let docs = rendered(&coder()).documents();
    let pod = &find(&docs, "StatefulSet", "coder")["spec"]["template"]["spec"];
    assert_eq!(pod["automountServiceAccountToken"], false);
    assert_eq!(pod["terminationGracePeriodSeconds"], 120);
    let sc = &pod["securityContext"];
    assert_eq!(sc["runAsNonRoot"], true);
    assert_eq!(sc["runAsUser"], 10001);
    assert_eq!(sc["runAsGroup"], 10001);
    assert_eq!(sc["fsGroup"], 10001);
    assert_eq!(sc["fsGroupChangePolicy"], "OnRootMismatch");
    assert_eq!(sc["seccompProfile"]["type"], "RuntimeDefault");

    let agent = &pod["containers"][0];
    assert_eq!(agent["name"], "agent");
    assert_eq!(agent["securityContext"]["allowPrivilegeEscalation"], false);
    assert_eq!(
        agent["securityContext"]["capabilities"]["drop"],
        json!(["ALL"])
    );
    assert_eq!(
        agent["ports"][0],
        json!({"name": "http", "containerPort": 8080, "protocol": "TCP"})
    );
    for (probe, period) in [
        ("startupProbe", 3),
        ("livenessProbe", 15),
        ("readinessProbe", 5),
    ] {
        assert_eq!(
            agent[probe]["httpGet"],
            json!({"path": "/healthz", "port": "http"}),
            "{probe}"
        );
        assert_eq!(agent[probe]["periodSeconds"], period, "{probe}");
    }

    // The native sidecar: an init container that restarts, probed by exec on loopback.
    let sidecar = &pod["initContainers"][0];
    assert_eq!(sidecar["name"], "github-mcp");
    assert_eq!(sidecar["restartPolicy"], "Always");
    assert_eq!(
        sidecar["securityContext"]["capabilities"]["drop"],
        json!(["ALL"])
    );
    assert_eq!(sidecar["startupProbe"]["exec"]["command"][0], "bash");
    assert!(sidecar.get("ports").is_none());
    assert_eq!(pod["containers"].as_array().unwrap().len(), 1);
}

#[test]
fn a_secret_is_a_reference_and_a_pod_name_is_the_downward_api() {
    let docs = rendered(&coder()).documents();
    let pod = &find(&docs, "StatefulSet", "coder")["spec"]["template"]["spec"];
    let env = pod["containers"][0]["env"].as_array().unwrap();
    let var = |n: &str| {
        env.iter()
            .find(|e| e["name"] == n)
            .unwrap_or_else(|| panic!("{n}"))
    };
    assert_eq!(
        var("MODEL_API_KEY")["valueFrom"]["secretKeyRef"],
        json!({"name": "coder-secrets", "key": "MODEL_API_KEY"})
    );
    assert!(var("MODEL_API_KEY").get("value").is_none());
    assert_eq!(var("LISTEN_ADDR")["value"], "0.0.0.0:8080");
    assert!(var("LISTEN_ADDR").get("valueFrom").is_none());
    // The GitHub App key is a file of a Secret volume, never a variable.
    let key = pod["volumes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == "github-app")
        .unwrap();
    assert_eq!(key["secret"]["secretName"], "coder-github-app");
    assert_eq!(key["secret"]["defaultMode"], 0o440);
    assert_eq!(
        key["secret"]["items"],
        json!([{"key": "private-key.pem", "path": "private-key.pem"}])
    );
}

#[test]
fn the_network_policy_admits_the_namespace_to_the_port_and_has_no_egress() {
    let docs = rendered(&coder()).documents();
    let np = find(&docs, "NetworkPolicy", "coder");
    assert_eq!(np["spec"]["policyTypes"], json!(["Ingress"]));
    assert!(np["spec"].get("egress").is_none());
    assert_eq!(
        np["spec"]["ingress"][0]["from"],
        json!([{"namespaceSelector": {"matchLabels": {"kubernetes.io/metadata.name": "another-agentic-system"}}}])
    );
    assert_eq!(
        np["spec"]["ingress"][0]["ports"],
        json!([{"protocol": "TCP", "port": 8080}])
    );
    assert_eq!(
        np["spec"]["podSelector"]["matchLabels"],
        json!({"app.kubernetes.io/instance": "coder", "app.kubernetes.io/managed-by": "adam-operator"})
    );
}

#[test]
fn no_allow_from_is_no_policy() {
    let (id, mut spec) = coder();
    spec.network.allow_from.clear();
    let r = render(&id, &spec).unwrap();
    assert!(r.network_policy.is_none());
    assert!(
        !kinds(&r.documents())
            .iter()
            .any(|k| k.contains("NetworkPolicy"))
    );
}

#[test]
fn split_has_a_front_a_budget_and_a_service_that_selects_the_front() {
    let spec = split();
    let docs = rendered(&spec).documents();
    let front = find(&docs, "Deployment", "coder-front");
    assert_eq!(front["spec"]["replicas"], 2);
    assert_eq!(
        front["metadata"]["labels"]["app.kubernetes.io/component"],
        "front"
    );
    let workers = find(&docs, "StatefulSet", "coder");
    assert_eq!(workers["spec"]["replicas"], 2);
    assert_eq!(
        workers["metadata"]["labels"]["app.kubernetes.io/component"],
        "worker"
    );
    let pdb = find(&docs, "PodDisruptionBudget", "coder-front");
    assert_eq!(pdb["spec"]["minAvailable"], 1);
    assert_eq!(
        pdb["spec"]["selector"]["matchLabels"],
        json!({"app.kubernetes.io/instance": "coder", "app.kubernetes.io/name": "coder-front"})
    );
    assert_eq!(
        find(&docs, "Service", "coder")["spec"]["selector"]["app.kubernetes.io/name"],
        "coder-front"
    );
    // One policy covers both workloads' pods.
    let np = find(&docs, "NetworkPolicy", "coder");
    assert!(np["spec"]["podSelector"]["matchLabels"]["app.kubernetes.io/name"].is_null());
}

#[test]
fn a_front_of_one_has_no_budget() {
    let r = rendered(&{
        let r = resolved("coder", |s, _| {
            set(s, "/spec/scaling/topology", json!("split"));
            set(s, "/spec/scaling/front", json!({"replicas": 1}));
        });
        (r.id, r.runtime)
    });
    assert!(r.disruption_budgets.is_empty());
}

#[test]
fn a_folder_agent_is_a_deployment_with_an_immutable_file_set() {
    let spec = chat();
    let r = rendered(&spec);
    let docs = r.documents();
    assert_eq!(r.workloads.len(), 1);
    assert!(matches!(r.workloads[0].1, WorkloadObject::Deployment(_)));
    assert!(r.network_policy.is_none(), "chat has no allowFrom");
    assert!(r.disruption_budgets.is_empty());
    assert!(r.claims.is_empty());

    let cm = &r.config_maps[0];
    let name = cm.metadata.name.as_deref().unwrap();
    assert!(
        name.starts_with("chat-agent-") && name.len() == "chat-agent-".len() + 8,
        "{name}"
    );
    assert_eq!(cm.immutable, Some(true));
    assert!(cm.data.as_ref().unwrap().contains_key("instructions.md"));

    let pod = &find(&docs, "Deployment", "chat")["spec"]["template"]["spec"];
    let volume = pod["volumes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == "agent-folder")
        .unwrap();
    assert_eq!(volume["configMap"]["name"], name);
    assert_eq!(volume["configMap"]["defaultMode"], 0o444);
    assert_eq!(
        volume["configMap"]["items"],
        json!([{"key": "instructions.md", "path": "instructions.md"}])
    );
    let agent = &pod["containers"][0];
    assert_eq!(agent["command"], json!(["tini", "--", "adam-agent"]));
    assert!(pod.get("initContainers").is_none(), "no sidecar");
}

#[test]
fn a_file_path_with_a_slash_is_a_key_and_is_mounted_at_its_path() {
    let (id, mut spec) = chat_with_a_nested_file();
    spec.deletion = DeletionPolicy::Delete;
    let r = render(&id, &spec).unwrap();
    let cm = &r.config_maps[0];
    assert!(
        cm.data
            .as_ref()
            .unwrap()
            .contains_key("skills_sreview_sSKILL.md")
    );
    let docs = r.documents();
    let pod = &find(&docs, "Deployment", "chat")["spec"]["template"]["spec"];
    let items = &pod["volumes"][0]["configMap"]["items"];
    assert!(
        items.as_array().unwrap().contains(
            &json!({"key": "skills_sreview_sSKILL.md", "path": "skills/review/SKILL.md"})
        ),
        "{items}"
    );
}

fn chat_with_a_nested_file() -> (RuntimeId, RuntimeSpec) {
    let r = resolved("chat", |_, c| {
        set(
            c,
            "/spec/harness/adam/agent/folder/files/skills~1review~1SKILL.md",
            json!("# Review\n"),
        );
    });
    (r.id, r.runtime)
}

#[test]
fn a_shared_volume_is_one_claim_with_no_owner_that_every_pod_mounts() {
    let r = resolved("coder", |s, c| {
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
    });
    let (id, spec) = (r.id, r.runtime);
    let rendered = render(&id, &spec).unwrap();
    assert_eq!(rendered.claims.len(), 1);
    let claim = &rendered.claims[0];
    assert_eq!(claim.metadata.name.as_deref(), Some("coder-work"));
    assert!(
        claim.metadata.owner_references.is_none(),
        "data has no owner"
    );
    let annotations = claim.metadata.annotations.as_ref().unwrap();
    assert_eq!(annotations[DELETION_POLICY_ANNOTATION], "Retain");
    assert_eq!(
        claim.spec.as_ref().unwrap().access_modes.as_deref(),
        Some(&["ReadWriteMany".to_owned()][..])
    );
    let docs = rendered.documents();
    let sts = find(&docs, "StatefulSet", "coder");
    assert!(sts["spec"].get("volumeClaimTemplates").is_none());
    let volumes = sts["spec"]["template"]["spec"]["volumes"]
        .as_array()
        .unwrap();
    let work = volumes.iter().find(|v| v["name"] == "work").unwrap();
    assert_eq!(work["persistentVolumeClaim"]["claimName"], "coder-work");
    // The name of each pod is its worker's identity, so the set keeps its identity.
    let env = sts["spec"]["template"]["spec"]["containers"][0]["env"]
        .as_array()
        .unwrap();
    assert!(
        env.iter().any(|e| e["name"] == "WORKER_ID"
            && e["valueFrom"]["fieldRef"]["fieldPath"] == "metadata.name")
    );
}

#[test]
fn suspend_is_zero_replicas_and_changes_nothing_else() {
    let (id, mut spec) = coder();
    let before = render(&id, &spec).unwrap().documents();
    spec.suspend = true;
    let after = render(&id, &spec).unwrap().documents();
    let sts = find(&after, "StatefulSet", "coder");
    assert_eq!(sts["spec"]["replicas"], 0);
    let mut expected = before;
    for d in &mut expected {
        if d["kind"] == "StatefulSet" {
            d["spec"]["replicas"] = json!(0);
        }
    }
    assert_eq!(after, expected);
}

#[test]
fn what_is_applied_does_not_move_the_pods() {
    // Scale and network are applied, not rolled: the pod template is the same.
    let (id, spec) = coder();
    let template = |spec: &RuntimeSpec| {
        let docs = render(&id, spec).unwrap().documents();
        find(&docs, "StatefulSet", "coder")["spec"]["template"].clone()
    };
    let mut bigger = spec.clone();
    bigger.workloads[0].replicas = 5;
    bigger.network.allow_from.clear();
    bigger.owner = OwnerHandle::none();
    assert_eq!(template(&spec), template(&bigger));
}

#[test]
fn no_secret_name_is_in_plain_text() {
    // Every Secret of the example gets a name with a sentinel; it may be a reference and nothing else.
    const SENTINEL: &str = "adam-operator-sentinel-never-a-value";
    let (mut service, mut config) = example("coder");
    let swap = |v: &mut Value| {
        let text = v
            .to_string()
            .replace("coder-secrets", &format!("{SENTINEL}-secrets"));
        let text = text.replace("coder-github-app", &format!("{SENTINEL}-key"));
        *v = serde_json::from_str(&text).unwrap();
    };
    swap(&mut service);
    swap(&mut config);
    let r = {
        let s = serde_json::from_value(service).unwrap();
        let c = serde_json::from_value(config).unwrap();
        adam_operator_domain::resolve(&s, &c, OwnerHandle::none()).unwrap()
    };
    let docs = render(&r.id, &r.runtime).unwrap().documents();
    let mut text = Vec::new();
    for mut d in docs {
        strip(&mut d);
        collect(&d, &mut text);
    }
    assert!(!text.is_empty());
    assert!(
        !text.iter().any(|t| t.contains(SENTINEL)),
        "a Secret's name reached plain text: {:?}",
        text.iter()
            .filter(|t| t.contains(SENTINEL))
            .collect::<Vec<_>>()
    );
    // And the references are there, so the strip removed something.
    let all = render(&r.id, &r.runtime).unwrap().documents();
    assert!(all.iter().any(|d| d.to_string().contains(SENTINEL)));
}

fn strip(v: &mut Value) {
    match v {
        Value::Object(m) => {
            m.remove("secretKeyRef");
            if m.contains_key("name") {
                m.remove("secret");
            }
            m.values_mut().for_each(strip);
        }
        Value::Array(a) => a.iter_mut().for_each(strip),
        _ => {}
    }
}

fn collect(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Array(a) => a.iter().for_each(|i| collect(i, out)),
        Value::Object(m) => {
            for (k, c) in m {
                out.push(k.clone());
                collect(c, out);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------- what is refused

fn refused(change: impl FnOnce(&mut RuntimeSpec), says: &str) {
    let (id, mut spec) = coder();
    change(&mut spec);
    let err = render(&id, &spec).expect_err(says).to_string();
    assert!(err.contains(says), "{err:?} should say {says:?}");
}

#[test]
fn a_per_replica_volume_needs_a_stable_identity() {
    refused(
        |s| s.workloads[0].stable_identity = false,
        "stable identity",
    );
}

#[test]
fn an_http_probe_needs_a_port() {
    refused(|s| s.workloads[0].container.port = None, "serve a port");
}

#[test]
fn a_mode_that_is_not_a_permission_is_refused() {
    refused(
        |s| {
            for v in &mut s.workloads[0].volumes {
                if let VolumeSource::SecretFile { mode, .. } = &mut v.source {
                    *mode = 0o1777;
                }
            }
        },
        "not a permission",
    );
}

#[test]
fn a_claim_defined_twice_differently_is_refused() {
    let r = resolved("coder", |s, c| {
        set(s, "/spec/scaling/topology", json!("split"));
        set(
            c,
            "/spec/harness/adam/coder/workspacePlacement",
            json!("shared"),
        );
        set(
            c,
            "/spec/environment/volumes/0/source/persistent/perReplica",
            json!(false),
        );
    });
    let (id, mut spec) = (r.id, r.runtime);
    // The front gets the worker's volume too, with another size.
    let mut front = spec.workloads[0].clone();
    front.name = "coder-front".to_owned();
    for v in &mut front.volumes {
        if let VolumeSource::Persistent(p) = &mut v.source {
            p.size = "1Gi".to_owned();
            assert_eq!(p.sharing, Sharing::Shared);
        }
    }
    spec.workloads.push(front);
    let err = render(&id, &spec).unwrap_err().to_string();
    assert!(err.contains("defined twice"), "{err}");
}

#[test]
fn an_id_without_a_namespace_is_refused() {
    let (_, spec) = coder();
    assert!(render(&RuntimeId::new("", "coder"), &spec).is_err());
}
