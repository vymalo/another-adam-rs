//! `StoreProvisioner` for an operator-owned CloudNativePG `Cluster`.
//!
//! A service whose `store.postgres.cnpg` is set gets a `postgresql.cnpg.io/v1` `Cluster` named
//! `<service>-db`, applied by **server-side apply** under the field manager `adam-operator`. The
//! connection string is the `uri` key of the Secret CloudNativePG makes for it, `<service>-db-app`
//! ([`adam_operator_ports::cnpg_connection`]): **this crate reports that reference and never reads the Secret**
//! (the operator has no right on Secrets). Readiness is read from the Cluster's own `status`.
//!
//! * **No CloudNativePG crate.** The Cluster is a [`DynamicObject`]: the provisioner needs six fields
//!   of it, and a generated type would tie the operator to one CloudNativePG release.
//! * **The API is looked up, not assumed.** Before it writes, `ensure` asks the API server for
//!   `postgresql.cnpg.io/v1`; an answer of `404`, or one without `clusters`, is
//!   [`StoreError::NotInstalled`] (the controller's `StoreReady: False`, reason `CNPGNotInstalled`).
//! * **The Cluster is data**, so it carries no owner reference (garbage
//!   collection must not take a database with a service). `release` follows the deletion policy that
//!   `ensure` remembered on the object: `Retain` keeps the Cluster (and strips any owner reference
//!   from it); `Delete` deletes it.
//! * **A referenced Secret is served too** (a provisioner passes the whole suite of
//!   `adam-operator-ports`), by [`adam_operator_store_secret::SecretStore`], so the composition root names one store type.
//!
//! No Kubernetes type is in a signature the ports define.

#![warn(missing_docs)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod cluster;
mod error;

use adam_operator_ports::{
    ReleaseOutcome, StoreCapabilities, StoreError, StoreId, StoreKind, StoreProvisioner, StoreSpec,
    StoreStatus, cnpg_connection,
};
use adam_operator_store_secret::SecretStore;
use kube::api::{DeleteParams, DynamicObject, Patch, PatchParams};
use kube::{Api, Client};
use serde_json::json;

use crate::cluster::{FIELD_MANAGER, cluster_name, is_ours};
use crate::error::map;

/// The provisioner of operator-owned CloudNativePG clusters (and of referenced Secrets).
///
/// Cheap to clone: every clone shares one client. It needs the right to `get`, `patch`, `create` and
/// `delete` `clusters` of `postgresql.cnpg.io` in the namespaces it serves. **It never touches a
/// Secret.**
pub struct CnpgStore {
    client: Client,
    secrets: SecretStore,
}

impl CnpgStore {
    /// A provisioner on this client.
    pub fn new(client: Client) -> Self {
        Self {
            client,
            secrets: SecretStore::new(),
        }
    }

    fn api(&self, namespace: &str) -> Api<DynamicObject> {
        Api::namespaced_with(self.client.clone(), namespace, &cluster::api_resource())
    }

    /// The Cluster `name`, or `None` when the API server says there is none. **A `404` of any kind is
    /// "none"**: an API server without CloudNativePG answers a request for a `Cluster` with a plain
    /// `404 page not found` that is no `Status` object (seen on 2026-10-05, kube-apiserver v1.35.8), which
    /// `Api::get_opt` does not take for "not found".
    async fn get(
        api: &Api<DynamicObject>,
        name: &str,
    ) -> Result<Option<DynamicObject>, StoreError> {
        match api.get(name).await {
            Ok(found) => Ok(Some(found)),
            Err(kube::Error::Api(status)) if status.code == 404 => Ok(None),
            Err(e) => Err(map(format!("Cluster {name}"), e)),
        }
    }

    /// Whether the API server serves `postgresql.cnpg.io/v1` `Cluster`.
    async fn installed(&self) -> Result<bool, StoreError> {
        match self
            .client
            .list_api_group_resources(cluster::API_VERSION)
            .await
        {
            Ok(list) => Ok(list.resources.iter().any(|r| r.name == cluster::PLURAL)),
            Err(kube::Error::Api(status)) if status.code == 404 => Ok(false),
            Err(e) => Err(map("the CloudNativePG API", e)),
        }
    }

    async fn ensure_cluster(
        &self,
        id: &StoreId,
        spec: &StoreSpec,
        cnpg: &adam_operator_ports::CnpgSpec,
    ) -> Result<StoreStatus, StoreError> {
        if !self.installed().await? {
            return Err(StoreError::NotInstalled {
                what: "CloudNativePG",
            });
        }
        let name = cluster_name(id.name());
        let api = self.api(id.scope());
        // The adoption guard, as the runtime provider's: a Cluster of this name that is not ours is
        // never written to.
        let existing = Self::get(&api, &name).await?;
        if existing.as_ref().is_some_and(|c| !is_ours(&c.metadata, id)) {
            return Err(StoreError::InvalidSpec(format!(
                "a Cluster {name} exists and is not managed by {FIELD_MANAGER} for this service; nothing was changed (rename the service, or remove that Cluster)"
            )));
        }
        let object = cluster::render(id, spec.deletion, cnpg);
        let applied = api
            .patch(
                &name,
                // The one writer of the fields it sets: a field another manager holds is taken back.
                &PatchParams::apply(FIELD_MANAGER).force(),
                &Patch::Apply(&object),
            )
            .await
            .map_err(|e| map(format!("Cluster {name}"), e))?;
        Ok(StoreStatus {
            state: cluster::state_of(&applied.data),
            connection: cnpg_connection(id.name()),
        })
    }

    async fn release_cluster(&self, id: &StoreId) -> Result<ReleaseOutcome, StoreError> {
        let name = cluster_name(id.name());
        let api = self.api(id.scope());
        // A missing API is a missing Cluster: there is nothing to release.
        let Some(found) = Self::get(&api, &name)
            .await?
            .filter(|c| is_ours(&c.metadata, id))
        else {
            return Ok(ReleaseOutcome::default());
        };
        match cluster::policy_of(&found.metadata) {
            adam_operator_ports::DeletionPolicy::Delete => {
                match api.delete(&name, &DeleteParams::background()).await {
                    Ok(_) => {}
                    Err(kube::Error::Api(status)) if status.code == 404 => {
                        return Ok(ReleaseOutcome::default());
                    }
                    Err(e) => return Err(map(format!("Cluster {name}"), e)),
                }
                Ok(ReleaseOutcome {
                    existed: true,
                    retained: false,
                })
            }
            adam_operator_ports::DeletionPolicy::Retain => {
                self.strip_owners(&api, &name, &found).await?;
                Ok(ReleaseOutcome {
                    existed: true,
                    retained: true,
                })
            }
        }
    }

    /// A kept Cluster keeps nothing that would let the garbage collector take it.
    async fn strip_owners(
        &self,
        api: &Api<DynamicObject>,
        name: &str,
        found: &DynamicObject,
    ) -> Result<(), StoreError> {
        if found
            .metadata
            .owner_references
            .as_ref()
            .is_none_or(Vec::is_empty)
        {
            return Ok(());
        }
        let params = PatchParams {
            field_manager: Some(FIELD_MANAGER.to_owned()),
            ..PatchParams::default()
        };
        api.patch(
            name,
            &params,
            &Patch::Merge(json!({"metadata": {"ownerReferences": null}})),
        )
        .await
        .map_err(|e| map(format!("Cluster {name}"), e))?;
        Ok(())
    }

    /// Every plain-text value of the Cluster applied for `id`, as the conformance suite reads them:
    /// names, labels, annotations and spec. The server's bookkeeping (`managedFields`) is left out.
    ///
    /// # Errors
    ///
    /// Any failure of the API server.
    pub async fn plain_text(&self, id: &StoreId) -> Result<Vec<String>, StoreError> {
        let name = cluster_name(id.name());
        let found = Self::get(&self.api(id.scope()), &name).await?;
        Ok(found
            .and_then(|c| serde_json::to_value(c).ok())
            .map(|mut doc| {
                if let Some(meta) = doc.get_mut("metadata").and_then(|m| m.as_object_mut()) {
                    meta.remove("managedFields");
                }
                let mut out = Vec::new();
                cluster::collect_strings(&doc, &mut out);
                out
            })
            .unwrap_or_default())
    }
}

impl StoreProvisioner for CnpgStore {
    fn capabilities(&self) -> StoreCapabilities {
        StoreCapabilities { cnpg: true }
    }

    async fn ensure(&self, id: &StoreId, spec: &StoreSpec) -> Result<StoreStatus, StoreError> {
        spec.check()?;
        match &spec.kind {
            StoreKind::Secret(_) => self.secrets.ensure(id, spec).await,
            StoreKind::Cnpg(cnpg) => self.ensure_cluster(id, spec, cnpg).await,
        }
    }

    async fn release(&self, id: &StoreId) -> Result<ReleaseOutcome, StoreError> {
        let secret = self.secrets.release(id).await?;
        let cluster = self.release_cluster(id).await?;
        Ok(ReleaseOutcome {
            existed: secret.existed || cluster.existed,
            retained: cluster.retained,
        })
    }
}

#[cfg(feature = "testkit")]
impl adam_operator_ports::testkit::StoreUnderTest for CnpgStore {
    #[allow(clippy::panic)]
    async fn materialised(&self, id: &StoreId) -> Vec<String> {
        self.plain_text(id)
            .await
            .unwrap_or_else(|e| panic!("reading the Cluster of {id}: {e}"))
    }
}
