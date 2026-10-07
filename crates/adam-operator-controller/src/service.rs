//! The `AgentService` reconciler. One pass:
//!
//! 1. the finalizer `agents.vymalo.com/runtime`, before anything is created (kube's `finalizer`
//!    helper: the first pass only adds it, and the patch brings the second);
//! 2. the `AgentConfig` the service names, and `adam_operator_domain::resolve` (which validates);
//! 3. the store, through [`StoreProvisioner`];
//! 4. [`RuntimeProvider::ensure`] with the resolved spec;
//! 5. the status (conditions and state, [`derive`]), patched by server-side apply;
//! 6. Events on transitions.
//!
//! The directory the registry reads is the reflector's cache: step 5 *is* the update of the
//! directory ([`ReflectorDirectory`](crate::ReflectorDirectory)).

use std::sync::Arc;

use adam_operator_api::{
    AgentConfig, AgentService, AgentServiceStatus, ConfigStatus, Endpoints, ServiceState,
};
use adam_operator_domain::{ResolvedAgent, resolve};
use adam_operator_ports::{
    Phase, RuntimeError, RuntimeId, RuntimeProvider, StoreError, StoreProvisioner, StoreState,
    Surface,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::api::{Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::runtime::events::{Event as KubeEvent, EventType};
use kube::runtime::finalizer::{Event, finalizer};
use kube::{Api, Resource, ResourceExt};
use serde_json::json;

use crate::context::{Context, FIELD_MANAGER, FINALIZER, failed};
use crate::derive::{ConfigOutcome, Observed, StoreOutcome, derive, ready_summary};
use crate::error::Error;
use crate::metrics::name;

const CONTROLLER: &str = "agentservice";

/// The reconciler of `AgentService`: what [`Operator`](crate::Operator) runs, public so a test (or
/// another composition) can drive one pass.
///
/// # Errors
///
/// A failed pass: the API server or a provider could not be reached, or an object is malformed. What a
/// pass *finds* (a missing config, an invalid one, a store that is not ready, a runtime that is not
/// well) is a status, not an error.
pub async fn reconcile_service<R, S>(
    svc: Arc<AgentService>,
    ctx: Arc<Context<R, S>>,
) -> Result<Action, Error>
where
    R: RuntimeProvider + 'static,
    S: StoreProvisioner + 'static,
{
    let ns = svc
        .namespace()
        .ok_or(Error::Malformed("an AgentService has no namespace"))?;
    let key = format!("{ns}/{}", svc.name_any());
    let api: Api<AgentService> = Api::namespaced(ctx.client.clone(), &ns);
    let result = finalizer(&api, FINALIZER, svc, |event| async {
        match event {
            Event::Apply(svc) => apply(&svc, &ctx).await,
            Event::Cleanup(svc) => cleanup(&svc, &ctx).await,
        }
    })
    .await
    .map_err(|e| Error::Finalizer(Box::new(e)));
    if result.is_ok() {
        ctx.retries.succeeded(&key);
        ctx.metrics.inc(
            name::RECONCILES,
            &[("controller", CONTROLLER), ("result", "ok")],
        );
    }
    result
}

/// The error policy of [`reconcile_service`]: back off by the class of the error.
pub fn service_error_policy<R, S>(
    svc: Arc<AgentService>,
    err: &Error,
    ctx: Arc<Context<R, S>>,
) -> Action {
    let key = format!("{}/{}", svc.namespace().unwrap_or_default(), svc.name_any());
    failed(CONTROLLER, &key, &ctx.retries, &ctx.metrics, err)
}

/// The outcome of looking for the config.
enum Resolution {
    Resolved {
        agent: Box<ResolvedAgent>,
        config: ConfigStatus,
    },
    Blocked(ConfigOutcome),
}

async fn fetch_and_resolve<R, S>(
    svc: &AgentService,
    ctx: &Context<R, S>,
) -> Result<Resolution, Error> {
    let ns = svc
        .namespace()
        .ok_or(Error::Malformed("an AgentService has no namespace"))?;
    let configs: Api<AgentConfig> = Api::namespaced(ctx.client.clone(), &ns);
    let wanted = &svc.spec.config_ref.name;
    let Some(config) = configs.get_opt(wanted).await? else {
        return Ok(Resolution::Blocked(ConfigOutcome::NotFound {
            name: wanted.clone(),
        }));
    };
    match resolve(svc, &config, (ctx.owner)(svc)) {
        Err(issues) => Ok(Resolution::Blocked(ConfigOutcome::Invalid {
            message: issues
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; "),
        })),
        Ok(agent) => Ok(Resolution::Resolved {
            config: ConfigStatus {
                name: config.name_any(),
                observed_generation: config.metadata.generation,
                digest: Some(agent.digest.clone()),
            },
            agent: Box::new(agent),
        }),
    }
}

async fn apply<R, S>(svc: &Arc<AgentService>, ctx: &Context<R, S>) -> Result<Action, Error>
where
    R: RuntimeProvider,
    S: StoreProvisioner,
{
    let ns = svc
        .namespace()
        .ok_or(Error::Malformed("an AgentService has no namespace"))?;
    let id = RuntimeId::new(&ns, svc.name_any());
    let previous = svc.status.clone().unwrap_or_default();
    let mut obs = Observed {
        generation: svc.metadata.generation,
        suspend: svc.spec.suspend,
        a2a_enabled: svc.spec.interfaces.a2a.enabled,
        registry: ctx.options.registry,
        registry_full: ctx
            .options
            .registry_full
            .load(std::sync::atomic::Ordering::Acquire),
        provider: ctx.runtime.name(),
        config: ConfigOutcome::Resolved,
        store: StoreOutcome::NotEvaluated,
        runtime: None,
        // What runs was resolved from these, and stays while the service is blocked.
        config_status: previous.config.clone(),
        endpoints: previous.endpoints.clone(),
    };

    // 2. The config.
    let (agent, config) = match fetch_and_resolve(svc, ctx).await? {
        Resolution::Blocked(outcome) => {
            obs.config = outcome;
            obs.runtime = Some(ctx.runtime.status(&id).await?);
            return finish(svc, ctx, &previous, obs).await;
        }
        Resolution::Resolved { agent, config } => (agent, config),
    };

    // 3. The store. A store that is not ready blocks the service: the agent would only crash-loop
    // with exit code 69, and what runs is left as it is.
    match ctx.store.ensure(&agent.store_id, &agent.store).await {
        Ok(status) => obs.store = StoreOutcome::Answered(status.state),
        Err(StoreError::NotInstalled { what }) => {
            obs.store = StoreOutcome::NotInstalled {
                message: format!("{what} is not installed in this cluster"),
            };
        }
        Err(StoreError::Unsupported(what)) => {
            obs.store = StoreOutcome::NotInstalled {
                message: format!("this operator cannot make {what}"),
            };
        }
        Err(StoreError::InvalidSpec(why)) => {
            obs.config = ConfigOutcome::Invalid {
                message: format!("the store provisioner refused the store: {why}"),
            };
        }
        Err(e) => return Err(e.into()),
    }
    let store_ready = matches!(
        obs.store,
        StoreOutcome::Answered(StoreState::SecretReferenced | StoreState::ClusterReady)
    );
    if !store_ready || obs.config != ConfigOutcome::Resolved {
        obs.runtime = Some(ctx.runtime.status(&id).await?);
        return finish(svc, ctx, &previous, obs).await;
    }

    // 4. The runtime.
    match ctx.runtime.ensure(&id, &agent.runtime).await {
        Ok(status) => {
            obs.config_status = Some(config);
            obs.runtime = Some(status);
        }
        // The provider refused a spec the domain accepted (Kubernetes says a StatefulSet's claim
        // templates cannot change, for one): nothing was applied, and the person has to change the
        // objects. The same as an invalid config.
        Err(e @ (RuntimeError::InvalidSpec(_) | RuntimeError::Unsupported(_))) => {
            obs.config = ConfigOutcome::Invalid {
                message: format!(
                    "the runtime provider {} refused the spec: {e}",
                    ctx.runtime.name()
                ),
            };
            obs.runtime = Some(ctx.runtime.status(&id).await?);
        }
        Err(e) => return Err(e.into()),
    }

    // Where it is reached, once there is something to reach (and nothing blocks it).
    if let Some(rt) = &obs.runtime
        && rt.phase != Phase::Absent
        && !rt.is_name_conflict()
    {
        obs.endpoints = Some(endpoints(ctx, &id).await?);
    }
    finish(svc, ctx, &previous, obs).await
}

async fn endpoints<R: RuntimeProvider, S>(
    ctx: &Context<R, S>,
    id: &RuntimeId,
) -> Result<Endpoints, Error> {
    let one = |surface| async move {
        match ctx.runtime.endpoint(id, surface).await {
            Ok(e) => Ok(Some(e.url)),
            Err(RuntimeError::NotFound(_)) => Ok(None),
            Err(e) => Err(Error::from(e)),
        }
    };
    Ok(Endpoints {
        a2a: one(Surface::A2a).await?,
        agent_card: one(Surface::AgentCard).await?,
    })
}

/// Write the status when it changed, say so when the state did, and decide when to look again.
async fn finish<R, S>(
    svc: &AgentService,
    ctx: &Context<R, S>,
    previous: &AgentServiceStatus,
    obs: Observed,
) -> Result<Action, Error> {
    let now = (ctx.clock)();
    let status = derive(&obs, &previous.conditions, &now);
    let name = svc.name_any();

    if svc.status.as_ref() != Some(&status) {
        let api: Api<AgentService> = Api::namespaced(
            ctx.client.clone(),
            &svc.namespace().ok_or(Error::Malformed("no namespace"))?,
        );
        let body = json!({
            "apiVersion": AgentService::api_version(&()),
            "kind": AgentService::kind(&()),
            "metadata": { "name": name },
            "status": status,
        });
        api.patch_status(
            &name,
            &PatchParams::apply(FIELD_MANAGER).force(),
            &Patch::Apply(&body),
        )
        .await?;
        ctx.metrics
            .inc(name::STATUS_PATCHES, &[("controller", CONTROLLER)]);
    }

    announce(svc, ctx, previous, &status).await;
    Ok(Action::requeue(period(ctx, &status, &obs.store)))
}

/// An Event when the state, or the reason the service is or is not ready, changed.
async fn announce<R, S>(
    svc: &AgentService,
    ctx: &Context<R, S>,
    previous: &AgentServiceStatus,
    status: &AgentServiceStatus,
) {
    let (Some((reason, message)), Some(state)) = (ready_summary(status), status.state) else {
        return;
    };
    let before = ready_summary(previous).map(|(r, _)| r);
    if previous.state == status.state && before == Some(reason) {
        return;
    }
    if previous.state != status.state {
        ctx.metrics
            .inc(name::STATE_CHANGES, &[("state", &format!("{state:?}"))]);
    }
    let calm = matches!(reason, "Reconciled" | "Provisioning" | "Suspended");
    let event = KubeEvent {
        type_: if calm && state != ServiceState::Blocked {
            EventType::Normal
        } else {
            EventType::Warning
        },
        reason: reason.to_owned(),
        note: Some(message.chars().take(1000).collect()),
        action: "Reconcile".to_owned(),
        secondary: None,
    };
    // An Event is a courtesy: the status says the same, and a failure to write one fails nothing.
    if let Err(e) = ctx.recorder.publish(&event, &svc.object_ref(&())).await {
        tracing::warn!(service = %svc.name_any(), "could not publish an Event: {e}");
    }
}

/// When to look again: the timer is what makes a lost signal harmless.
fn period<R, S>(
    ctx: &Context<R, S>,
    status: &AgentServiceStatus,
    store: &StoreOutcome,
) -> std::time::Duration {
    let r = &ctx.options.resync;
    match status.state {
        Some(ServiceState::Ready | ServiceState::Suspended) => r.settled,
        Some(ServiceState::Blocked) => {
            if matches!(store, StoreOutcome::NotInstalled { .. }) {
                r.not_installed
            } else if config_is_the_problem(&status.conditions) {
                // Only a change of the objects helps, and the watches bring it. The timer is the net.
                r.settled
            } else {
                r.pending
            }
        }
        Some(ServiceState::Degraded) | None => r.pending,
    }
}

fn config_is_the_problem(conditions: &[Condition]) -> bool {
    conditions.iter().any(|c| {
        c.type_ == adam_operator_api::condition_type::CONFIG_RESOLVED && c.status == "False"
    })
}

/// The finalizer ran: remove the compute, release the store, and let the object go.
async fn cleanup<R, S>(svc: &Arc<AgentService>, ctx: &Context<R, S>) -> Result<Action, Error>
where
    R: RuntimeProvider,
    S: StoreProvisioner,
{
    let ns = svc
        .namespace()
        .ok_or(Error::Malformed("an AgentService has no namespace"))?;
    let id = RuntimeId::new(&ns, svc.name_any());
    let store_id = adam_operator_ports::StoreId::new(&ns, svc.name_any());

    // The policy a provider honours is the one of its last `ensure`. A person who changes it and
    // deletes in one breath has not had a pass in between, and Delete -> Retain must not lose data:
    // so when something exists and the objects still resolve, apply them once more. Anything that
    // does not resolve, or that a provider refuses, is left: the remembered policy stands.
    if ctx.runtime.status(&id).await?.phase != Phase::Absent
        && let Resolution::Resolved { agent, .. } = fetch_and_resolve(svc, ctx).await?
    {
        match ctx.runtime.ensure(&id, &agent.runtime).await {
            Ok(_) | Err(RuntimeError::InvalidSpec(_) | RuntimeError::Unsupported(_)) => {}
            Err(e) => return Err(e.into()),
        }
        match ctx.store.ensure(&agent.store_id, &agent.store).await {
            Ok(_)
            | Err(
                StoreError::InvalidSpec(_)
                | StoreError::Unsupported(_)
                | StoreError::NotInstalled { .. },
            ) => {}
            Err(e) => return Err(e.into()),
        }
    }

    let deleted = ctx.runtime.delete(&id).await?;
    let released = ctx.store.release(&store_id).await?;
    ctx.metrics.inc(name::SERVICES_DELETED, &[]);

    let mut note = if deleted.existed {
        "removed the workloads, the Service and the other compute".to_owned()
    } else {
        "there was no compute to remove".to_owned()
    };
    if !deleted.retained_volumes.is_empty() {
        note.push_str(&format!(
            "; kept the volumes {} (deletionPolicy: Retain)",
            deleted.retained_volumes.join(", ")
        ));
    }
    if released.retained {
        note.push_str("; kept the database (deletionPolicy: Retain)");
    }
    let event = KubeEvent {
        type_: EventType::Normal,
        reason: "Deleted".to_owned(),
        note: Some(note),
        action: "Finalize".to_owned(),
        secondary: None,
    };
    if let Err(e) = ctx.recorder.publish(&event, &svc.object_ref(&())).await {
        tracing::warn!(service = %svc.name_any(), "could not publish an Event: {e}");
    }
    Ok(Action::await_change())
}
