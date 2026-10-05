//! What a run pod is called, and how it is found again.
//!
//! A pod's name and its label value cannot hold a run id's every character the same way a store
//! does, and a run id is a UUID, long for a name that a person reads in `kubectl get pods`. The pod
//! is named after a short hash of the id, the label carries the same hash, and the **full id is an
//! annotation**: [`Environment::held_runs`](adam_workspace::Environment::held_runs) reads the run
//! ids back from it, and a name that collides with another run's is detected by it.

use sha2::{Digest, Sha256};

/// `app.kubernetes.io/managed-by`: whose pods these are. Every pod this crate makes carries it, and
/// the chart's admission policy refuses a pod of the coder's ServiceAccount without it.
pub const MANAGED_BY_LABEL: &str = "app.kubernetes.io/managed-by";

/// The value of [`MANAGED_BY_LABEL`].
pub const MANAGED_BY: &str = "adam-coder";

/// `app.kubernetes.io/instance`: the release, so that two deployments in one namespace never
/// sweep each other's pods.
pub const INSTANCE_LABEL: &str = "app.kubernetes.io/instance";

/// The run's short hash, as a label.
pub const RUN_LABEL: &str = "adam.vymalo.com/run";

/// The run's full id, as an annotation.
pub const RUN_ID_ANNOTATION: &str = "adam.vymalo.com/run-id";

/// The worker that made the pod (`WORKER_ID`, else the deployment id), as an annotation: the idle
/// timeout is that worker's, so that a worker never deletes a pod another worker is using.
pub const WORKER_ANNOTATION: &str = "adam.vymalo.com/worker";

/// How many hex digits of the run's hash name the pod: 48 bits, so a collision needs about 16
/// million runs alive at once, and the annotation catches it.
const HASH_DIGITS: usize = 12;

/// The short hash of a run id.
pub fn run_hash(run: &str) -> String {
    let digest = Sha256::digest(run.as_bytes());
    let mut hex = String::with_capacity(HASH_DIGITS);
    for byte in digest.iter().take(HASH_DIGITS.div_ceil(2)) {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex.truncate(HASH_DIGITS);
    hex
}

/// The name of the pod of a run: `adam-run-<hash>`.
pub fn pod_name(run: &str) -> String {
    format!("adam-run-{}", run_hash(run))
}

/// The label selector that finds the pods of one deployment.
pub fn selector(instance: &str) -> String {
    format!("{MANAGED_BY_LABEL}={MANAGED_BY},{INSTANCE_LABEL}={instance}")
}

/// Whether `value` can be a label value: up to 63 characters of letters, digits, `-`, `_` and `.`,
/// starting and ending with a letter or a digit, or empty.
pub fn is_label_value(value: &str) -> bool {
    let ok = |c: char| c.is_ascii_alphanumeric();
    value.len() <= 63
        && value.chars().all(|c| ok(c) || matches!(c, '-' | '_' | '.'))
        && value.chars().next().is_none_or(ok)
        && value.chars().last().is_none_or(ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_twelve_hex_digits_of_the_ids_hash_and_stable() {
        let run = "018f3a2b-7c1d-7000-8000-000000000001";
        let name = pod_name(run);
        assert_eq!(name, pod_name(run));
        let hash = name.strip_prefix("adam-run-").unwrap();
        assert_eq!(hash.len(), 12);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(hash, run_hash(run));
        assert_ne!(name, pod_name("018f3a2b-7c1d-7000-8000-000000000002"));
        // A DNS-1123 label: lowercase, short.
        assert!(name.len() <= 63 && name == name.to_ascii_lowercase());
    }

    #[test]
    fn the_selector_names_the_manager_and_the_release() {
        assert_eq!(
            selector("coder"),
            "app.kubernetes.io/managed-by=adam-coder,app.kubernetes.io/instance=coder"
        );
    }

    #[test]
    fn label_values_follow_kubernetes() {
        assert!(is_label_value("coder-0") && is_label_value("a.b_c-d") && is_label_value(""));
        assert!(!is_label_value("-a") && !is_label_value("a-") && !is_label_value("a b"));
        assert!(!is_label_value(&"x".repeat(64)) && is_label_value(&"x".repeat(63)));
    }
}
