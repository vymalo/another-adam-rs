//! The errors of this provider. Each has a class (`adam_error::Classify`): a caller decides from the
//! class, never from the variant. A runtime that is unwell (a name taken, a missing Secret, a crash
//! loop) is **not** an error: it is an issue of the `RuntimeStatus`.

use adam_error::{Classify, ErrorClass};
use adam_operator_ports::{RuntimeError, RuntimeId};

/// Why a call to the API server failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The API server could not be reached, or answered with a failure that may pass (a timeout, a
    /// `429`, a `5xx`).
    #[error("the Kubernetes API server is unavailable")]
    Unavailable(#[source] Box<kube::Error>),
    /// The API server refused the credentials or the verb (`401`, `403`). Usually an RBAC rule that
    /// is missing or has not propagated yet, so it is retried like an outage.
    #[error("the Kubernetes API server refused the request ({code}): {message}")]
    Forbidden {
        /// The HTTP status.
        code: u16,
        /// What the API server said.
        message: String,
    },
    /// Another writer got there first (`409`).
    #[error("{what} was changed by another writer: {message}")]
    Conflict {
        /// The object, as `Kind name`.
        what: String,
        /// What the API server said.
        message: String,
    },
    /// The API server found the object invalid (`400`, `422`): the same input never succeeds. The
    /// message is the server's, naming the field.
    #[error("the API server refused {what} as invalid: {message}")]
    Invalid {
        /// The object, as `Kind name`.
        what: String,
        /// What the API server said.
        message: String,
    },
    /// The object is not there (`404`).
    #[error("{what} not found")]
    NotFound {
        /// The object, as `Kind name`.
        what: String,
    },
    /// The spec cannot be made into objects (a rule of this provider that `RuntimeSpec::check` does
    /// not cover), or the runtime id has no namespace.
    #[error("{0}")]
    InvalidSpec(String),
    /// A response that is not what it should be: a bug.
    #[error("unexpected response for {what}")]
    Unexpected {
        /// The object, as `Kind name`.
        what: String,
        /// The cause.
        #[source]
        source: Box<kube::Error>,
    },
}

impl Error {
    /// Classify an error of the client for the object `what` (`Kind name`).
    pub fn from_kube(what: impl Into<String>, error: kube::Error) -> Self {
        let what = what.into();
        match error {
            kube::Error::Api(status) => {
                let message = status.message.clone();
                match status.code {
                    404 => Self::NotFound { what },
                    409 => Self::Conflict { what, message },
                    400 | 422 => Self::Invalid { what, message },
                    401 | 403 => Self::Forbidden {
                        code: status.code,
                        message,
                    },
                    408 | 425 | 429 | 500..=599 => {
                        Self::Unavailable(Box::new(kube::Error::Api(status)))
                    }
                    _ => Self::Unexpected {
                        what,
                        source: Box::new(kube::Error::Api(status)),
                    },
                }
            }
            kube::Error::SerdeError(_)
            | kube::Error::BuildRequest(_)
            | kube::Error::HttpError(_) => Self::Unexpected {
                what,
                source: Box::new(error),
            },
            // The rest is the transport: the connection, TLS, authentication, the configuration.
            other => Self::Unavailable(Box::new(other)),
        }
    }

    /// Whether the API server said the object is not there.
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound { .. })
    }
}

impl Classify for Error {
    fn class(&self) -> ErrorClass {
        match self {
            Self::Unavailable(_) | Self::Forbidden { .. } => ErrorClass::Transient,
            Self::Conflict { .. } => ErrorClass::Conflict,
            Self::Invalid { .. } | Self::InvalidSpec(_) => ErrorClass::Invalid,
            Self::NotFound { .. } => ErrorClass::NotFound,
            Self::Unexpected { .. } => ErrorClass::Internal,
        }
    }
}

impl Error {
    /// This error as the port's, for the runtime `id`.
    pub fn into_runtime(self, id: &RuntimeId) -> RuntimeError {
        match self {
            Self::Unavailable(_) | Self::Forbidden { .. } => {
                RuntimeError::Unavailable(Box::new(self))
            }
            Self::Conflict { .. } => RuntimeError::Conflict(self.to_string()),
            Self::Invalid { .. } | Self::InvalidSpec(_) => {
                RuntimeError::InvalidSpec(self.to_string())
            }
            Self::NotFound { .. } => RuntimeError::NotFound(id.clone()),
            Self::Unexpected { .. } => RuntimeError::Internal(Box::new(self)),
        }
    }
}

#[cfg(test)]
mod tests {
    use kube_status::status;

    use super::*;

    /// A stand-in for the API server's `Status`.
    mod kube_status {
        pub fn status(code: u16) -> kube::Error {
            let status: kube::core::Status = serde_json::from_value(serde_json::json!({
                "status": "Failure", "message": "a message", "reason": "Reason", "code": code
            }))
            .unwrap();
            kube::Error::Api(Box::new(status))
        }
    }

    #[test]
    fn each_status_has_the_class_that_decides_retrying() {
        let class = |code| Error::from_kube("Service x", status(code)).class();
        assert_eq!(class(404), ErrorClass::NotFound);
        assert_eq!(class(409), ErrorClass::Conflict);
        assert_eq!(class(422), ErrorClass::Invalid);
        assert_eq!(class(400), ErrorClass::Invalid);
        assert_eq!(class(403), ErrorClass::Transient);
        assert_eq!(class(401), ErrorClass::Transient);
        assert_eq!(class(429), ErrorClass::Transient);
        assert_eq!(class(503), ErrorClass::Transient);
        assert_eq!(class(418), ErrorClass::Internal);
    }

    #[test]
    fn the_port_error_has_the_same_class() {
        let id = RuntimeId::new("ns", "svc");
        for code in [404, 409, 422, 403, 503, 418] {
            let e = Error::from_kube("Service x", status(code));
            let class = e.class();
            assert_eq!(e.into_runtime(&id).class(), class, "{code}");
        }
        assert_eq!(
            Error::InvalidSpec("x".into()).into_runtime(&id).class(),
            ErrorClass::Invalid
        );
    }

    #[test]
    fn a_transport_error_is_transient() {
        let e = Error::from_kube("Service x", kube::Error::TlsRequired);
        assert_eq!(e.class(), ErrorClass::Transient);
    }
}
