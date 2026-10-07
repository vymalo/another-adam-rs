//! The parts of the controller that touch no API: the status a pass derives, the back-off, the
//! counters and the directory over a reflector's cache.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use adam_operator_api::{AgentService, ConfigStatus, Endpoints, ServiceState};
use adam_operator_controller::derive::{ConfigOutcome, Observed, StoreOutcome, derive};
use adam_operator_controller::{Metrics, ReflectorDirectory, RegistryMode, backoff};
use adam_operator_ports::{
    AgentDirectory, DirectoryError, ErrorClass, Issue, IssueReason, Phase, Role, RuntimeStatus,
    StoreState,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::runtime::{reflector, watcher};
use serde_json::json;

fn at(text: &str) -> Time {
    Time(text.parse().unwrap())
}

fn observed() -> Observed {
    Observed {
        generation: Some(3),
        suspend: false,
        a2a_enabled: true,
        registry: RegistryMode::Disabled,
        registry_full: false,
        provider: "memory",
        config: ConfigOutcome::Resolved,
        store: StoreOutcome::Answered(StoreState::SecretReferenced),
        runtime: Some(RuntimeStatus {
            phase: Phase::Ready,
            replicas: 2,
            issues: vec![],
        }),
        config_status: Some(ConfigStatus {
            name: "c".to_owned(),
            observed_generation: Some(1),
            digest: Some("sha256:x".to_owned()),
        }),
        endpoints: Some(Endpoints {
            a2a: Some("http://a/".to_owned()),
            agent_card: Some("http://a/.well-known/agent-card.json".to_owned()),
        }),
    }
}

fn types(status: &adam_operator_api::AgentServiceStatus) -> Vec<(&str, &str, &str)> {
    status
        .conditions
        .iter()
        .map(|c| (c.type_.as_str(), c.status.as_str(), c.reason.as_str()))
        .collect()
}

#[test]
fn a_healthy_service_has_the_five_conditions_of_the_status_table_in_order() {
    let status = derive(&observed(), &[], &at("2026-10-05T10:00:00Z"));
    assert_eq!(status.state, Some(ServiceState::Ready));
    assert_eq!(status.observed_generation, Some(3));
    assert_eq!(
        types(&status),
        [
            ("ConfigResolved", "True", "Resolved"),
            ("StoreReady", "True", "SecretReferenced"),
            ("RuntimeReady", "True", "Ready"),
            ("Listed", "False", "RegistryDisabled"),
            ("Ready", "True", "Reconciled"),
        ]
    );
    assert_eq!(status.runtime.as_ref().unwrap().replicas, 2);
    assert!(
        status
            .conditions
            .iter()
            .all(|c| c.observed_generation == Some(3))
    );
}

#[test]
fn a_condition_keeps_its_transition_time_until_its_status_changes() {
    let t0 = at("2026-10-05T10:00:00Z");
    let t1 = at("2026-10-05T10:05:00Z");
    let first = derive(&observed(), &[], &t0);

    // Same facts, later: nothing moves.
    let again = derive(&observed(), &first.conditions, &t1);
    assert_eq!(again, first);

    // The runtime stops being ready: RuntimeReady and Ready transition, the others do not.
    let mut o = observed();
    o.runtime = Some(RuntimeStatus {
        phase: Phase::Provisioning,
        replicas: 0,
        issues: vec![],
    });
    let later = derive(&o, &first.conditions, &t1);
    let time = |s: &adam_operator_api::AgentServiceStatus, t: &str| {
        s.conditions
            .iter()
            .find(|c| c.type_ == t)
            .unwrap()
            .last_transition_time
            .clone()
    };
    assert_eq!(time(&later, "ConfigResolved"), t0);
    assert_eq!(time(&later, "RuntimeReady"), t1);
    assert_eq!(time(&later, "Ready"), t1);
}

#[test]
fn the_state_follows_the_rule_of_the_reconciliation_section() {
    let case = |change: fn(&mut Observed)| {
        let mut o = observed();
        change(&mut o);
        derive(&o, &[], &at("2026-10-05T10:00:00Z")).state.unwrap()
    };
    use ServiceState::{Blocked, Degraded, Ready, Suspended};
    assert_eq!(case(|_| {}), Ready);
    assert_eq!(
        case(|o| o.config = ConfigOutcome::NotFound { name: "c".into() }),
        Blocked
    );
    assert_eq!(
        case(|o| o.config = ConfigOutcome::Invalid {
            message: "m".into()
        }),
        Blocked
    );
    assert_eq!(
        case(|o| o.store = StoreOutcome::Answered(StoreState::ClusterNotReady)),
        Blocked
    );
    assert_eq!(
        case(|o| o.store = StoreOutcome::NotInstalled {
            message: "m".into()
        }),
        Blocked
    );
    assert_eq!(
        case(|o| o.runtime = Some(RuntimeStatus {
            phase: Phase::Ready,
            replicas: 1,
            issues: vec![Issue {
                role: Role::All,
                reason: IssueReason::NameConflict,
                message: "m".into()
            }],
        })),
        Blocked,
        "a conflict blocks even when something of ours runs"
    );
    assert_eq!(
        case(|o| o.runtime = Some(RuntimeStatus {
            phase: Phase::Provisioning,
            replicas: 0,
            issues: vec![]
        })),
        Degraded
    );
    assert_eq!(
        case(|o| o.runtime = Some(RuntimeStatus {
            phase: Phase::Failed,
            replicas: 0,
            issues: vec![]
        })),
        Degraded
    );
    assert_eq!(
        case(|o| {
            o.suspend = true;
            o.runtime = Some(RuntimeStatus {
                phase: Phase::Suspended,
                replicas: 0,
                issues: vec![],
            });
        }),
        Suspended
    );
    // Suspended without having asked for it is not "Suspended is healthy".
    assert_eq!(
        case(|o| o.runtime = Some(RuntimeStatus {
            phase: Phase::Suspended,
            replicas: 0,
            issues: vec![]
        })),
        Degraded
    );
}

#[test]
fn the_ready_reason_is_that_of_the_first_condition_that_is_not_true() {
    let mut o = observed();
    o.config = ConfigOutcome::Invalid {
        message: "bad".into(),
    };
    o.store = StoreOutcome::NotEvaluated;
    o.runtime = Some(RuntimeStatus {
        phase: Phase::Provisioning,
        replicas: 0,
        issues: vec![],
    });
    let status = derive(&o, &[], &at("2026-10-05T10:00:00Z"));
    let ready = status.conditions.last().unwrap();
    assert_eq!(
        (
            ready.status.as_str(),
            ready.reason.as_str(),
            ready.message.as_str()
        ),
        ("False", "ConfigInvalid", "bad")
    );
}

#[test]
fn of_several_issues_the_most_actionable_speaks() {
    let issue = |reason| Issue {
        role: Role::All,
        reason,
        message: "m".into(),
    };
    let mut o = observed();
    o.runtime = Some(RuntimeStatus {
        phase: Phase::Failed,
        replicas: 0,
        issues: vec![
            issue(IssueReason::CrashLoop),
            issue(IssueReason::ImagePull),
            issue(IssueReason::MissingSecret { name: "s".into() }),
        ],
    });
    let status = derive(&o, &[], &at("2026-10-05T10:00:00Z"));
    assert_eq!(
        types(&status)[2],
        ("RuntimeReady", "False", "MissingSecret")
    );
}

#[test]
fn listed_is_decided_when_a_registry_is_served() {
    let reason = |change: fn(&mut Observed)| {
        let mut o = observed();
        o.registry = RegistryMode::Enabled;
        change(&mut o);
        let s = derive(&o, &[], &at("2026-10-05T10:00:00Z"));
        let c = s
            .conditions
            .iter()
            .find(|c| c.type_ == "Listed")
            .unwrap()
            .clone();
        (c.status, c.reason)
    };
    assert_eq!(reason(|_| {}), ("True".into(), "Listed".into()));
    assert_eq!(
        reason(|o| o.a2a_enabled = false),
        ("False".into(), "A2ADisabled".into())
    );
    assert_eq!(
        reason(|o| o.config = ConfigOutcome::NotFound { name: "c".into() }),
        ("False".into(), "ServiceBlocked".into())
    );
    // A full registry lists nothing, and says so on the services it would have listed.
    assert_eq!(
        reason(|o| o.registry_full = true),
        ("False".into(), "RegistryFull".into())
    );
    // A service that is not listable anyway keeps its own, more specific reason.
    assert_eq!(
        reason(|o| {
            o.registry_full = true;
            o.a2a_enabled = false;
        }),
        ("False".into(), "A2ADisabled".into())
    );
    assert_eq!(
        reason(|o| {
            o.registry_full = true;
            o.config = ConfigOutcome::NotFound { name: "c".into() };
        }),
        ("False".into(), "ServiceBlocked".into())
    );
    // `Listed` informs: it never makes a ready agent unready.
    let mut o = observed();
    o.registry = RegistryMode::Enabled;
    o.a2a_enabled = false;
    let s = derive(&o, &[], &at("2026-10-05T10:00:00Z"));
    assert_eq!(s.state, Some(ServiceState::Ready));
    assert_eq!(types(&s)[4], ("Ready", "True", "Reconciled"));
}

#[test]
fn a_missing_secret_is_named_in_the_condition_and_the_providers_words_are_kept() {
    // A Secret's *name* is a reference, not a value; the provider's message never holds the name.
    let mut o = observed();
    o.runtime = Some(RuntimeStatus {
        phase: Phase::Failed,
        replicas: 0,
        issues: vec![Issue {
            role: Role::All,
            reason: IssueReason::MissingSecret {
                name: "coder-secrets".into(),
            },
            message: "CreateContainerConfigError".into(),
        }],
    });
    let status = derive(&o, &[], &at("2026-10-05T10:00:00Z"));
    let message = &status.conditions[2].message;
    assert!(
        message.contains("\"coder-secrets\"") && message.contains("CreateContainerConfigError"),
        "{message}"
    );
}

// ------------------------------------------------------------------ back-off

#[test]
fn the_back_off_is_by_class() {
    let secs = |c, n| backoff(c, n).as_secs();
    assert_eq!(
        (1..=8)
            .map(|n| secs(ErrorClass::Transient, n))
            .collect::<Vec<_>>(),
        [5, 10, 20, 40, 80, 160, 300, 300]
    );
    assert_eq!(secs(ErrorClass::Conflict, 1), 5);
    assert_eq!(secs(ErrorClass::NotFound, 9), 5);
    assert_eq!(
        (1..=7)
            .map(|n| secs(ErrorClass::Internal, n))
            .collect::<Vec<_>>(),
        [30, 60, 120, 240, 480, 600, 600]
    );
    assert_eq!(
        secs(ErrorClass::Invalid, 1),
        600,
        "the same input never succeeds"
    );
    assert_eq!(secs(ErrorClass::Unsupported, 1), 600);
    assert_eq!(
        backoff(ErrorClass::Transient, u32::MAX),
        Duration::from_secs(300),
        "no overflow"
    );
}

// ------------------------------------------------------------------- metrics

#[test]
fn the_counters_render_as_prometheus_text() {
    let m = Metrics::new();
    m.inc(
        "adam_operator_reconcile_total",
        &[("controller", "agentservice"), ("result", "ok")],
    );
    m.inc(
        "adam_operator_reconcile_total",
        &[("controller", "agentservice"), ("result", "ok")],
    );
    m.inc(
        "adam_operator_reconcile_total",
        &[("controller", "agentservice"), ("result", "error")],
    );
    m.inc("adam_operator_runtime_signals_total", &[]);
    m.inc(
        "adam_operator_service_state_changes_total",
        &[("state", "a\"b\\c")],
    );
    assert_eq!(
        m.get(
            "adam_operator_reconcile_total",
            &[("controller", "agentservice"), ("result", "ok")]
        ),
        2
    );
    assert_eq!(
        m.get(
            "adam_operator_reconcile_total",
            &[("controller", "nothing")]
        ),
        0
    );
    let text = m.render();
    assert!(
        text.contains("# TYPE adam_operator_reconcile_total counter\n"),
        "{text}"
    );
    assert!(
        text.contains(
            "adam_operator_reconcile_total{controller=\"agentservice\",result=\"ok\"} 2\n"
        ),
        "{text}"
    );
    assert!(
        text.contains("adam_operator_runtime_signals_total 1\n"),
        "{text}"
    );
    assert!(
        text.contains(r#"state="a\"b\\c""#),
        "label values are escaped: {text}"
    );
    assert_eq!(
        text.matches("# TYPE adam_operator_reconcile_total").count(),
        1,
        "one header per series name"
    );
}

// ----------------------------------------------------------------- directory

fn service(ns: &str, name: &str, extra: serde_json::Value) -> AgentService {
    let mut v = json!({
        "apiVersion": "agents.vymalo.com/v1alpha1",
        "kind": "AgentService",
        "metadata": {"name": name, "namespace": ns},
        "spec": {
            "description": format!("{name} does things"),
            "configRef": {"name": "c"},
            "interfaces": {"a2a": {"enabled": true, "bearerTokensSecretRef": {"name": "s", "key": "k"}}},
            "store": {"postgres": {"secretRef": {"name": "db", "key": "uri"}}},
            "registry": {"title": name.to_uppercase(), "tags": ["x"]},
        },
    });
    if let (Some(extra), Some(obj)) = (extra.as_object(), v.as_object_mut()) {
        for (k, e) in extra {
            obj.insert(k.clone(), e.clone());
        }
    }
    serde_json::from_value(v).unwrap()
}

fn ready(card: &str) -> serde_json::Value {
    json!({"status": {"state": "Ready", "endpoints": {"agentCard": card}, "conditions": []}})
}

#[tokio::test]
async fn the_directory_is_not_ready_before_the_first_sync_and_lists_in_order_after() {
    let (reader, mut writer) = reflector::store::<AgentService>();
    let directory = ReflectorDirectory::new(reader);
    assert!(!directory.is_synced());
    assert!(
        matches!(directory.list().await, Err(DirectoryError::NotReady)),
        "never an empty list that looks true"
    );

    writer.apply_watcher_event(&watcher::Event::Init);
    writer.apply_watcher_event(&watcher::Event::InitApply(service(
        "b",
        "zed",
        ready("http://zed/card"),
    )));
    writer.apply_watcher_event(&watcher::Event::InitApply(service(
        "a",
        "beta",
        ready("http://beta/card"),
    )));
    writer.apply_watcher_event(&watcher::Event::InitApply(service("a", "alpha", json!({}))));
    writer.apply_watcher_event(&watcher::Event::InitApply(service(
        "a",
        "gone",
        json!({"metadata": {"name": "gone", "namespace": "a", "deletionTimestamp": "2026-10-05T10:00:00Z"}}),
    )));
    writer.apply_watcher_event(&watcher::Event::InitDone);
    assert!(directory.is_synced());

    let entries = directory.list().await.unwrap();
    let names: Vec<_> = entries
        .iter()
        .map(|e| format!("{}/{}", e.scope, e.name))
        .collect();
    assert_eq!(
        names,
        ["a/alpha", "a/beta", "b/zed"],
        "by scope and name, and not a service being deleted"
    );

    let beta = directory.get("a", "beta").await.unwrap().unwrap();
    assert_eq!(beta.title.as_deref(), Some("BETA"));
    assert_eq!(beta.description.as_deref(), Some("beta does things"));
    assert_eq!(beta.tags, ["x"]);
    assert_eq!(beta.agent_card.as_deref(), Some("http://beta/card"));
    assert!(beta.a2a_enabled && !beta.blocked && beta.listed());

    // A service the controller has not reconciled is blocked: a registry lists nothing unapplied.
    let alpha = directory.get("a", "alpha").await.unwrap().unwrap();
    assert!(alpha.blocked && !alpha.listed());
    assert!(directory.get("a", "nothing").await.unwrap().is_none());
}

#[tokio::test]
async fn the_directory_follows_the_cache() {
    let (reader, mut writer) = reflector::store::<AgentService>();
    let directory = ReflectorDirectory::new(reader);
    writer.apply_watcher_event(&watcher::Event::Init);
    writer.apply_watcher_event(&watcher::Event::InitDone);
    assert!(directory.list().await.unwrap().is_empty());

    writer.apply_watcher_event(&watcher::Event::Apply(service(
        "a",
        "x",
        ready("http://x/card"),
    )));
    assert_eq!(directory.list().await.unwrap().len(), 1);
    let blocked = json!({"status": {"state": "Blocked", "conditions": []}});
    writer.apply_watcher_event(&watcher::Event::Apply(service("a", "x", blocked)));
    assert!(directory.get("a", "x").await.unwrap().unwrap().blocked);
    writer.apply_watcher_event(&watcher::Event::Delete(service("a", "x", json!({}))));
    assert!(directory.list().await.unwrap().is_empty());
}

#[test]
fn a_finalizer_patch_that_loses_its_race_is_a_conflict_not_bad_input() {
    use adam_operator_controller::Error;
    use adam_operator_ports::Classify;
    use kube::runtime::finalizer;

    let status = |code: u16| {
        kube::Error::Api(Box::new(
            kube::core::Status::failure("x", "Invalid").with_code(code),
        ))
    };
    let add = |code| Error::Finalizer(Box::new(finalizer::Error::AddFinalizer(status(code))));
    let remove = |code| Error::Finalizer(Box::new(finalizer::Error::RemoveFinalizer(status(code))));
    // The `test` operation of the patch failed: somebody changed the finalizers first.
    assert_eq!(add(422).class(), ErrorClass::Conflict);
    assert_eq!(remove(422).class(), ErrorClass::Conflict);
    assert_eq!(add(409).class(), ErrorClass::Conflict);
    // The rest of the classes stand.
    assert_eq!(add(503).class(), ErrorClass::Transient);
    assert_eq!(add(403).class(), ErrorClass::Internal);
    // A 422 of the status patch is the operator's own bug: it is not a race.
    assert_eq!(Error::Kube(status(422)).class(), ErrorClass::Invalid);
}
