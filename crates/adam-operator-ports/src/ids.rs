//! Identities and references.

use std::fmt;

use serde::{Deserialize, Serialize};

macro_rules! scoped_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub struct $name {
            scope: String,
            name: String,
        }

        impl $name {
            /// An id from its scope and name.
            pub fn new(scope: impl Into<String>, name: impl Into<String>) -> Self {
                Self { scope: scope.into(), name: name.into() }
            }

            /// The scope the name lives in (a Kubernetes namespace).
            pub fn scope(&self) -> &str {
                &self.scope
            }

            /// The name of the service this belongs to.
            pub fn name(&self) -> &str {
                &self.name
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}/{}", self.scope, self.name)
            }
        }
    };
}

scoped_id! {
    /// The identity of a runtime: the scope and the name of the service it runs. The same on every
    /// `ensure`, so a call is idempotent. The scope is what a Kubernetes provider reads as a namespace.
    RuntimeId
}

scoped_id! {
    /// The identity of a store: the scope and the name of the service whose ledger it is.
    StoreId
}

/// A reference to one key of a Secret: **the only form in which a secret travels**. No
/// type of this crate has a field for a secret's value, so no provider can be handed one.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SecretRef {
    /// Name of the Secret.
    pub name: String,
    /// Key inside it.
    pub key: String,
}

impl SecretRef {
    /// A reference to `key` of the Secret `name`.
    pub fn new(name: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            key: key.into(),
        }
    }
}

/// An opaque token that says whose a runtime is, so a provider can make the garbage collector
/// remove its compute with the service (on Kubernetes: owner references).
///
/// The controller got it from the object it reconciles and never looks inside; the provider that
/// understands the token is the one that made it. A provider that has no use for ownership ignores
/// it. It is **not** part of the digest of a spec (it names an object, not a configuration).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerHandle(String);

impl OwnerHandle {
    /// A handle from the provider's own encoding.
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    /// No owner (tests, and providers without ownership).
    pub fn none() -> Self {
        Self::default()
    }

    /// The provider's encoding. Only the provider that made the handle can read it.
    pub fn token(&self) -> &str {
        &self.0
    }
}
