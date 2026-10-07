//! `StoreProvisioner` for a referenced Secret.
//!
//! A service whose `store.postgres.secretRef` names a Secret key needs nothing made: someone else
//! owns the Secret and the database behind it. This provisioner checks the reference is well formed
//! and reports it, so the controller can say `StoreReady: SecretReferenced` and `adam-operator-domain` can
//! hand the same reference to the pod as `DATABASE_URL`.
//!
//! **It never reads a Secret, and it has no client of any kind.** The operator has no right on
//! Secrets (a `get` returns the values, and a `list` of keys does not exist), so whether the
//! Secret and its key exist is the kubelet's to say: a pod that references a missing one stays in
//! `CreateContainerConfigError`, which the runtime provider reports as the issue `MissingSecret` and
//! the controller as `RuntimeReady: False`, reason `MissingSecret`. That is why [`StoreState`] has
//! `SecretReferenced` and not a `SecretFound`.
//!
//! A cluster kind is refused as [`StoreError::Unsupported`]: it is `adam-operator-store-cnpg`'s.

#![warn(missing_docs)]

use std::collections::BTreeSet;
use std::sync::{Mutex, PoisonError};

use adam_operator_ports::{
    ReleaseOutcome, StoreCapabilities, StoreError, StoreId, StoreKind, StoreProvisioner, StoreSpec,
    StoreState, StoreStatus,
};

/// The provisioner of referenced Secrets. It keeps the ids it was asked about (so that releasing
/// what it never saw says `existed: false`, as the contract wants) and nothing else.
#[derive(Debug, Default)]
pub struct SecretStore {
    known: Mutex<BTreeSet<StoreId>>,
}

impl SecretStore {
    /// A provisioner that has seen nothing yet.
    pub fn new() -> Self {
        Self::default()
    }

    fn known(&self) -> std::sync::MutexGuard<'_, BTreeSet<StoreId>> {
        // A poisoned set is still a set of ids.
        self.known.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl StoreProvisioner for SecretStore {
    fn capabilities(&self) -> StoreCapabilities {
        StoreCapabilities { cnpg: false }
    }

    async fn ensure(&self, id: &StoreId, spec: &StoreSpec) -> Result<StoreStatus, StoreError> {
        spec.check()?;
        match &spec.kind {
            StoreKind::Secret(reference) => {
                self.known().insert(id.clone());
                Ok(StoreStatus {
                    state: StoreState::SecretReferenced,
                    connection: reference.clone(),
                })
            }
            StoreKind::Cnpg(_) => Err(StoreError::Unsupported(
                "an operator-owned CloudNativePG cluster (store-cnpg is not built into this operator)",
            )),
        }
    }

    async fn release(&self, id: &StoreId) -> Result<ReleaseOutcome, StoreError> {
        // A Secret someone else owns holds no data of ours: nothing is retained and nothing deleted,
        // whatever the deletion policy says.
        let existed = self.known().remove(id);
        Ok(ReleaseOutcome {
            existed,
            retained: false,
        })
    }
}

#[cfg(feature = "testkit")]
impl adam_operator_ports::testkit::StoreUnderTest for SecretStore {
    async fn materialised(&self, _id: &StoreId) -> Vec<String> {
        // It writes nothing anywhere.
        Vec::new()
    }
}
