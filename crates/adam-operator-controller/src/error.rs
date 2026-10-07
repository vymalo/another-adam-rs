//! The controller's errors. Each has a class (`adam_error::Classify`): the error policy decides from
//! the class, never from the variant.

use adam_error::{Classify, ErrorClass};
use adam_operator_ports::{RuntimeError, StoreError};
use kube::runtime::finalizer;

/// Why a pass failed, as opposed to what it found: a service that is blocked, invalid or unwell is
/// a *status*, and the pass that wrote it succeeded.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The Kubernetes API refused or could not be reached.
    #[error("the Kubernetes API: {0}")]
    Kube(#[source] kube::Error),
    /// A `RuntimeProvider` call failed.
    #[error("the runtime provider: {0}")]
    Runtime(#[from] RuntimeError),
    /// A `StoreProvisioner` call failed.
    #[error("the store provisioner: {0}")]
    Store(#[from] StoreError),
    /// An object the API server sent lacks what every object has.
    #[error("malformed object: {0}")]
    Malformed(&'static str),
    /// Adding or removing the finalizer failed, or the work it wraps did.
    #[error("finalizer: {0}")]
    Finalizer(#[source] Box<finalizer::Error<Error>>),
}

impl From<kube::Error> for Error {
    fn from(e: kube::Error) -> Self {
        Self::Kube(e)
    }
}

fn kube_class(e: &kube::Error) -> ErrorClass {
    match e {
        kube::Error::Api(status) => match status.code {
            404 => ErrorClass::NotFound,
            409 => ErrorClass::Conflict,
            400 | 422 => ErrorClass::Invalid,
            // The operator's own RBAC is wrong, or a webhook is down: the deployment fixes it, and
            // it may be fixed while we wait.
            401 | 403 => ErrorClass::Internal,
            429 | 500..=599 => ErrorClass::Transient,
            _ => ErrorClass::Internal,
        },
        // Connection refused, a timeout, a body that did not parse: the API server is the next
        // thing that changes, so look again.
        _ => ErrorClass::Transient,
    }
}

impl Classify for Error {
    fn class(&self) -> ErrorClass {
        match self {
            Self::Kube(e) => kube_class(e),
            Self::Runtime(e) => e.class(),
            Self::Store(e) => e.class(),
            Self::Malformed(_) => ErrorClass::Internal,
            Self::Finalizer(f) => match &**f {
                finalizer::Error::ApplyFailed(e) | finalizer::Error::CleanupFailed(e) => e.class(),
                // The finalizer is a JSON patch that first `test`s what it saw, so that it never overwrites
                // someone else's. A reconcile that ran on a cached object an earlier pass had already
                // changed fails that test, and the API server says 422 (409 for a conflict): a lost race,
                // not bad input. The change that beat us is on its way as a watch event.
                finalizer::Error::AddFinalizer(e) | finalizer::Error::RemoveFinalizer(e) => {
                    match kube_class(e) {
                        ErrorClass::Invalid => ErrorClass::Conflict,
                        other => other,
                    }
                }
                finalizer::Error::UnnamedObject | finalizer::Error::InvalidFinalizer => {
                    ErrorClass::Internal
                }
            },
        }
    }
}

/// The label of a class in a metric.
pub(crate) fn class_label(class: ErrorClass) -> &'static str {
    match class {
        ErrorClass::Transient => "transient",
        ErrorClass::RateLimited => "rate_limited",
        ErrorClass::Conflict => "conflict",
        ErrorClass::Invalid => "invalid",
        ErrorClass::NotFound => "not_found",
        ErrorClass::Rejected => "rejected",
        ErrorClass::Unauthenticated => "unauthenticated",
        ErrorClass::Unsupported => "unsupported",
        ErrorClass::Corrupt => "corrupt",
        ErrorClass::Internal => "internal",
        // `ErrorClass` is non-exhaustive.
        _ => "other",
    }
}
