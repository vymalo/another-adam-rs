//! The pure core of the v0 operator: `validate`, `resolve`, and the env contract of
//! `adam-coder` and `adam-agent`.
//!
//! No async, no I/O, no Kubernetes client, and no `k8s-openapi` type in the public API: the inputs
//! are `adam-operator-api`'s objects and the outputs are `adam-operator-ports`' neutral types. The same input always
//! gives the same output, the same digest included.

#![warn(missing_docs)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod contract;
mod convert;
mod digest;
mod plan;
mod resolve;
mod syntax;
mod validate;

pub use digest::{canonical_json, digest_json, spec_digest};
pub use resolve::{ResolvedAgent, resolve};
pub use validate::{ConfigIssue, validate, validate_config};
