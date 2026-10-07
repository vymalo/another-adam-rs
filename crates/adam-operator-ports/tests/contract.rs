//! What the types promise, and that the suites would fail a provider that breaks the promise.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use adam_operator_ports::memory::{MemoryDirectory, MemoryRuntime, MemoryStore};
use adam_operator_ports::testkit::{self, RuntimeUnderTest, SENTINEL, sample_spec};
use adam_operator_ports::{
    AgentDirectory, Capabilities, Classify, CnpgSpec, DeleteOutcome, DeletionPolicy,
    DirectoryEntry, DirectoryError, Endpoint, EnvValue, ErrorClass, IssueReason, OwnerHandle,
    Phase, RuntimeError, RuntimeId, RuntimeProvider, RuntimeSpec, RuntimeStatus, SecretRef,
    StoreError, StoreId, StoreKind, StoreProvisioner, StoreSpec, Surface, VolumeSource,
};
use futures::StreamExt;
use futures::stream::BoxStream;

type Break = Box<dyn Fn(&mut RuntimeSpec)>;

fn id() -> RuntimeId {
    RuntimeId::new("ns", testkit::unique("rt"))
}

// ------------------------------------------------------------- the spec

#[test]
fn the_sample_spec_is_valid() {
    sample_spec(&id()).check().unwrap();
}

#[test]
fn check_names_the_broken_invariant() {
    let base = sample_spec(&id());
    let broken: Vec<(&str, Break)> = vec![
        ("no workload", Box::new(|s| s.workloads.clear())),
        ("selects", Box::new(|s| s.network.selects = "nope".into())),
        ("port", Box::new(|s| s.network.port = 0)),
        (
            "image",
            Box::new(|s| s.workloads[0].container.image.clear()),
        ),
        (
            "mounts",
            Box::new(|s| s.workloads[0].container.mounts[0].volume = "ghost".into()),
        ),
        (
            "file set",
            Box::new(|s| {
                s.workloads[0].volumes[1].source = VolumeSource::Files {
                    file_set: "ghost".into(),
                    mode: 0o444,
                }
            }),
        ),
        (
            "twice",
            Box::new(|s| {
                let again = s.workloads[0].container.env[0].clone();
                s.workloads[0].container.env.push(again);
            }),
        ),
        (
            "empty secret reference",
            Box::new(|s| {
                s.workloads[0].container.env[1].value = EnvValue::Secret(SecretRef::new("", "k"));
            }),
        ),
        (
            "appears twice",
            Box::new(|s| {
                let again = s.workloads[0].clone();
                s.workloads.push(again);
            }),
        ),
    ];
    for (needle, break_it) in broken {
        let mut spec = base.clone();
        break_it(&mut spec);
        let err = spec.check().unwrap_err();
        assert_eq!(err.class(), ErrorClass::Invalid);
        assert!(err.to_string().contains(needle), "{needle}: {err}");
    }
}

#[test]
fn worker_replicas_counts_the_roles_that_step_runs() {
    use adam_operator_ports::Role;
    let mut spec = sample_spec(&id());
    spec.workloads[0].role = Role::Worker;
    spec.workloads[0].replicas = 4;
    let mut front = spec.workloads[0].clone();
    front.name = "front".into();
    front.role = Role::ControlPlane;
    front.replicas = 2;
    spec.workloads.push(front);
    assert_eq!(spec.worker_replicas(), 4);
}

#[test]
fn a_spec_round_trips_through_json() {
    let spec = sample_spec(&id());
    let json = serde_json::to_string(&spec).unwrap();
    assert_eq!(serde_json::from_str::<RuntimeSpec>(&json).unwrap(), spec);
}

// ----------------------------------------------------------- the errors

#[test]
fn every_error_has_a_class_and_only_the_right_ones_retry() {
    let boxed = || -> adam_operator_ports::BoxError { "x".into() };
    let cases = [
        (
            RuntimeError::InvalidSpec("x".into()).class(),
            ErrorClass::Invalid,
            false,
        ),
        (
            RuntimeError::NotFound(id()).class(),
            ErrorClass::NotFound,
            false,
        ),
        (
            RuntimeError::Unsupported("x").class(),
            ErrorClass::Unsupported,
            false,
        ),
        (
            RuntimeError::Unavailable(boxed()).class(),
            ErrorClass::Transient,
            true,
        ),
        (
            RuntimeError::Conflict("x".into()).class(),
            ErrorClass::Conflict,
            true,
        ),
        (
            RuntimeError::Internal(boxed()).class(),
            ErrorClass::Internal,
            false,
        ),
        (
            StoreError::NotInstalled { what: "x" }.class(),
            ErrorClass::Unsupported,
            false,
        ),
        (
            StoreError::Unavailable(boxed()).class(),
            ErrorClass::Transient,
            true,
        ),
        (
            DirectoryError::NotReady.class(),
            ErrorClass::Transient,
            true,
        ),
    ];
    for (class, expected, retry) in cases {
        assert_eq!(class, expected);
        assert_eq!(class.is_retryable(), retry);
    }
}

// ------------------------------------------- the suites catch a bad provider

/// A provider that writes the name of a Secret into plain text, as a careless one would when it
/// resolves a reference.
#[derive(Clone)]
struct Leaky(MemoryRuntime);

impl RuntimeProvider for Leaky {
    fn name(&self) -> &'static str {
        "leaky"
    }
    fn capabilities(&self) -> Capabilities {
        self.0.capabilities()
    }
    async fn ensure(&self, i: &RuntimeId, s: &RuntimeSpec) -> Result<RuntimeStatus, RuntimeError> {
        self.0.ensure(i, s).await
    }
    async fn suspend(&self, i: &RuntimeId) -> Result<RuntimeStatus, RuntimeError> {
        self.0.suspend(i).await
    }
    async fn delete(&self, i: &RuntimeId) -> Result<DeleteOutcome, RuntimeError> {
        self.0.delete(i).await
    }
    async fn status(&self, i: &RuntimeId) -> Result<RuntimeStatus, RuntimeError> {
        self.0.status(i).await
    }
    async fn endpoint(&self, i: &RuntimeId, s: Surface) -> Result<Endpoint, RuntimeError> {
        self.0.endpoint(i, s).await
    }
    fn watch(&self) -> BoxStream<'static, RuntimeId> {
        self.0.watch()
    }
}

impl RuntimeUnderTest for Leaky {
    async fn materialised(&self, _: &RuntimeId) -> Vec<String> {
        vec![format!("MODEL_API_KEY={SENTINEL}-secrets")]
    }
}

#[tokio::test]
#[should_panic(expected = "reached plain text")]
async fn the_suite_fails_a_provider_that_materialises_a_secret() {
    testkit::runtime::no_secret_value_materialises(Leaky(MemoryRuntime::new())).await;
}

/// A provider that never says a change happened.
#[derive(Clone)]
struct Silent(MemoryRuntime);

impl RuntimeProvider for Silent {
    fn name(&self) -> &'static str {
        "silent"
    }
    fn capabilities(&self) -> Capabilities {
        self.0.capabilities()
    }
    async fn ensure(&self, i: &RuntimeId, s: &RuntimeSpec) -> Result<RuntimeStatus, RuntimeError> {
        self.0.ensure(i, s).await
    }
    async fn suspend(&self, i: &RuntimeId) -> Result<RuntimeStatus, RuntimeError> {
        self.0.suspend(i).await
    }
    async fn delete(&self, i: &RuntimeId) -> Result<DeleteOutcome, RuntimeError> {
        self.0.delete(i).await
    }
    async fn status(&self, i: &RuntimeId) -> Result<RuntimeStatus, RuntimeError> {
        self.0.status(i).await
    }
    async fn endpoint(&self, i: &RuntimeId, s: Surface) -> Result<Endpoint, RuntimeError> {
        self.0.endpoint(i, s).await
    }
    fn watch(&self) -> BoxStream<'static, RuntimeId> {
        futures::stream::pending().boxed()
    }
}

impl RuntimeUnderTest for Silent {
    async fn materialised(&self, i: &RuntimeId) -> Vec<String> {
        self.0.materialised(i).await
    }
}

#[tokio::test(start_paused = true)]
#[should_panic(expected = "was not reported by watch()")]
async fn the_suite_fails_a_provider_whose_watch_is_silent() {
    testkit::runtime::watch_reports_a_change(Silent(MemoryRuntime::new())).await;
}

// -------------------------------------------------------- memory runtime

#[tokio::test]
async fn a_name_that_is_not_ours_is_an_issue_and_nothing_is_made() {
    let rt = MemoryRuntime::new();
    let id = id();
    rt.set_foreign(&id, true);
    let status = rt.ensure(&id, &sample_spec(&id)).await.unwrap();
    assert!(status.is_name_conflict(), "{status:?}");
    assert_eq!(status.issues[0].reason, IssueReason::NameConflict);
    assert_eq!(status.phase, Phase::Absent);
    assert!(rt.spec(&id).is_none(), "the adoption guard changes nothing");
    rt.set_foreign(&id, false);
    assert!(
        !rt.ensure(&id, &sample_spec(&id))
            .await
            .unwrap()
            .is_name_conflict()
    );
}

#[tokio::test]
async fn a_forced_status_is_what_status_reports() {
    let rt = MemoryRuntime::new();
    let id = id();
    rt.ensure(&id, &sample_spec(&id)).await.unwrap();
    let issue = adam_operator_ports::Issue {
        role: adam_operator_ports::Role::All,
        reason: IssueReason::MissingSecret {
            name: "coder-secrets".into(),
        },
        message: "a Secret is missing".into(),
    };
    rt.force_status(&id, Some((Phase::Provisioning, vec![issue.clone()])));
    let status = rt.status(&id).await.unwrap();
    assert_eq!(
        (status.phase, status.issues),
        (Phase::Provisioning, vec![issue])
    );
    rt.force_status(&id, None);
    assert_eq!(rt.status(&id).await.unwrap().phase, Phase::Ready);
}

#[tokio::test]
async fn an_unreachable_backend_is_a_transient_error_everywhere() {
    let rt = MemoryRuntime::new();
    let id = id();
    rt.set_unavailable(true);
    let spec = sample_spec(&id);
    for err in [
        rt.ensure(&id, &spec).await.unwrap_err(),
        rt.status(&id).await.unwrap_err(),
        rt.delete(&id).await.map(|_| ()).unwrap_err(),
    ] {
        assert_eq!(err.class(), ErrorClass::Transient);
    }
}

#[tokio::test]
async fn the_owner_is_kept_and_opaque() {
    let rt = MemoryRuntime::new();
    let id = id();
    let mut spec = sample_spec(&id);
    spec.owner = OwnerHandle::new("whatever the provider understands");
    rt.ensure(&id, &spec).await.unwrap();
    assert_eq!(
        rt.spec(&id).unwrap().owner.token(),
        "whatever the provider understands"
    );
}

// ---------------------------------------------------------- memory store

#[tokio::test]
async fn a_missing_cnpg_api_is_reported_not_swallowed() {
    let store = MemoryStore::new();
    store.set_cnpg_installed(false);
    let spec = StoreSpec {
        owner: OwnerHandle::none(),
        deletion: DeletionPolicy::Retain,
        kind: StoreKind::Cnpg(CnpgSpec {
            instances: 1,
            size: "5Gi".into(),
            storage_class: None,
        }),
    };
    let err = store
        .ensure(&StoreId::new("ns", "coder"), &spec)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::NotInstalled {
                what: "CloudNativePG"
            }
        ),
        "{err}"
    );
    store.set_cnpg_installed(true);
    store.set_cluster_ready(false);
    let status = store
        .ensure(&StoreId::new("ns", "coder"), &spec)
        .await
        .unwrap();
    assert_eq!(
        status.state,
        adam_operator_ports::StoreState::ClusterNotReady
    );
    assert_eq!(
        status.connection,
        adam_operator_ports::cnpg_connection("coder")
    );
    assert_eq!(status.connection, SecretRef::new("coder-db-app", "uri"));
}

// ------------------------------------------------------ memory directory

#[test]
fn an_entry_is_listed_when_a2a_is_on_it_is_not_blocked_and_has_a_card() {
    let base = DirectoryEntry {
        scope: "ns".into(),
        name: "coder".into(),
        title: None,
        description: None,
        tags: Vec::new(),
        agent_card: Some("http://coder.ns.svc:8080/.well-known/agent-card.json".into()),
        a2a_enabled: true,
        blocked: false,
    };
    assert!(base.listed());
    assert!(
        !DirectoryEntry {
            a2a_enabled: false,
            ..base.clone()
        }
        .listed()
    );
    assert!(
        !DirectoryEntry {
            blocked: true,
            ..base.clone()
        }
        .listed()
    );
    assert!(
        !DirectoryEntry {
            agent_card: None,
            ..base
        }
        .listed()
    );
}

#[tokio::test]
async fn a_directory_that_has_not_synced_is_not_an_empty_list() {
    let d = MemoryDirectory::new();
    d.set_ready(false);
    assert!(matches!(d.list().await, Err(DirectoryError::NotReady)));
    assert!(matches!(
        d.get("ns", "x").await,
        Err(DirectoryError::NotReady)
    ));
    d.set_ready(true);
    assert!(d.list().await.unwrap().is_empty());
}
