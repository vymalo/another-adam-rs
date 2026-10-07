//! Conformance suites for the three traits, in the style of adam-rs's `adam-store-testkit`: every
//! implementation runs the same cases, so "passes the testkit" means "behaves like every other
//! one".
//!
//! ```ignore
//! async fn make() -> Option<MyProvider> {
//!     // None skips the suite; with ADAM_OPERATOR_TEST_REQUIRE_BACKEND=1 it fails instead
//!     let backend = adam_operator_ports::testkit::backend("MY_BACKEND_URL")?;
//!     Some(MyProvider::connect(&backend).await)
//! }
//! adam_operator_ports::runtime_provider_conformance!(make);
//! ```
//!
//! The macros expand to `#[tokio::test]` functions, so the crate that uses them needs `tokio` with
//! `macros` and `rt` as a dev-dependency. Cases name everything with a fresh id, so they run in
//! parallel against one backend and need no cleanup between them.

// A conformance suite is test code: a failed unwrap is a failed assertion.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::missing_panics_doc)]

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::{
    Classify, Container, DeletionPolicy, DirectoryEntry, EnvVar, ErrorClass, FileSet,
    FsGroupChangePolicy, Mount, Network, OwnerHandle, PersistentVolume, Phase, Probe, ProbeAction,
    Probes, Resources, Role, RuntimeId, RuntimeProvider, RuntimeSpec, SecretRef, Security, Sharing,
    StoreId, StoreProvisioner, VolumeSource, VolumeSpec, Workload,
};

/// The environment variable that turns a skipped suite into a failure (CI sets it).
pub const REQUIRE_BACKEND_VAR: &str = "ADAM_OPERATOR_TEST_REQUIRE_BACKEND";

/// A string no spec may let reach plain text: the cases put it in the names of Secrets.
pub const SENTINEL: &str = "adam-operator-sentinel-never-a-value-8f3c1d";

/// Report a skipped suite: a note on stderr, or a panic when [`REQUIRE_BACKEND_VAR`] is `1` or
/// `true`.
///
/// # Panics
///
/// When skipping is forbidden.
pub fn skipped(reason: &str) {
    let forbidden = matches!(
        std::env::var(REQUIRE_BACKEND_VAR).as_deref(),
        Ok("1" | "true")
    );
    assert!(
        !forbidden,
        "test would be skipped ({reason}) but {REQUIRE_BACKEND_VAR}=1 forbids skipping"
    );
    eprintln!("skipped: {reason}");
}

/// The value of an environment variable that names a backend, or `None` (and a skip note, or a
/// panic under [`REQUIRE_BACKEND_VAR`]).
pub fn backend(var: &str) -> Option<String> {
    match std::env::var(var) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => {
            skipped(&format!("{var} is not set"));
            None
        }
    }
}

/// A name no other case, process or run uses.
pub fn unique(prefix: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    // DNS-label safe and short: a Kubernetes name is at most 63 characters.
    format!("{prefix}-{:x}{nanos:x}{n:x}", std::process::id())
}

/// A [`RuntimeProvider`] the suite can inspect.
pub trait RuntimeUnderTest: RuntimeProvider + 'static {
    /// Every plain-text value the provider wrote to its backend for `id`: literal variables,
    /// commands, file contents, names, labels, annotations. A secret *reference* (the name and key
    /// of a Secret) is how a secret travels and is not plain text; a value read from a Secret is.
    /// The case "no secret value materialises" fails when the sentinel is in the result.
    fn materialised(&self, id: &RuntimeId) -> impl Future<Output = Vec<String>> + Send;
}

/// A [`StoreProvisioner`](crate::StoreProvisioner) the suite can inspect.
pub trait StoreUnderTest: StoreProvisioner + 'static {
    /// Every plain-text value written to the backend for `id`, as for [`RuntimeUnderTest`].
    fn materialised(&self, id: &StoreId) -> impl Future<Output = Vec<String>> + Send;
}

/// An [`AgentDirectory`](crate::AgentDirectory) the suite can feed: the source of its entries.
pub trait DirectoryUnderTest: crate::AgentDirectory + 'static {
    /// Add or replace an entry in the source the directory reads.
    fn put(&self, entry: DirectoryEntry) -> impl Future<Output = ()> + Send;
    /// Remove an entry from the source.
    fn remove(&self, scope: &str, name: &str) -> impl Future<Output = ()> + Send;
}

/// A resolved-looking spec that uses every kind of thing a provider must handle: a workload with a
/// sidecar, literal and secret variables, a pod-name variable, a per-replica volume `work`, a file
/// set mounted read-only, a secret file, probes and resources. Secret names carry [`SENTINEL`].
pub fn sample_spec(id: &RuntimeId) -> RuntimeSpec {
    let name = id.name().to_owned();
    let mut files = BTreeMap::new();
    files.insert(
        "instructions.md".to_owned(),
        "Your name is Sample.\n".to_owned(),
    );
    files.insert("skills/review/SKILL.md".to_owned(), "# Review\n".to_owned());
    let probe = |path: &str, period| Probe {
        action: ProbeAction::Http {
            path: path.to_owned(),
        },
        period_secs: period,
        timeout_secs: Some(3),
        failure_threshold: Some(4),
    };
    let mut requests = BTreeMap::new();
    requests.insert("cpu".to_owned(), "100m".to_owned());
    requests.insert("memory".to_owned(), "256Mi".to_owned());
    let mut limits = BTreeMap::new();
    limits.insert("memory".to_owned(), "512Mi".to_owned());
    RuntimeSpec {
        owner: OwnerHandle::new("sample-owner"),
        deletion: DeletionPolicy::Retain,
        digest: "sha256:0000000000000000000000000000000000000000000000000000000000000000"
            .to_owned(),
        suspend: false,
        workloads: vec![Workload {
            name: name.clone(),
            role: Role::All,
            replicas: 2,
            stable_identity: true,
            container: Container {
                name: "agent".to_owned(),
                image: "registry.example/agent:sha-0000000".to_owned(),
                command: vec!["tini".to_owned(), "--".to_owned(), "adam-agent".to_owned()],
                args: Vec::new(),
                env: vec![
                    EnvVar::literal("LISTEN_ADDR", "0.0.0.0:8080"),
                    EnvVar::secret(
                        "MODEL_API_KEY",
                        SecretRef::new(format!("{SENTINEL}-secrets"), "MODEL_API_KEY"),
                    ),
                    EnvVar {
                        name: "WORKER_ID".to_owned(),
                        value: crate::EnvValue::PodName,
                    },
                ],
                port: Some(8080),
                mounts: vec![
                    Mount {
                        volume: "work".to_owned(),
                        path: "/work".to_owned(),
                        read_only: false,
                    },
                    Mount {
                        volume: "folder".to_owned(),
                        path: "/etc/adam/agent".to_owned(),
                        read_only: true,
                    },
                    Mount {
                        volume: "key".to_owned(),
                        path: "/var/run/secrets/key".to_owned(),
                        read_only: true,
                    },
                ],
                probes: Probes {
                    startup: Some(probe("/healthz", 3)),
                    liveness: Some(probe("/healthz", 15)),
                    readiness: Some(probe("/healthz", 5)),
                },
                resources: Resources { requests, limits },
            },
            sidecars: vec![Container {
                name: "helper".to_owned(),
                image: "registry.example/agent:sha-0000000".to_owned(),
                command: vec!["helper".to_owned()],
                args: vec!["--port".to_owned(), "8082".to_owned()],
                env: Vec::new(),
                port: None,
                mounts: Vec::new(),
                probes: Probes {
                    startup: Some(Probe {
                        action: ProbeAction::Exec {
                            command: vec!["true".to_owned()],
                        },
                        period_secs: 2,
                        timeout_secs: None,
                        failure_threshold: Some(30),
                    }),
                    liveness: None,
                    readiness: None,
                },
                resources: Resources::default(),
            }],
            volumes: vec![
                VolumeSpec {
                    name: "work".to_owned(),
                    source: VolumeSource::Persistent(PersistentVolume {
                        size: "20Gi".to_owned(),
                        storage_class: None,
                        sharing: Sharing::PerReplica,
                    }),
                },
                VolumeSpec {
                    name: "folder".to_owned(),
                    source: VolumeSource::Files {
                        file_set: format!("{name}-agent-0a1b2c3d"),
                        mode: 0o444,
                    },
                },
                VolumeSpec {
                    name: "key".to_owned(),
                    source: VolumeSource::SecretFile {
                        secret: SecretRef::new(format!("{SENTINEL}-key"), "private-key.pem"),
                        file: "private-key.pem".to_owned(),
                        mode: 0o440,
                    },
                },
            ],
            security: Security {
                run_as_user: 10001,
                run_as_group: 10001,
                fs_group: 10001,
                fs_group_change_policy: Some(FsGroupChangePolicy::OnRootMismatch),
            },
            termination_grace_secs: Some(120),
            min_available: None,
        }],
        file_sets: vec![FileSet {
            name: format!("{name}-agent-0a1b2c3d"),
            files,
            immutable: true,
        }],
        network: Network {
            port: 8080,
            selects: name,
            allow_from: Vec::new(),
        },
    }
}

/// Cases of the runtime suite, as plain async functions for harnesses that do not use the macro.
#[allow(missing_docs)] // each case is named for what it asserts
pub mod runtime {
    use futures::StreamExt;

    use super::*;

    fn id() -> RuntimeId {
        RuntimeId::new("adam-operator-test", unique("rt"))
    }

    async fn ready_or_rolling<R: RuntimeUnderTest>(p: &R, id: &RuntimeId, spec: &RuntimeSpec) {
        let status = p.ensure(id, spec).await.unwrap();
        assert!(
            matches!(status.phase, Phase::Provisioning | Phase::Ready),
            "a running spec is Provisioning or Ready, got {:?}",
            status.phase
        );
    }

    pub async fn ensure_makes_the_runtime<R: RuntimeUnderTest>(p: R) {
        let id = id();
        let spec = sample_spec(&id);
        let status = p.ensure(&id, &spec).await.unwrap();
        assert!(
            matches!(status.phase, Phase::Provisioning | Phase::Ready),
            "{:?}",
            status.phase
        );
        assert!(!status.is_name_conflict());
        assert!(status.replicas <= spec.worker_replicas());
        let again = p.status(&id).await.unwrap();
        assert_ne!(
            again.phase,
            Phase::Absent,
            "an ensured runtime is not absent"
        );
        p.delete(&id).await.unwrap();
    }

    pub async fn status_of_an_unknown_runtime_is_absent<R: RuntimeUnderTest>(p: R) {
        let status = p.status(&id()).await.unwrap();
        assert_eq!(status.phase, Phase::Absent);
        assert_eq!(status.replicas, 0);
        assert!(status.issues.is_empty());
    }

    pub async fn ensure_twice_is_one_runtime<R: RuntimeUnderTest>(p: R) {
        let id = id();
        let spec = sample_spec(&id);
        ready_or_rolling(&p, &id, &spec).await;
        ready_or_rolling(&p, &id, &spec).await;
        p.delete(&id).await.unwrap();
        assert_eq!(
            p.status(&id).await.unwrap().phase,
            Phase::Absent,
            "one delete removes it"
        );
    }

    pub async fn ensure_accepts_a_changed_spec<R: RuntimeUnderTest>(p: R) {
        let id = id();
        let mut spec = sample_spec(&id);
        ready_or_rolling(&p, &id, &spec).await;
        spec.digest =
            "sha256:1111111111111111111111111111111111111111111111111111111111111111".to_owned();
        spec.workloads[0].container.image = "registry.example/agent:sha-1111111".to_owned();
        spec.workloads[0].replicas = 3;
        ready_or_rolling(&p, &id, &spec).await;
        p.delete(&id).await.unwrap();
    }

    pub async fn ensure_rejects_a_malformed_spec<R: RuntimeUnderTest>(p: R) {
        let id = id();
        let mut spec = sample_spec(&id);
        spec.network.selects = "no-such-workload".to_owned();
        let err = p.ensure(&id, &spec).await.unwrap_err();
        assert_eq!(err.class(), ErrorClass::Invalid, "{err}");
        assert_eq!(
            p.status(&id).await.unwrap().phase,
            Phase::Absent,
            "nothing was made"
        );

        let mut spec = sample_spec(&id);
        spec.workloads.clear();
        let err = p.ensure(&id, &spec).await.unwrap_err();
        assert_eq!(err.class(), ErrorClass::Invalid, "{err}");
    }

    pub async fn suspend_scales_to_zero_and_ensure_wakes<R: RuntimeUnderTest>(p: R) {
        let id = id();
        let spec = sample_spec(&id);
        if !p.capabilities().suspend {
            let err = p.suspend(&id).await.unwrap_err();
            assert_eq!(err.class(), ErrorClass::Unsupported, "{err}");
            let mut spec = spec;
            spec.suspend = true;
            let err = p.ensure(&id, &spec).await.unwrap_err();
            assert_eq!(err.class(), ErrorClass::Unsupported, "{err}");
            return;
        }
        ready_or_rolling(&p, &id, &spec).await;
        let status = p.suspend(&id).await.unwrap();
        assert_eq!(status.phase, Phase::Suspended);
        assert_eq!(status.replicas, 0);
        assert_eq!(p.status(&id).await.unwrap().phase, Phase::Suspended);
        // Ensuring a suspended runtime wakes it: there is no separate activate.
        ready_or_rolling(&p, &id, &spec).await;
        // A spec that is suspended is suspended by ensure alone.
        let mut asleep = spec;
        asleep.suspend = true;
        let status = p.ensure(&id, &asleep).await.unwrap();
        assert_eq!(status.phase, Phase::Suspended);
        p.delete(&id).await.unwrap();
    }

    pub async fn suspend_of_an_unknown_runtime_is_not_found<R: RuntimeUnderTest>(p: R) {
        let err = p.suspend(&id()).await.unwrap_err();
        let expected = if p.capabilities().suspend {
            ErrorClass::NotFound
        } else {
            ErrorClass::Unsupported
        };
        assert_eq!(err.class(), expected, "{err}");
    }

    pub async fn delete_retains_data_under_retain<R: RuntimeUnderTest>(p: R) {
        let id = id();
        let mut spec = sample_spec(&id);
        spec.deletion = DeletionPolicy::Retain;
        ready_or_rolling(&p, &id, &spec).await;
        let outcome = p.delete(&id).await.unwrap();
        assert!(outcome.existed);
        assert_eq!(
            outcome.retained_volumes,
            vec!["work".to_owned()],
            "the work volume stays"
        );
        assert_eq!(p.status(&id).await.unwrap().phase, Phase::Absent);
    }

    pub async fn delete_removes_data_under_delete<R: RuntimeUnderTest>(p: R) {
        let id = id();
        let mut spec = sample_spec(&id);
        spec.deletion = DeletionPolicy::Delete;
        ready_or_rolling(&p, &id, &spec).await;
        let outcome = p.delete(&id).await.unwrap();
        assert!(outcome.existed);
        assert!(
            outcome.retained_volumes.is_empty(),
            "nothing stays: {:?}",
            outcome.retained_volumes
        );
        assert_eq!(p.status(&id).await.unwrap().phase, Phase::Absent);
    }

    pub async fn delete_is_idempotent<R: RuntimeUnderTest>(p: R) {
        let id = id();
        let outcome = p.delete(&id).await.unwrap();
        assert!(!outcome.existed, "nothing to delete is not an error");
        ready_or_rolling(&p, &id, &sample_spec(&id)).await;
        assert!(p.delete(&id).await.unwrap().existed);
        assert!(
            !p.delete(&id).await.unwrap().existed,
            "the second delete finds nothing"
        );
    }

    pub async fn endpoints_name_the_service<R: RuntimeUnderTest>(p: R) {
        let id = id();
        let missing = p.endpoint(&id, crate::Surface::A2a).await.unwrap_err();
        assert_eq!(missing.class(), ErrorClass::NotFound, "{missing}");
        ready_or_rolling(&p, &id, &sample_spec(&id)).await;
        let a2a = p.endpoint(&id, crate::Surface::A2a).await.unwrap();
        let card = p.endpoint(&id, crate::Surface::AgentCard).await.unwrap();
        assert!(
            a2a.url.starts_with("http://") || a2a.url.starts_with("https://"),
            "{}",
            a2a.url
        );
        assert!(card.url.starts_with("http"), "{}", card.url);
        assert!(
            card.url.ends_with("/.well-known/agent-card.json"),
            "{}",
            card.url
        );
        p.delete(&id).await.unwrap();
    }

    pub async fn watch_reports_a_change<R: RuntimeUnderTest>(p: R) {
        let id = id();
        let mut changes = p.watch();
        ready_or_rolling(&p, &id, &sample_spec(&id)).await;
        // A signal is never the truth, and a backend may also report other runtimes' changes:
        // what a controller needs is that its own change arrives at all, and in time.
        let seen = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(changed) = changes.next().await {
                if changed == id {
                    return true;
                }
            }
            false
        })
        .await;
        assert_eq!(
            seen,
            Ok(true),
            "the change of {id} was not reported by watch()"
        );
        p.delete(&id).await.unwrap();
    }

    pub async fn runtimes_are_independent<R: RuntimeUnderTest>(p: R) {
        let (a, b) = (id(), id());
        ready_or_rolling(&p, &a, &sample_spec(&a)).await;
        ready_or_rolling(&p, &b, &sample_spec(&b)).await;
        p.delete(&a).await.unwrap();
        assert_eq!(p.status(&a).await.unwrap().phase, Phase::Absent);
        assert_ne!(
            p.status(&b).await.unwrap().phase,
            Phase::Absent,
            "deleting a leaves b"
        );
        p.delete(&b).await.unwrap();
    }

    pub async fn no_secret_value_materialises<R: RuntimeUnderTest>(p: R) {
        let id = id();
        let spec = sample_spec(&id);
        let status = p.ensure(&id, &spec).await.unwrap();
        for text in p.materialised(&id).await {
            assert!(
                !text.contains(SENTINEL),
                "a secret reference reached plain text: {text}"
            );
        }
        for issue in &status.issues {
            assert!(
                !issue.message.contains(SENTINEL),
                "an issue message holds it: {}",
                issue.message
            );
        }
        let status = p.status(&id).await.unwrap();
        for issue in &status.issues {
            assert!(
                !issue.message.contains(SENTINEL),
                "an issue message holds it: {}",
                issue.message
            );
        }
        p.delete(&id).await.unwrap();
    }
}

/// Cases of the store suite.
#[allow(missing_docs)] // each case is named for what it asserts
pub mod store {
    use super::*;
    use crate::{CnpgSpec, StoreKind, StoreSpec, StoreState, cnpg_connection};

    fn id() -> StoreId {
        StoreId::new("adam-operator-test", unique("st"))
    }

    fn secret_spec(name: &str) -> StoreSpec {
        StoreSpec {
            owner: OwnerHandle::none(),
            deletion: DeletionPolicy::Retain,
            kind: StoreKind::Secret(SecretRef::new(format!("{SENTINEL}-{name}"), "uri")),
        }
    }

    fn cnpg_spec(deletion: DeletionPolicy) -> StoreSpec {
        StoreSpec {
            owner: OwnerHandle::none(),
            deletion,
            kind: StoreKind::Cnpg(CnpgSpec {
                instances: 1,
                size: "5Gi".to_owned(),
                storage_class: Some("fast".to_owned()),
            }),
        }
    }

    pub async fn a_referenced_secret_is_reported_as_such<S: StoreUnderTest>(p: S) {
        let id = id();
        let spec = secret_spec("db");
        let StoreKind::Secret(secret) = &spec.kind else {
            unreachable!()
        };
        let status = p.ensure(&id, &spec).await.unwrap();
        assert_eq!(status.state, StoreState::SecretReferenced);
        assert_eq!(
            &status.connection, secret,
            "the connection is the referenced key"
        );
        p.release(&id).await.unwrap();
    }

    pub async fn ensure_is_idempotent<S: StoreUnderTest>(p: S) {
        let id = id();
        let spec = secret_spec("db");
        let first = p.ensure(&id, &spec).await.unwrap();
        let second = p.ensure(&id, &spec).await.unwrap();
        assert_eq!(first, second);
        p.release(&id).await.unwrap();
    }

    pub async fn a_cluster_names_its_connection_secret<S: StoreUnderTest>(p: S) {
        let id = id();
        let spec = cnpg_spec(DeletionPolicy::Retain);
        if !p.capabilities().cnpg {
            let err = p.ensure(&id, &spec).await.unwrap_err();
            assert_eq!(err.class(), ErrorClass::Unsupported, "{err}");
            return;
        }
        let status = p.ensure(&id, &spec).await.unwrap();
        assert!(matches!(
            status.state,
            StoreState::ClusterReady | StoreState::ClusterNotReady
        ));
        assert_eq!(status.connection, cnpg_connection(id.name()));
        p.release(&id).await.unwrap();
    }

    pub async fn release_honours_the_deletion_policy<S: StoreUnderTest>(p: S) {
        // A Secret someone else owns is never ours to retain or delete.
        let id = id();
        p.ensure(&id, &secret_spec("db")).await.unwrap();
        let outcome = p.release(&id).await.unwrap();
        assert!(outcome.existed);
        assert!(
            !outcome.retained,
            "a referenced Secret holds no data of ours"
        );
        if !p.capabilities().cnpg {
            return;
        }
        let kept = super::store::id();
        p.ensure(&kept, &cnpg_spec(DeletionPolicy::Retain))
            .await
            .unwrap();
        let outcome = p.release(&kept).await.unwrap();
        assert!(
            outcome.existed && outcome.retained,
            "Retain keeps the cluster's data"
        );
        let gone = super::store::id();
        p.ensure(&gone, &cnpg_spec(DeletionPolicy::Delete))
            .await
            .unwrap();
        let outcome = p.release(&gone).await.unwrap();
        assert!(
            outcome.existed && !outcome.retained,
            "Delete takes the data too"
        );
    }

    pub async fn release_is_idempotent<S: StoreUnderTest>(p: S) {
        let outcome = p.release(&id()).await.unwrap();
        assert!(!outcome.existed);
        let id = id();
        p.ensure(&id, &secret_spec("db")).await.unwrap();
        assert!(p.release(&id).await.unwrap().existed);
        assert!(!p.release(&id).await.unwrap().existed);
    }

    pub async fn an_invalid_spec_is_refused<S: StoreUnderTest>(p: S) {
        let id = id();
        let spec = StoreSpec {
            owner: OwnerHandle::none(),
            deletion: DeletionPolicy::Retain,
            kind: StoreKind::Secret(SecretRef::new("", "uri")),
        };
        let err = p.ensure(&id, &spec).await.unwrap_err();
        assert_eq!(err.class(), ErrorClass::Invalid, "{err}");
        if p.capabilities().cnpg {
            let mut spec = cnpg_spec(DeletionPolicy::Retain);
            if let StoreKind::Cnpg(c) = &mut spec.kind {
                c.instances = 0;
            }
            let err = p.ensure(&id, &spec).await.unwrap_err();
            assert_eq!(err.class(), ErrorClass::Invalid, "{err}");
        }
    }

    pub async fn no_secret_value_materialises<S: StoreUnderTest>(p: S) {
        let id = id();
        p.ensure(&id, &secret_spec("db")).await.unwrap();
        for text in p.materialised(&id).await {
            assert!(
                !text.contains(SENTINEL),
                "a secret reference reached plain text: {text}"
            );
        }
        p.release(&id).await.unwrap();
    }
}

/// Cases of the directory suite.
#[allow(missing_docs)] // each case is named for what it asserts
pub mod directory {
    use super::*;

    fn entry(scope: &str, name: &str) -> DirectoryEntry {
        DirectoryEntry {
            scope: scope.to_owned(),
            name: name.to_owned(),
            title: Some(name.to_uppercase()),
            description: Some(format!("{name} does things")),
            tags: vec!["a".to_owned(), "b".to_owned()],
            agent_card: Some(format!(
                "http://{name}.{scope}.svc:8080/.well-known/agent-card.json"
            )),
            a2a_enabled: true,
            blocked: false,
        }
    }

    /// The entries of one scope, in the order the directory lists them.
    async fn in_scope<D: DirectoryUnderTest>(d: &D, scope: &str) -> Vec<DirectoryEntry> {
        d.list()
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.scope == scope)
            .collect()
    }

    pub async fn put_entries_are_listed_in_name_order<D: DirectoryUnderTest>(d: D) {
        let scope = unique("ns");
        for name in ["chat", "coder", "alpha"] {
            d.put(entry(&scope, name)).await;
        }
        let names: Vec<_> = in_scope(&d, &scope)
            .await
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(
            names,
            ["alpha", "chat", "coder"],
            "ordered by name, whatever the order of arrival"
        );
        // The same until something changes: an ETag is computed over it.
        assert_eq!(in_scope(&d, &scope).await, in_scope(&d, &scope).await);
    }

    pub async fn get_returns_what_was_put<D: DirectoryUnderTest>(d: D) {
        let scope = unique("ns");
        let e = entry(&scope, "coder");
        d.put(e.clone()).await;
        assert_eq!(d.get(&scope, "coder").await.unwrap(), Some(e));
        assert_eq!(d.get(&scope, "nobody").await.unwrap(), None);
        assert_eq!(d.get("no-such-scope", "coder").await.unwrap(), None);
    }

    pub async fn a_changed_entry_replaces_the_old_one<D: DirectoryUnderTest>(d: D) {
        let scope = unique("ns");
        d.put(entry(&scope, "coder")).await;
        let mut blocked = entry(&scope, "coder");
        blocked.blocked = true;
        blocked.agent_card = None;
        d.put(blocked.clone()).await;
        assert_eq!(in_scope(&d, &scope).await, vec![blocked.clone()]);
        assert!(!blocked.listed());
    }

    pub async fn a_removed_entry_is_gone<D: DirectoryUnderTest>(d: D) {
        let scope = unique("ns");
        d.put(entry(&scope, "coder")).await;
        d.put(entry(&scope, "chat")).await;
        d.remove(&scope, "coder").await;
        let names: Vec<_> = in_scope(&d, &scope)
            .await
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["chat"]);
        assert_eq!(d.get(&scope, "coder").await.unwrap(), None);
        d.remove(&scope, "coder").await; // removing twice is fine
    }

    pub async fn scopes_do_not_mix<D: DirectoryUnderTest>(d: D) {
        let (a, b) = (unique("ns"), unique("ns"));
        d.put(entry(&a, "coder")).await;
        d.put(entry(&b, "coder")).await;
        assert_eq!(in_scope(&d, &a).await.len(), 1);
        assert_eq!(in_scope(&d, &b).await.len(), 1);
        assert_ne!(
            d.get(&a, "coder").await.unwrap(),
            d.get(&b, "coder").await.unwrap()
        );
    }
}

/// Generate one `#[tokio::test]` per case of the runtime suite. `$make` is a path to an
/// `async fn() -> Option<R>` where `R: RuntimeUnderTest`; `None` skips the suite, unless
/// `ADAM_OPERATOR_TEST_REQUIRE_BACKEND=1`, which fails it instead.
#[macro_export]
macro_rules! runtime_provider_conformance {
    ($make:path) => {
        $crate::runtime_provider_conformance!(@cases $make;
            ensure_makes_the_runtime, status_of_an_unknown_runtime_is_absent,
            ensure_twice_is_one_runtime, ensure_accepts_a_changed_spec,
            ensure_rejects_a_malformed_spec, suspend_scales_to_zero_and_ensure_wakes,
            suspend_of_an_unknown_runtime_is_not_found, delete_retains_data_under_retain,
            delete_removes_data_under_delete, delete_is_idempotent, endpoints_name_the_service,
            watch_reports_a_change, runtimes_are_independent, no_secret_value_materialises,
        );
    };
    (@cases $make:path; $($case:ident),* $(,)?) => {
        $(
            #[tokio::test]
            async fn $case() {
                let Some(provider) = $make().await else {
                    $crate::testkit::skipped(&format!("{}: runtime provider not configured", stringify!($case)));
                    return;
                };
                $crate::testkit::runtime::$case(provider).await;
            }
        )*
    };
}

/// Generate one `#[tokio::test]` per case of the store suite. `$make` is a path to an
/// `async fn() -> Option<S>` where `S: StoreUnderTest`.
#[macro_export]
macro_rules! store_provisioner_conformance {
    ($make:path) => {
        $crate::store_provisioner_conformance!(@cases $make;
            a_referenced_secret_is_reported_as_such, ensure_is_idempotent,
            a_cluster_names_its_connection_secret, release_honours_the_deletion_policy,
            release_is_idempotent, an_invalid_spec_is_refused, no_secret_value_materialises,
        );
    };
    (@cases $make:path; $($case:ident),* $(,)?) => {
        $(
            #[tokio::test]
            async fn $case() {
                let Some(provisioner) = $make().await else {
                    $crate::testkit::skipped(&format!("{}: store provisioner not configured", stringify!($case)));
                    return;
                };
                $crate::testkit::store::$case(provisioner).await;
            }
        )*
    };
}

/// Generate one `#[tokio::test]` per case of the directory suite. `$make` is a path to an
/// `async fn() -> Option<D>` where `D: DirectoryUnderTest`.
#[macro_export]
macro_rules! agent_directory_conformance {
    ($make:path) => {
        $crate::agent_directory_conformance!(@cases $make;
            put_entries_are_listed_in_name_order, get_returns_what_was_put,
            a_changed_entry_replaces_the_old_one, a_removed_entry_is_gone, scopes_do_not_mix,
        );
    };
    (@cases $make:path; $($case:ident),* $(,)?) => {
        $(
            #[tokio::test]
            async fn $case() {
                let Some(directory) = $make().await else {
                    $crate::testkit::skipped(&format!("{}: agent directory not configured", stringify!($case)));
                    return;
                };
                $crate::testkit::directory::$case(directory).await;
            }
        )*
    };
}
