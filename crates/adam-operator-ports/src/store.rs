//! `StoreProvisioner`: where an agent's durable run ledger lives.

use std::future::Future;

use serde::{Deserialize, Serialize};

use crate::{DeletionPolicy, OwnerHandle, SecretRef, StoreError, StoreId};

/// The key of the connection string in the Secret CloudNativePG makes for a cluster.
pub const CNPG_URI_KEY: &str = "uri";

/// The Secret that holds the connection string of the operator-owned cluster of a service:
/// `<service>-db-app`, key `uri` (CloudNativePG names the Secret of a cluster `<cluster>-app`, and
/// the cluster is `<service>-db`). One definition, used by `adam-operator-domain` for `DATABASE_URL` and by
/// every provisioner for its status.
pub fn cnpg_connection(service: &str) -> SecretRef {
    SecretRef::new(format!("{service}-db-app"), CNPG_URI_KEY)
}

/// What the controller asks of a provisioner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreSpec {
    /// Whose store it is.
    pub owner: OwnerHandle,
    /// What a release does to data.
    pub deletion: DeletionPolicy,
    /// Which kind.
    pub kind: StoreKind,
}

impl StoreSpec {
    /// The invariants every provisioner relies on.
    ///
    /// # Errors
    ///
    /// [`StoreError::InvalidSpec`].
    pub fn check(&self) -> Result<(), StoreError> {
        match &self.kind {
            StoreKind::Secret(s) if s.name.is_empty() || s.key.is_empty() => Err(
                StoreError::InvalidSpec("the Secret reference needs a name and a key".into()),
            ),
            StoreKind::Cnpg(c) if c.instances == 0 => Err(StoreError::InvalidSpec(
                "a cluster needs at least one instance".into(),
            )),
            StoreKind::Cnpg(c) if c.size.is_empty() => Err(StoreError::InvalidSpec(
                "a cluster needs a storage size".into(),
            )),
            _ => Ok(()),
        }
    }
}

/// Which kind of store. Closed on purpose: a new kind must fail to compile in every provisioner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StoreKind {
    /// A Secret someone else owns holds the connection string. Nothing is made.
    Secret(SecretRef),
    /// An operator-owned CloudNativePG cluster `<service>-db`.
    Cnpg(CnpgSpec),
}

/// An operator-owned CloudNativePG cluster.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CnpgSpec {
    /// Instances of the cluster.
    pub instances: u32,
    /// Size of each instance's volume, a quantity string (`5Gi`).
    pub size: String,
    /// Storage class. `None`: the cluster's default.
    pub storage_class: Option<String>,
}

/// How a store is doing: the reasons of the `StoreReady` condition that are not errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StoreState {
    /// A Secret is referenced (`SecretReferenced`). The provisioner cannot read Secrets and does
    /// not check that it exists: the kubelet's answer in the pod's status is how the controller
    /// learns.
    SecretReferenced,
    /// The cluster is ready (`ClusterReady`).
    ClusterReady,
    /// The cluster exists and is not ready yet (`ClusterNotReady`).
    ClusterNotReady,
}

/// What a provisioner says of a store.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreStatus {
    /// How it is doing.
    pub state: StoreState,
    /// The Secret key that holds the connection string (`DATABASE_URL`).
    pub connection: SecretRef,
}

/// What a provisioner can do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreCapabilities {
    /// [`StoreKind::Cnpg`] is served. Without it `ensure` of that kind fails with
    /// [`StoreError::Unsupported`]; a provisioner whose backend lacks the CloudNativePG API at run
    /// time fails with [`StoreError::NotInstalled`] instead.
    pub cnpg: bool,
}

/// What a release left behind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseOutcome {
    /// There was something to release.
    pub existed: bool,
    /// Data stays, because the deletion policy is `Retain`.
    pub retained: bool,
}

/// Makes the store of an agent exist.
///
/// The same conventions as [`RuntimeProvider`](crate::RuntimeProvider): native `async fn`s with a
/// `Send` bound, idempotent, the id is the identity, and the deletion policy of the last `ensure`
/// is what `release` honours.
pub trait StoreProvisioner: Send + Sync {
    /// What this provisioner can do.
    fn capabilities(&self) -> StoreCapabilities;

    /// Make the store match `spec` and say how it is doing.
    ///
    /// # Errors
    ///
    /// `InvalidSpec` when [`StoreSpec::check`] fails, `Unsupported` for a kind the provisioner does
    /// not serve, `NotInstalled` when the backend lacks the API the kind needs.
    fn ensure(
        &self,
        id: &StoreId,
        spec: &StoreSpec,
    ) -> impl Future<Output = Result<StoreStatus, StoreError>> + Send;

    /// Let go of the store, deleting its data only when the deletion policy is `Delete`. Releasing
    /// what is not there is a success with `existed: false`.
    ///
    /// # Errors
    ///
    /// `Unavailable` when the backend cannot be reached.
    fn release(
        &self,
        id: &StoreId,
    ) -> impl Future<Output = Result<ReleaseOutcome, StoreError>> + Send;
}
