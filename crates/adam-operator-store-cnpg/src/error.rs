//! Calls to the API server, as the port's errors.

use adam_operator_ports::StoreError;

/// A failed call on `what`, as the class that decides retrying: the same mapping as the runtime
/// provider's. `401` and `403` are retried like an outage (an RBAC rule that has not propagated), and
/// the message of a `409` or `422` is the API server's, which names the field.
pub(crate) fn map(what: impl Into<String>, error: kube::Error) -> StoreError {
    let what = what.into();
    match error {
        kube::Error::Api(status) => {
            let message = status.message.clone();
            match status.code {
                409 => {
                    StoreError::Conflict(format!("{what} was changed by another writer: {message}"))
                }
                400 | 422 => StoreError::InvalidSpec(format!(
                    "the API server refused {what} as invalid: {message}"
                )),
                401 | 403 | 408 | 425 | 429 | 500..=599 => {
                    StoreError::Unavailable(Box::new(kube::Error::Api(status)))
                }
                // A `404` of an object is read as `None` by the callers; this one is something else.
                _ => StoreError::Internal(Box::new(kube::Error::Api(status))),
            }
        }
        kube::Error::SerdeError(_) | kube::Error::BuildRequest(_) | kube::Error::HttpError(_) => {
            StoreError::Internal(Box::new(error))
        }
        // The rest is the transport: the connection, TLS, authentication, the configuration.
        other => StoreError::Unavailable(Box::new(other)),
    }
}

#[cfg(test)]
mod tests {
    use adam_operator_ports::{Classify, ErrorClass};

    use super::*;

    fn status(code: u16) -> kube::Error {
        let status: kube::core::Status = serde_json::from_value(serde_json::json!({
            "status": "Failure", "message": "a message", "reason": "Reason", "code": code
        }))
        .unwrap();
        kube::Error::Api(Box::new(status))
    }

    #[test]
    fn each_status_has_the_class_that_decides_retrying() {
        let class = |code| map("Cluster x", status(code)).class();
        assert_eq!(class(409), ErrorClass::Conflict);
        assert_eq!(class(422), ErrorClass::Invalid);
        assert_eq!(class(400), ErrorClass::Invalid);
        assert_eq!(class(403), ErrorClass::Transient);
        assert_eq!(class(401), ErrorClass::Transient);
        assert_eq!(class(429), ErrorClass::Transient);
        assert_eq!(class(503), ErrorClass::Transient);
        assert_eq!(class(418), ErrorClass::Internal);
        assert_eq!(
            map("x", kube::Error::TlsRequired).class(),
            ErrorClass::Transient
        );
    }
}
