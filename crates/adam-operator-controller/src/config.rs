//! The `AgentConfig` reconciler: validate the object and say so in its `Valid` condition. It has no finalizer and makes nothing: the services that name the config are
//! reconciled when it changes ([`Operator`](crate::Operator) maps the watch).

use std::sync::Arc;

use adam_operator_api::{AgentConfig, AgentConfigStatus, condition_type, reason};
use adam_operator_domain::validate_config;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::api::{Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::{Api, Resource, ResourceExt};
use serde_json::json;

use crate::context::{ConfigContext, FIELD_MANAGER, failed};
use crate::error::Error;
use crate::metrics::name;

const CONTROLLER: &str = "agentconfig";

/// The status of a config: what [`validate_config`] said, as a `Valid` condition.
pub(crate) fn config_status(
    config: &AgentConfig,
    previous: &[Condition],
    now: &k8s_openapi::apimachinery::pkg::apis::meta::v1::Time,
) -> AgentConfigStatus {
    let (status, why, message) = match validate_config(config) {
        Ok(()) => (
            "True",
            reason::VALID,
            "the config passed validation".to_owned(),
        ),
        Err(issues) => (
            "False",
            reason::CONFIG_INVALID,
            issues
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; "),
        ),
    };
    let since = previous
        .iter()
        .find(|c| c.type_ == condition_type::VALID && c.status == status)
        .map_or_else(|| now.clone(), |c| c.last_transition_time.clone());
    AgentConfigStatus {
        observed_generation: config.metadata.generation,
        conditions: vec![Condition {
            type_: condition_type::VALID.to_owned(),
            status: status.to_owned(),
            reason: why.to_owned(),
            message,
            observed_generation: config.metadata.generation,
            last_transition_time: since,
        }],
    }
}

/// One pass over an `AgentConfig`.
///
/// # Errors
///
/// The status could not be written.
pub async fn reconcile_config(
    config: Arc<AgentConfig>,
    ctx: Arc<ConfigContext>,
) -> Result<Action, Error> {
    let ns = config
        .namespace()
        .ok_or(Error::Malformed("an AgentConfig has no namespace"))?;
    let name_of = config.name_any();
    let previous = config.status.clone().unwrap_or_default();
    let status = config_status(&config, &previous.conditions, &(ctx.clock)());

    if config.status.as_ref() != Some(&status) {
        let api: Api<AgentConfig> = Api::namespaced(ctx.client.clone(), &ns);
        let body = json!({
            "apiVersion": AgentConfig::api_version(&()),
            "kind": AgentConfig::kind(&()),
            "metadata": { "name": name_of },
            "status": status,
        });
        api.patch_status(
            &name_of,
            &PatchParams::apply(FIELD_MANAGER).force(),
            &Patch::Apply(&body),
        )
        .await?;
        ctx.metrics
            .inc(name::STATUS_PATCHES, &[("controller", CONTROLLER)]);
    }
    ctx.retries.succeeded(&format!("{ns}/{name_of}"));
    ctx.metrics.inc(
        name::RECONCILES,
        &[("controller", CONTROLLER), ("result", "ok")],
    );
    Ok(Action::await_change())
}

/// The error policy of [`reconcile_config`].
pub fn config_error_policy(
    config: Arc<AgentConfig>,
    err: &Error,
    ctx: Arc<ConfigContext>,
) -> Action {
    let key = format!(
        "{}/{}",
        config.namespace().unwrap_or_default(),
        config.name_any()
    );
    failed(CONTROLLER, &key, &ctx.retries, &ctx.metrics, err)
}
