//! What the cluster's answers mean for an environment.
//!
//! The API server's refusals are the deployment's policy speaking, and each has a different remedy,
//! so each is its own [`EnvError`]: a pod refused for **quota** is [`EnvError::Unavailable`] (no slot
//! now; the coder waits and tries again), a pod the admission policy or RBAC refuses is
//! [`EnvError::Refused`] (the deployment is wrong; waiting does not help), and a server that does not
//! answer is `Unavailable` too. Messages are the cluster's own, cut short; none holds a secret
//! (the API server never echoes one in a refusal).

use adam_workspace::EnvError;
use kube::Error as KubeError;

/// The longest message of the cluster that is kept.
const MESSAGE_CAP: usize = 400;

/// Cut `text` to [`MESSAGE_CAP`] characters, on one line.
pub(crate) fn clip(text: &str) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= MESSAGE_CAP {
        return one_line;
    }
    let mut cut: String = one_line.chars().take(MESSAGE_CAP).collect();
    cut.push('…');
    cut
}

/// Whether `e` is the API server saying the thing is not there.
pub(crate) fn is_not_found(e: &KubeError) -> bool {
    matches!(e, KubeError::Api(status) if status.code == 404)
}

/// Whether `e` is the API server saying the thing is already there.
pub(crate) fn is_already_exists(e: &KubeError) -> bool {
    matches!(e, KubeError::Api(status) if status.code == 409)
}

/// What `e` means, for something done to the cluster: `doing` says what ("making the pod").
pub(crate) fn env_error(e: &KubeError, doing: &str) -> EnvError {
    match e {
        KubeError::Api(status) => {
            let message = clip(&status.message);
            match status.code {
                // A ResourceQuota says "exceeded quota" in a 403. Anything else that is 403 is RBAC.
                403 if message.contains("exceeded quota") => EnvError::Unavailable(format!(
                    "no run pod can be made now: the namespace's quota is used up ({message})"
                )),
                401 | 403 => EnvError::Refused(format!("the cluster refused {doing}: {message}")),
                // Admission (the chart's policy) answers 422, and a malformed object 400.
                400 | 422 => EnvError::Refused(format!("the cluster refused {doing}: {message}")),
                code => EnvError::Unavailable(format!(
                    "the cluster could not do {doing} (HTTP {code}): {message}"
                )),
            }
        }
        other => EnvError::Unavailable(format!(
            "the cluster could not be reached for {doing}: {}",
            clip(&other.to_string())
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use adam_error::{Classify, ErrorClass};
    use kube::core::Status;

    fn api(code: u16, message: &str) -> KubeError {
        KubeError::Api(Box::new(
            Status::failure(message, "Whatever").with_code(code),
        ))
    }

    #[test]
    fn a_quota_refusal_is_unavailable_and_transient() {
        let e = env_error(
            &api(
                403,
                "pods \"adam-run-x\" is forbidden: exceeded quota: run-pods, requested: pods=1, used: pods=4, limited: pods=4",
            ),
            "making the pod",
        );
        assert!(matches!(e, EnvError::Unavailable(_)), "{e}");
        assert_eq!(e.class(), ErrorClass::Transient);
        assert!(e.to_string().contains("quota"));
    }

    #[test]
    fn rbac_and_admission_refusals_are_refused_and_invalid() {
        for (code, message) in [
            (
                403,
                "pods is forbidden: User cannot create resource \"pods\"",
            ),
            (
                422,
                "ValidatingAdmissionPolicy 'x' denied request: the image is not allowed",
            ),
            (400, "bad request"),
        ] {
            let e = env_error(&api(code, message), "making the pod");
            assert!(matches!(e, EnvError::Refused(_)), "{code}: {e}");
            assert_eq!(e.class(), ErrorClass::Invalid);
            assert!(e.to_string().contains(message), "{e}");
        }
    }

    #[test]
    fn a_server_that_fails_or_does_not_answer_is_unavailable() {
        assert!(matches!(
            env_error(&api(503, "etcd is down"), "listing pods"),
            EnvError::Unavailable(_)
        ));
        assert!(matches!(
            env_error(&KubeError::TlsRequired, "listing pods"),
            EnvError::Unavailable(_)
        ));
    }

    #[test]
    fn not_found_and_already_exists_are_told_apart() {
        assert!(is_not_found(&api(404, "gone")) && !is_not_found(&api(409, "x")));
        assert!(is_already_exists(&api(409, "there")) && !is_already_exists(&api(404, "x")));
        assert!(!is_not_found(&KubeError::TlsRequired));
    }

    #[test]
    fn a_long_message_is_cut_on_one_line() {
        let long = format!("a\n  b {}", "x".repeat(1000));
        let cut = clip(&long);
        assert!(!cut.contains('\n') && cut.chars().count() <= MESSAGE_CAP + 1);
        assert!(cut.starts_with("a b x"));
    }
}
