//! The v0 CRD types of the adam-rs operator: `AgentService` and
//! `AgentConfig`, group `agents.vymalo.com`, version `v1alpha1`, namespaced.
//!
//! This crate is types only. The CEL rules (`x-kubernetes-validations`) are part of the generated
//! schema; [`crds()`] returns both definitions, and `adam-operator crdgen` prints them.

mod common;
mod config;
mod service;
mod status;

use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::core::CustomResourceExt;

pub use common::{Empty, NameRef, SecretKeyRef};
pub use config::*;
pub use service::*;
pub use status::*;

/// The API group of both kinds.
pub const GROUP: &str = "agents.vymalo.com";

/// The version of both kinds.
pub const VERSION: &str = "v1alpha1";

/// The CustomResourceDefinitions of both kinds, in a fixed order: `AgentConfig`, `AgentService`.
pub fn crds() -> Vec<CustomResourceDefinition> {
    vec![AgentConfig::crd(), AgentService::crd()]
}
