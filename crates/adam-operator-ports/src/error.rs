//! Errors carry a class: a caller decides what to do from [`Classify::class`], never from the
//! variant, so a new variant needs a class and nothing else.

pub use adam_error::{BoxError, Classify, ErrorClass};

use crate::RuntimeId;

/// Why a [`RuntimeProvider`](crate::RuntimeProvider) call failed.
///
/// A runtime that is *blocked* (a name taken by an object that is not ours, a missing Secret, a
/// crash loop) is **not** an error: it is a [`RuntimeStatus`](crate::RuntimeStatus) with issues.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// The [`RuntimeSpec`](crate::RuntimeSpec) breaks an invariant every provider relies on
    /// ([`RuntimeSpec::check`](crate::RuntimeSpec::check)).
    #[error("invalid runtime spec: {0}")]
    InvalidSpec(String),
    /// No runtime has this id.
    #[error("runtime {0} not found")]
    NotFound(RuntimeId),
    /// The provider does not offer this ([`Capabilities`](crate::Capabilities)).
    #[error("this runtime provider does not support {0}")]
    Unsupported(&'static str),
    /// The backend could not be reached or refused for a reason that may pass.
    #[error("the runtime backend is unavailable")]
    Unavailable(#[source] BoxError),
    /// Another writer changed the runtime meanwhile.
    #[error("runtime conflict: {0}")]
    Conflict(String),
    /// A bug.
    #[error("internal runtime error")]
    Internal(#[source] BoxError),
}

impl Classify for RuntimeError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::InvalidSpec(_) => ErrorClass::Invalid,
            Self::NotFound(_) => ErrorClass::NotFound,
            Self::Unsupported(_) => ErrorClass::Unsupported,
            Self::Unavailable(_) => ErrorClass::Transient,
            Self::Conflict(_) => ErrorClass::Conflict,
            Self::Internal(_) => ErrorClass::Internal,
        }
    }
}

/// Why a [`StoreProvisioner`](crate::StoreProvisioner) call failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The [`StoreSpec`](crate::StoreSpec) is wrong.
    #[error("invalid store spec: {0}")]
    InvalidSpec(String),
    /// The backend lacks what this kind of store needs, e.g. the CloudNativePG API
    /// (`StoreReady` is `False` with the reason `CNPGNotInstalled`).
    #[error("{what} is not installed")]
    NotInstalled {
        /// What is missing, e.g. `CloudNativePG`.
        what: &'static str,
    },
    /// The provisioner does not offer this kind of store.
    #[error("this store provisioner does not support {0}")]
    Unsupported(&'static str),
    /// The backend could not be reached or refused for a reason that may pass.
    #[error("the store backend is unavailable")]
    Unavailable(#[source] BoxError),
    /// Another writer changed the store meanwhile.
    #[error("store conflict: {0}")]
    Conflict(String),
    /// A bug.
    #[error("internal store error")]
    Internal(#[source] BoxError),
}

impl Classify for StoreError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::InvalidSpec(_) => ErrorClass::Invalid,
            Self::NotInstalled { .. } | Self::Unsupported(_) => ErrorClass::Unsupported,
            Self::Unavailable(_) => ErrorClass::Transient,
            Self::Conflict(_) => ErrorClass::Conflict,
            Self::Internal(_) => ErrorClass::Internal,
        }
    }
}

/// Why an [`AgentDirectory`](crate::AgentDirectory) read failed.
#[derive(Debug, thiserror::Error)]
pub enum DirectoryError {
    /// The directory has not seen the cluster yet (a reflector that has not synced), or its source
    /// is unreachable. A client asking the registry gets a `503`, never an empty list.
    #[error("the agent directory is not ready")]
    NotReady,
    /// The source could not be read.
    #[error("the agent directory is unavailable")]
    Unavailable(#[source] BoxError),
}

impl Classify for DirectoryError {
    fn class(&self) -> ErrorClass {
        ErrorClass::Transient
    }
}
