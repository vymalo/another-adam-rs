//! The whole controller, running: `Operator::run` against the fake API server's list and watch, the
//! `Memory` store and a scripted runtime. What these show is the wiring that no single pass can: that
//! an object that appears is reconciled, that a changed `AgentConfig` reconciles the services that name
//! it, that an id from `RuntimeProvider::watch()` reconciles its service, that another namespace is
//! left alone, and that the controllers stop when told to. A real API server is the kind job's.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::sync::Arc;
use std::time::Duration;

use adam_operator_controller::{Operator, Options, OwnerOf, Resync};
use adam_operator_ports::memory::MemoryStore;
use adam_operator_ports::{
    AgentDirectory, Issue, IssueReason, OwnerHandle, Phase, Role, RuntimeId, RuntimeStatus,
};
use serde_json::{Value, json};
use support::Fake;
use support::scripted::Scripted;
use tokio::sync::oneshot;

const SERVICES: &str = "agentservices";
const CONFIGS: &str = "agentconfigs";

fn example(name: &str, ns: &str) -> (Value, Value) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../deploy/operator/examples")
        .join(format!("{name}.yaml"));
    let text = std::fs::read_to_string(path).unwrap();
    let mut docs: Vec<Value> = serde_yaml::Deserializer::from_str(&text)
        .map(|d| serde::Deserialize::deserialize(d).unwrap())
        .collect();
    for d in &mut docs {
        d["metadata"]["namespace"] = json!(ns);
    }
    let service = docs
        .iter()
        .find(|d| d["kind"] == "AgentService")
        .cloned()
        .unwrap();
    let config = docs
        .iter()
        .find(|d| d["kind"] == "AgentConfig")
        .cloned()
        .unwrap();
    (service, config)
}

/// Poll `check` until it holds, or fail after ten seconds with `what`.
async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..1000 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {what}");
}

struct Running {
    fake: Fake,
    runtime: Scripted,
    operator_done: tokio::task::JoinHandle<()>,
    stop: oneshot::Sender<()>,
    directory: adam_operator_controller::ReflectorDirectory,
    readiness: adam_operator_controller::Readiness,
    metrics: Arc<adam_operator_controller::Metrics>,
}

async fn start(namespace: Option<&str>) -> Running {
    let fake = Fake::new();
    let runtime = Scripted::new();
    let owner: OwnerOf = Arc::new(|_| OwnerHandle::none());
    let options = Options {
        watch_namespace: namespace.map(str::to_owned),
        // Far-away timers: what is asserted here arrives by a trigger or not at all, and a timer
        // that fired in the time we wait would hide a missing trigger.
        resync: Resync {
            settled: Duration::from_secs(3600),
            pending: Duration::from_secs(3600),
            not_installed: Duration::from_secs(3600),
        },
        ..Options::default()
    };
    let operator = Operator::new(
        fake.client(),
        runtime.clone(),
        MemoryStore::new(),
        owner,
        options,
    );
    let directory = operator.directory();
    let readiness = operator.readiness();
    let metrics = operator.metrics();
    let (stop, stopped) = oneshot::channel::<()>();
    let operator_done = tokio::spawn(operator.run(async move {
        let _ = stopped.await;
    }));
    // The watches are open: nothing below races the list that precedes them.
    eventually("the controllers' watches", || {
        fake.watchers(SERVICES) >= 1 && fake.watchers(CONFIGS) >= 2
    })
    .await;
    Running {
        fake,
        runtime,
        operator_done,
        stop,
        directory,
        readiness,
        metrics,
    }
}

impl Running {
    fn state(&self, ns: &str, name: &str) -> Option<String> {
        self.fake
            .get(SERVICES, ns, name)?
            .pointer("/status/state")?
            .as_str()
            .map(str::to_owned)
    }

    async fn stop(self) {
        let _ = self.stop.send(());
        tokio::time::timeout(Duration::from_secs(10), self.operator_done)
            .await
            .expect("the operator stops when told to")
            .unwrap();
    }
}

#[tokio::test]
async fn a_service_that_appears_is_reconciled_and_listed_in_the_directory() {
    let r = start(Some("team")).await;
    eventually("the caches to sync", || r.readiness.is_ready()).await;
    assert!(r.directory.list().await.unwrap().is_empty());

    let (service, config) = example("chat", "team");
    r.fake.put(CONFIGS, config);
    r.fake.put(SERVICES, service);
    eventually("the service to be Ready", || {
        r.state("team", "chat").as_deref() == Some("Ready")
    })
    .await;

    let svc = r.fake.get(SERVICES, "team", "chat").unwrap();
    assert_eq!(
        svc["metadata"]["finalizers"],
        json!(["agents.vymalo.com/runtime"])
    );
    assert!(
        r.runtime
            .inner
            .spec(&RuntimeId::new("team", "chat"))
            .is_some()
    );
    // The config's own status, from the other controller.
    eventually("the config to be validated", || {
        r.fake.get(CONFIGS, "team", "chat").unwrap()["status"]["conditions"][0]["status"] == "True"
    })
    .await;
    // The directory reads the cache the status landed in.
    for _ in 0..1000 {
        let entry = r.directory.get("team", "chat").await.unwrap();
        if entry.is_some_and(|e| !e.blocked && e.agent_card.is_some()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let entry = r.directory.get("team", "chat").await.unwrap().unwrap();
    assert!(entry.listed(), "{entry:?}");
    assert!(
        r.metrics.get(
            adam_operator_controller::metric::RECONCILES,
            &[("controller", "agentservice"), ("result", "ok")]
        ) >= 2
    );
    r.stop().await;
}

#[tokio::test]
async fn a_changed_config_reconciles_the_services_that_name_it() {
    let r = start(None).await;
    let (service, config) = example("chat", "team");
    r.fake.put(CONFIGS, config);
    r.fake.put(SERVICES, service);
    eventually("Ready", || {
        r.state("team", "chat").as_deref() == Some("Ready")
    })
    .await;
    let digest = |r: &Running| {
        r.fake.get(SERVICES, "team", "chat").unwrap()["status"]["config"]["digest"].clone()
    };
    let before = digest(&r);

    let mut config = r.fake.get(CONFIGS, "team", "chat").unwrap();
    config["spec"]["harness"]["adam"]["agent"]["folder"]["files"]["instructions.md"] =
        json!("---\nname: chat\n---\nChanged while it ran.");
    r.fake.put(CONFIGS, config);
    eventually("the service to follow its config", || digest(&r) != before).await;

    // And an invalid one blocks, without anyone touching the service.
    let mut config = r.fake.get(CONFIGS, "team", "chat").unwrap();
    config["spec"]["harness"]["adam"]["agent"]["folder"]["files"] = json!({"README.md": "x"});
    r.fake.put(CONFIGS, config);
    eventually("Blocked", || {
        r.state("team", "chat").as_deref() == Some("Blocked")
    })
    .await;
    r.stop().await;
}

#[tokio::test]
async fn the_runtime_providers_signal_reconciles_its_service() {
    let r = start(Some("team")).await;
    let (service, config) = example("chat", "team");
    r.fake.put(CONFIGS, config);
    r.fake.put(SERVICES, service);
    eventually("Ready", || {
        r.state("team", "chat").as_deref() == Some("Ready")
    })
    .await;

    // A pod starts failing: no object of ours changes, the provider says the runtime did.
    r.runtime.script(Some(RuntimeStatus {
        phase: Phase::Failed,
        replicas: 0,
        issues: vec![Issue {
            role: Role::All,
            reason: IssueReason::MissingSecret {
                name: "chat-secrets".to_owned(),
            },
            message: "CreateContainerConfigError".to_owned(),
        }],
    }));
    r.runtime.signal(&RuntimeId::new("team", "chat"));
    eventually("Degraded", || {
        r.state("team", "chat").as_deref() == Some("Degraded")
    })
    .await;
    let svc = r.fake.get(SERVICES, "team", "chat").unwrap();
    assert!(
        svc["status"]["conditions"]
            .to_string()
            .contains("MissingSecret")
    );

    // It recovers, and the signal of an unrelated runtime is heard and ignored.
    r.runtime.script(None);
    r.runtime.signal(&RuntimeId::new("other", "chat"));
    r.runtime.signal(&RuntimeId::new("team", "unknown"));
    r.runtime.signal(&RuntimeId::new("team", "chat"));
    eventually("Ready again", || {
        r.state("team", "chat").as_deref() == Some("Ready")
    })
    .await;
    r.stop().await;
}

#[tokio::test]
async fn a_namespaced_operator_leaves_other_namespaces_alone() {
    let r = start(Some("team")).await;
    let (service, config) = example("chat", "elsewhere");
    r.fake.put(CONFIGS, config);
    r.fake.put(SERVICES, service);
    let (service, config) = example("coder", "team");
    r.fake.put(CONFIGS, config);
    r.fake.put(SERVICES, service);
    eventually("the watched namespace", || {
        r.state("team", "coder").as_deref() == Some("Ready")
    })
    .await;
    // Give an unwatched object every chance to be (wrongly) picked up.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        r.fake
            .get(SERVICES, "elsewhere", "chat")
            .unwrap()
            .get("status")
            .is_none()
    );
    assert!(
        r.runtime
            .inner
            .spec(&RuntimeId::new("elsewhere", "chat"))
            .is_none()
    );
    r.stop().await;
}

#[tokio::test]
async fn deleting_a_running_service_runs_the_finalizer_through_the_controller() {
    let r = start(None).await;
    let (service, config) = example("coder", "team");
    r.fake.put(CONFIGS, config);
    r.fake.put(SERVICES, service);
    eventually("Ready", || {
        r.state("team", "coder").as_deref() == Some("Ready")
    })
    .await;

    r.fake.delete(SERVICES, "team", "coder");
    eventually("the object to go", || {
        r.fake.get(SERVICES, "team", "coder").is_none()
    })
    .await;
    assert!(r.runtime.inner.ids().is_empty(), "the compute went with it");
    r.stop().await;
}

#[tokio::test]
async fn the_operator_stops_when_told_to_even_with_nothing_to_do() {
    let r = start(None).await;
    r.stop().await;
}
