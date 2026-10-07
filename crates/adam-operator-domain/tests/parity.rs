//! Parity goldens: `resolve` of the examples agrees with what the adam-rs chart `deploy/coder`
//! renders for the same inputs.
//!
//! The goldens in `tests/golden/*.json` are projections of `helm template` (see
//! `tools/adam-operator-parity/regen.sh` and `tests/golden/README.md`). Here `RuntimeSpec` is projected the
//! way a Kubernetes provider would make the pods, onto the same shape, and the two are compared
//! section by section: the environment of the agent container (names and where each value comes
//! from), its command, mounts, probes and resources, the native sidecars, the volumes and claims, the
//! extra MCP file, the pod security context, what the Service selects, the budget of the front.
//!
//! What differs on purpose is normalised below, and `the_normalisations_are_the_documented_ones`
//! fails if a normalisation starts hiding something the README does not list.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;

use adam_operator_domain::contract;
use adam_operator_ports::{
    Container, EnvValue, FsGroupChangePolicy, ProbeAction, RuntimeSpec, Sharing, VolumeSource,
};
use common::{must_resolve, remove, repo_root, set};
use serde_json::{Value, json};

fn golden(name: &str) -> Value {
    let path: PathBuf = repo_root()
        .join("crates/adam-operator-domain/tests/golden")
        .join(format!("{name}.json"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap()
}

// ------------------------------------------------- RuntimeSpec -> projection

fn env_source(v: &EnvValue) -> Value {
    match v {
        EnvValue::Literal(s) => json!({ "literal": s }),
        EnvValue::Secret(r) => json!({ "secret": { "name": r.name, "key": r.key } }),
        EnvValue::PodName => json!({ "field": "metadata.name" }),
    }
}

fn probe(p: &Option<adam_operator_ports::Probe>) -> Value {
    let Some(p) = p else { return Value::Null };
    let mut v = json!({
        "period": p.period_secs,
        "timeout": p.timeout_secs,
        "failureThreshold": p.failure_threshold,
    });
    match &p.action {
        ProbeAction::Http { path } => v["http"] = json!(path),
        ProbeAction::Exec { command } => v["exec"] = json!(command),
    }
    v
}

fn container(c: &Container) -> Value {
    json!({
        "image": c.image,
        "command": c.command,
        "args": c.args,
        "env": c.env.iter().map(|e| {
            let mut v = env_source(&e.value);
            v["name"] = json!(e.name);
            v
        }).collect::<Vec<_>>(),
        "mounts": c.mounts.iter().map(|m| json!({"name": m.volume, "path": m.path, "readOnly": m.read_only})).collect::<Vec<_>>(),
        "probes": {
            "startup": probe(&c.probes.startup),
            "liveness": probe(&c.probes.liveness),
            "readiness": probe(&c.probes.readiness),
        },
        "resources": { "requests": c.resources.requests, "limits": c.resources.limits },
        "port": c.port,
    })
}

/// What a Kubernetes provider makes of a `RuntimeSpec`, in the shape of the goldens. The rules are
/// the ones the design states: a StatefulSet when pods need a stable identity, a `volumeClaimTemplate` for
/// a per-replica volume, one ReadWriteMany claim `<svc>-<volume>` for a shared one.
fn project(spec: &RuntimeSpec, svc: &str) -> Value {
    let mut workloads = serde_json::Map::new();
    let mut claims = serde_json::Map::new();
    let mut pdbs = serde_json::Map::new();
    for w in &spec.workloads {
        let mut volumes = Vec::new();
        let mut templates = Vec::new();
        for v in &w.volumes {
            match &v.source {
                VolumeSource::Persistent(p) => match p.sharing {
                    Sharing::PerReplica => templates.push(json!({
                        "name": v.name,
                        "accessModes": ["ReadWriteOnce"],
                        "storage": p.size,
                        "storageClass": p.storage_class,
                    })),
                    Sharing::Shared => {
                        let claim = format!("{svc}-{}", v.name);
                        claims.insert(claim.clone(), json!({
                            "accessModes": ["ReadWriteMany"],
                            "storage": p.size,
                            "storageClass": p.storage_class,
                        }));
                        volumes.push(json!({"name": v.name, "kind": "persistentVolumeClaim", "claim": claim}));
                    }
                },
                VolumeSource::Files { file_set, mode } => volumes.push(
                    json!({"name": v.name, "kind": "configMap", "configMap": file_set, "mode": mode}),
                ),
                VolumeSource::ExternalFiles { name, mode } => {
                    volumes.push(json!({"name": v.name, "kind": "configMap", "configMap": name, "mode": mode}));
                }
                VolumeSource::SecretFile { secret, file, mode } => volumes.push(json!({
                    "name": v.name, "kind": "secret", "secret": secret.name, "mode": mode,
                    "items": [{"key": secret.key, "path": file}],
                })),
            }
        }
        let stateful = w.stable_identity || !templates.is_empty();
        let sec = &w.security;
        workloads.insert(
            w.name.clone(),
            json!({
                "kind": if stateful { "StatefulSet" } else { "Deployment" },
                "replicas": w.replicas,
                "terminationGracePeriodSeconds": w.termination_grace_secs,
                "securityContext": {
                    "runAsUser": sec.run_as_user,
                    "runAsGroup": sec.run_as_group,
                    "fsGroup": sec.fs_group,
                    "fsGroupChangePolicy": sec.fs_group_change_policy.map(|p| match p {
                        FsGroupChangePolicy::OnRootMismatch => "OnRootMismatch",
                        FsGroupChangePolicy::Always => "Always",
                    }),
                },
                "agent": container(&w.container),
                "sidecars": w.sidecars.iter().map(|c| (c.name.clone(), container(c))).collect::<BTreeMap<_, _>>(),
                "volumes": volumes,
                "claimTemplates": templates,
            }),
        );
        if let Some(n) = w.min_available {
            pdbs.insert(w.name.clone(), json!({ "minAvailable": n }));
        }
    }
    let mut config_maps = serde_json::Map::new();
    for f in spec.file_sets.iter().filter(|f| f.name.ends_with("-mcp")) {
        let files: serde_json::Map<String, Value> = f
            .files
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::from_str(v).unwrap()))
            .collect();
        config_maps.insert(f.name.clone(), Value::Object(files));
    }
    json!({
        "workloads": workloads,
        "claims": claims,
        "configMaps": config_maps,
        "podDisruptionBudgets": pdbs,
        "service": { "name": svc, "port": spec.network.port, "selects": spec.network.selects },
    })
}

// --------------------------------------------------------- normalisations

/// What the chart does differently on purpose, applied to the golden. Each use is recorded, and the
/// test below holds the record to the README.
#[derive(Default)]
struct Applied(std::collections::BTreeSet<&'static str>);

/// The chart's `PUBLIC_URL` default names the Service as `<name>.<ns>.svc.cluster.local`; the operator
/// uses `<name>.<ns>.svc`. Both resolve in a cluster.
const PUBLIC_URL_DIFFERENCE: &str = "PUBLIC_URL host: svc.cluster.local";
/// The chart pins an image by tag; the operator's examples pin tag and digest.
const IMAGE_DIFFERENCE: &str = "image: the digest the chart cannot express";

fn normalise_chart(golden: &mut Value, applied: &mut Applied) {
    let Some(workloads) = golden["workloads"].as_object_mut() else {
        return;
    };
    for w in workloads.values_mut() {
        let Some(env) = w["agent"]["env"].as_array_mut() else {
            continue;
        };
        for e in env {
            if e["name"] == "PUBLIC_URL"
                && let Some(url) = e["literal"].as_str()
                && url.contains(".svc.cluster.local")
            {
                e["literal"] = json!(url.replace(".svc.cluster.local", ".svc"));
                applied.0.insert(PUBLIC_URL_DIFFERENCE);
            }
        }
    }
}

/// The operator's image carries `@sha256:…` after the tag; the chart's has none.
fn strip_digest(projection: &mut Value) {
    let Some(workloads) = projection["workloads"].as_object_mut() else {
        return;
    };
    for w in workloads.values_mut() {
        for path in ["/agent/image"] {
            if let Some(Value::String(s)) = w.pointer_mut(path)
                && let Some((tag, _)) = s.split_once('@')
            {
                *s = tag.to_owned();
            }
        }
        if let Some(sidecars) = w["sidecars"].as_object_mut() {
            for c in sidecars.values_mut() {
                if let Some(Value::String(s)) = c.get_mut("image")
                    && let Some((tag, _)) = s.split_once('@')
                {
                    *s = tag.to_owned();
                }
            }
        }
    }
}

/// Env as a map, so order does not count and a repeated name is a failure.
fn env_map(env: &Value) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    for e in env.as_array().unwrap() {
        let mut e = e.clone();
        let name = e.as_object_mut().unwrap().remove("name").unwrap();
        let name = name.as_str().unwrap().to_owned();
        assert!(out.insert(name.clone(), e).is_none(), "{name} is set twice");
    }
    out
}

/// Sort a list of objects by `name`.
fn by_name(list: &Value) -> Value {
    let mut items = list.as_array().unwrap().clone();
    items.sort_by_key(|i| i["name"].as_str().unwrap_or_default().to_owned());
    Value::Array(items)
}

fn compare_container(case: &str, who: &str, chart: &Value, mine: &Value) {
    let (c, m) = (env_map(&chart["env"]), env_map(&mine["env"]));
    let names: std::collections::BTreeSet<&String> = c.keys().chain(m.keys()).collect();
    let differences: Vec<String> = names
        .into_iter()
        .filter(|n| c.get(*n) != m.get(*n))
        .map(|n| {
            format!(
                "  {n}: the chart has {:?}, the operator has {:?}",
                c.get(n),
                m.get(n)
            )
        })
        .collect();
    assert!(
        differences.is_empty(),
        "{case}: {who}: environment (name -> source) differs:\n{}",
        differences.join("\n")
    );
    for key in ["image", "command", "args", "probes", "resources", "port"] {
        assert_eq!(chart[key], mine[key], "{case}: {who}: {key}");
    }
    assert_eq!(
        by_name(&chart["mounts"]),
        by_name(&mine["mounts"]),
        "{case}: {who}: mounts"
    );
}

fn compare(case: &str, chart: &Value, mine: &Value) {
    assert_eq!(chart["service"], mine["service"], "{case}: service");
    assert_eq!(
        chart["claims"], mine["claims"],
        "{case}: claims the chart makes outside the pods"
    );
    assert_eq!(
        chart["configMaps"], mine["configMaps"],
        "{case}: the extra MCP file"
    );
    assert_eq!(
        chart["podDisruptionBudgets"], mine["podDisruptionBudgets"],
        "{case}: budgets"
    );
    let cw = chart["workloads"].as_object().unwrap();
    let mw = mine["workloads"].as_object().unwrap();
    assert_eq!(
        cw.keys().collect::<Vec<_>>(),
        mw.keys().collect::<Vec<_>>(),
        "{case}: the workloads"
    );
    for (name, c) in cw {
        let m = &mw[name];
        let at = format!("{case}/{name}");
        for key in [
            "kind",
            "replicas",
            "terminationGracePeriodSeconds",
            "securityContext",
            "claimTemplates",
        ] {
            assert_eq!(c[key], m[key], "{at}: {key}");
        }
        assert_eq!(
            by_name(&c["volumes"]),
            by_name(&m["volumes"]),
            "{at}: volumes"
        );
        compare_container(&at, "agent container", &c["agent"], &m["agent"]);
        assert_eq!(
            c["sidecars"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            m["sidecars"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            "{at}: sidecars"
        );
        for (sname, sc) in c["sidecars"].as_object().unwrap() {
            compare_container(&at, sname, sc, &m["sidecars"][sname]);
        }
    }
}

fn parity(case: &str, mine: &Value, applied: &mut Applied) {
    let mut chart = golden(case);
    normalise_chart(&mut chart, applied);
    let mut mine = mine.clone();
    strip_digest(&mut mine);
    compare(case, &chart, &mine);
}

// ------------------------------------------------------------------ cases

/// `examples/coder.yaml` changed to say what `cases/coder-split.yaml` says.
fn split(_service: &mut Value, _config: &mut Value) {
    let (service, config) = (_service, _config);
    set(service, "/spec/scaling/topology", json!("split"));
    set(service, "/spec/scaling/workers", json!(2));
    set(service, "/spec/scaling/front", json!({"replicas": 2}));
    set(
        config,
        "/spec/harness/adam/coder/workspacePlacement",
        json!("isolated"),
    );
}

/// …and `cases/coder-affinity-token.yaml`.
fn affinity_token(service: &mut Value, config: &mut Value) {
    set(service, "/spec/scaling/workers", json!(2));
    set(
        service,
        "/spec/interfaces/a2a/publicUrl",
        json!("https://coder.example.com/"),
    );
    let coder = "/spec/harness/adam/coder";
    set(
        config,
        &format!("{coder}/workspacePlacement"),
        json!("affinity"),
    );
    remove(config, &format!("{coder}/github/app"));
    set(
        config,
        &format!("{coder}/github/token"),
        json!({"secretRef": {"name": "coder-secrets", "key": "GITHUB_TOKEN"}}),
    );
    set(
        config,
        &format!("{coder}/createRepoOwners"),
        json!(["vymalo", "acme"]),
    );
    set(
        config,
        &format!("{coder}/allowedRepoHosts"),
        json!(["github.com", "ghe.example.com"]),
    );
    set(
        config,
        &format!("{coder}/githubApiUrl"),
        json!("https://ghe.example.com/api/v3"),
    );
    set(config, &format!("{coder}/prDraft"), json!(false));
    set(
        config,
        "/spec/environment/volumes/0/source/persistent/perReplica",
        json!(false),
    );
    set(
        config,
        "/spec/environment/volumes/0/source/persistent/storageClass",
        json!("longhorn-rwx"),
    );
    set(
        config,
        "/spec/tools/githubMcp",
        json!({"sidecar": true, "port": 9090, "host": "ghe.example.com"}),
    );
    set(config, "/spec/tools/mcpServers", json!({}));
    set(config, "/spec/tools/allowInsecureHttp", json!(false));
    set(
        config,
        "/spec/model/baseUrl",
        json!({"value": "https://gateway.example.com/v1"}),
    );
    set(
        config,
        "/spec/model/extraBody",
        json!({"reasoning_effort": "medium", "chat_template_kwargs": {"enable_thinking": true}}),
    );
    set(
        config,
        "/spec/model/echoReasoning",
        json!("reasoning_content"),
    );
    set(
        config,
        "/spec/extraEnv",
        json!({"FOO": "bar", "RUST_LOG": "debug"}),
    );
}

#[test]
fn the_netcup_coder_renders_what_the_chart_renders() {
    let r = must_resolve("coder", |_, _| {});
    let mut applied = Applied::default();
    parity("coder", &project(&r.runtime, "coder"), &mut applied);
}

#[test]
fn a_split_coder_with_isolated_workers_renders_what_the_chart_renders() {
    let r = must_resolve("coder", split);
    let mut applied = Applied::default();
    parity("coder-split", &project(&r.runtime, "coder"), &mut applied);
}

#[test]
fn an_affinity_coder_on_a_token_renders_what_the_chart_renders() {
    let r = must_resolve("coder", affinity_token);
    let mut applied = Applied::default();
    parity(
        "coder-affinity-token",
        &project(&r.runtime, "coder"),
        &mut applied,
    );
}

#[test]
fn the_normalisations_are_the_documented_ones() {
    let mut applied = Applied::default();
    for (case, change) in [
        (
            "coder",
            (|_: &mut Value, _: &mut Value| {}) as fn(&mut Value, &mut Value),
        ),
        ("coder-split", split),
        ("coder-affinity-token", affinity_token),
    ] {
        let r = must_resolve("coder", change);
        let mut chart = golden(case);
        normalise_chart(&mut chart, &mut applied);
        // The image: the operator's carries a digest the chart cannot express.
        let mine = project(&r.runtime, "coder");
        let digest = mine["workloads"]["coder"]["agent"]["image"]
            .as_str()
            .unwrap()
            .contains("@sha256:");
        let tag_only = !chart["workloads"]["coder"]["agent"]["image"]
            .as_str()
            .unwrap()
            .contains('@');
        if digest && tag_only {
            applied.0.insert(IMAGE_DIFFERENCE);
        }
    }
    assert_eq!(
        applied.0.into_iter().collect::<Vec<_>>(),
        vec![PUBLIC_URL_DIFFERENCE, IMAGE_DIFFERENCE],
        "tests/golden/README.md lists the intended differences: update it with this list"
    );
}

#[test]
fn the_goldens_say_where_they_came_from() {
    for case in ["coder", "coder-split", "coder-affinity-token"] {
        let g = golden(case);
        let s = &g["source"];
        assert_eq!(s["chart"], "deploy/coder");
        assert_eq!(s["generatedBy"], "tools/adam-operator-parity/regen.sh");
        assert!(
            repo_root().join(s["values"].as_str().unwrap()).is_file(),
            "{case}: its values file"
        );
    }
}

// ----------------------------------------------------------------- chat

/// `adam-agent` has no chart render: its golden is written from the binary's README (see its
/// `source`), so this test holds the operator to a document, not to a render.
#[test]
fn a_folder_agent_matches_the_hand_written_golden() {
    let g = golden("chat-env");
    assert!(
        g["source"]["kind"]
            .as_str()
            .unwrap()
            .contains("not rendered")
    );
    let r = must_resolve("chat", |_, _| {});
    let mine = project(&r.runtime, "chat");
    let agent = &mine["workloads"]["chat"]["agent"];
    assert_eq!(
        mine["workloads"]["chat"]["kind"], "Deployment",
        "no volume, no pinned identity"
    );
    assert_eq!(agent["command"], g["agent"]["command"]);
    assert_eq!(env_map(&agent["env"]), env_map(&g["agent"]["env"]));
    assert_eq!(by_name(&agent["mounts"]), by_name(&g["agent"]["mounts"]));
    let names: Vec<String> = env_map(&agent["env"]).into_keys().collect();
    for absent in g["absent"].as_array().unwrap() {
        assert!(
            !names.contains(&absent.as_str().unwrap().to_owned()),
            "{absent} must not be set for adam-agent"
        );
    }
    // No sidecar: nothing in the example asks for one, and a folder agent has no GitHub.
    assert!(
        mine["workloads"]["chat"]["sidecars"]
            .as_object()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn every_variable_of_the_contract_is_covered_by_a_golden_or_a_test() {
    // The names the operator can set (contract::env::OPERATOR_SET) that no coder golden sets are the
    // ones this crate's other tests cover (GITHUB_APP_INSTALLATION_ID, MCP_ALLOW_INSECURE off, …).
    // This lists them, so a new variable added to the contract cannot be forgotten silently.
    let mut seen = std::collections::BTreeSet::new();
    for case in ["coder", "coder-split", "coder-affinity-token"] {
        for w in golden(case)["workloads"].as_object().unwrap().values() {
            for e in w["agent"]["env"].as_array().unwrap() {
                seen.insert(e["name"].as_str().unwrap().to_owned());
            }
        }
    }
    let unseen: Vec<&&str> = contract::env::OPERATOR_SET
        .iter()
        .filter(|n| !seen.contains(**n))
        .collect();
    assert_eq!(
        unseen,
        vec![&"ADAM_AGENT_DIR", &"GITHUB_APP_INSTALLATION_ID"],
        "the contract names a variable no golden sets: cover it in tests/examples.rs"
    );
}
