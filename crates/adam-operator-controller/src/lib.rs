//! The reconcilers of the v0 operator: kube-rs controllers for
//! `AgentService` and `AgentConfig`, generic over [`adam_operator_ports::RuntimeProvider`] and
//! [`adam_operator_ports::StoreProvisioner`].
//!
//! * [`Operator`] wires the two controllers to their triggers and runs them.
//! * [`reconcile_service`] and [`reconcile_config`] are one pass each, public so that a test can drive
//!   them against the `Memory` providers and a fake API server.
//! * [`derive`](mod@derive) is the pure part: what a pass saw, as conditions and a state.
//! * [`ReflectorDirectory`] is the [`adam_operator_ports::AgentDirectory`] the registry reads.
//!
//! No Kubernetes type is in a port's signature: `kube` appears here, in the controller, which is
//! its job. The provider-specific owner reference travels as an opaque handle the composition root
//! knows how to make ([`OwnerOf`]).

#![warn(missing_docs)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod config;
mod context;
pub mod derive;
mod directory;
mod error;
mod metrics;
mod operator;
mod options;
mod service;

pub use config::{config_error_policy, reconcile_config};
pub use context::{
    Clock, ConfigContext, Context, FIELD_MANAGER, FINALIZER, OwnerOf, backoff, system_clock,
};
pub use directory::ReflectorDirectory;
pub use error::Error;
pub use metrics::{Metrics, name as metric};
pub use operator::{Operator, Readiness};
pub use options::{Options, RegistryMode, Resync};
pub use service::{reconcile_service, service_error_policy};
