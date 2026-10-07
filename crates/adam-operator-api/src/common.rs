//! Types shared by both kinds.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A reference to one key of a Secret in the same namespace.
///
/// A secret is never a value in a custom resource: a field that needs one names a Secret
/// and a key, and the operator copies the reference into the pod spec as a `secretKeyRef`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SecretKeyRef {
    /// Name of the Secret.
    #[schemars(length(min = 1))]
    pub name: String,
    /// Key inside the Secret.
    #[schemars(length(min = 1))]
    pub key: String,
}

/// A reference to an object of the same namespace, by name.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NameRef {
    /// Name of the referenced object.
    #[schemars(length(min = 1))]
    pub name: String,
}

/// An empty object, `{}`: a choice that carries no settings (`agent.embedded`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Empty {}
