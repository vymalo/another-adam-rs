//! The names this provider writes on the objects it makes: labels, annotations, the field
//! manager, and the encoding of file paths into ConfigMap keys.

use std::collections::BTreeMap;

use adam_operator_ports::{Role, RuntimeId};

/// The field manager of every server-side apply and patch.
pub const FIELD_MANAGER: &str = "adam-operator";

/// The value of `app.kubernetes.io/managed-by` on every object this provider owns. The adoption
/// guard compares with it: an object of the right name that does not carry it is not ours.
pub const MANAGED_BY_VALUE: &str = "adam-operator";

/// `app.kubernetes.io/name`: the workload's name on pods, the service's name on other objects.
pub const NAME_LABEL: &str = "app.kubernetes.io/name";
/// `app.kubernetes.io/instance`: the service the object belongs to.
pub const INSTANCE_LABEL: &str = "app.kubernetes.io/instance";
/// `app.kubernetes.io/managed-by`.
pub const MANAGED_BY_LABEL: &str = "app.kubernetes.io/managed-by";
/// `app.kubernetes.io/component`: `agent`, `worker` or `front` (the [`Role`] of a workload).
pub const COMPONENT_LABEL: &str = "app.kubernetes.io/component";
/// On a claim: the name of the volume it backs (`work`), so a delete can say which volumes stay.
pub const VOLUME_LABEL: &str = "agents.vymalo.com/volume";

/// The digest of what the pods run, on every object this provider applies and on the pod template.
pub const DIGEST_ANNOTATION: &str = "agents.vymalo.com/config-digest";
/// What a delete does to data (`Retain` or `Delete`), remembered from the last `ensure` on the
/// workloads and on the shared claims.
pub const DELETION_POLICY_ANNOTATION: &str = "agents.vymalo.com/deletion-policy";

/// The API group of the custom resources: an owner reference to it is an owner of ours.
pub const OWNER_GROUP: &str = "agents.vymalo.com";

/// The `component` label value of a role.
pub const fn component(role: Role) -> &'static str {
    match role {
        Role::All => "agent",
        Role::Worker => "worker",
        Role::ControlPlane => "front",
    }
}

/// The role a `component` label value names.
pub fn role_of(component: &str) -> Option<Role> {
    match component {
        "agent" => Some(Role::All),
        "worker" => Some(Role::Worker),
        "front" => Some(Role::ControlPlane),
        _ => None,
    }
}

/// The labels that select a workload's pods: its name and the service.
pub fn selector_labels(workload: &str, id: &RuntimeId) -> BTreeMap<String, String> {
    BTreeMap::from([
        (NAME_LABEL.to_owned(), workload.to_owned()),
        (INSTANCE_LABEL.to_owned(), id.name().to_owned()),
    ])
}

/// The labels of a workload and its pods.
pub fn workload_labels(workload: &str, role: Role, id: &RuntimeId) -> BTreeMap<String, String> {
    let mut labels = selector_labels(workload, id);
    labels.insert(MANAGED_BY_LABEL.to_owned(), MANAGED_BY_VALUE.to_owned());
    labels.insert(COMPONENT_LABEL.to_owned(), component(role).to_owned());
    labels
}

/// The labels of an object that belongs to the service as a whole.
pub fn service_labels(id: &RuntimeId) -> BTreeMap<String, String> {
    BTreeMap::from([
        (NAME_LABEL.to_owned(), id.name().to_owned()),
        (INSTANCE_LABEL.to_owned(), id.name().to_owned()),
        (MANAGED_BY_LABEL.to_owned(), MANAGED_BY_VALUE.to_owned()),
    ])
}

/// The label selector of everything this provider made for a service.
pub fn ours_selector(id: &RuntimeId) -> String {
    format!(
        "{MANAGED_BY_LABEL}={MANAGED_BY_VALUE},{INSTANCE_LABEL}={}",
        id.name()
    )
}

/// The name of the claim that backs a shared volume: `<service>-<volume>`.
pub fn shared_claim(id: &RuntimeId, volume: &str) -> String {
    format!("{}-{volume}", id.name())
}

/// The longest key a ConfigMap accepts.
const MAX_KEY: usize = 253;

/// The ConfigMap key of a file path. Keys are letters, digits, `-`, `_` and `.`, and a path has
/// `/`: every other byte, `/` and `_` itself included, becomes `_` and a code, so two paths never
/// share a key (`a/b` is `a_sb`, `a_b` is `a_ub`). `None` when the key would be too long.
pub fn config_map_key(path: &str) -> Option<String> {
    let mut key = String::with_capacity(path.len());
    for b in path.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'.' => key.push(char::from(b)),
            b'/' => key.push_str("_s"),
            b'_' => key.push_str("_u"),
            other => key.push_str(&format!("_x{other:02x}")),
        }
    }
    (key.len() <= MAX_KEY).then_some(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_injective_and_valid() {
        let paths = [
            "instructions.md",
            "skills/review/SKILL.md",
            "skills_review_SKILL.md",
            "a/b",
            "a_b",
            "a_sb",
            "über.md",
            "with space",
        ];
        let mut keys = std::collections::BTreeSet::new();
        for p in paths {
            let k = config_map_key(p).unwrap();
            assert!(
                k.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')),
                "{k}"
            );
            assert!(keys.insert(k), "{p} collides");
        }
        assert_eq!(
            config_map_key("skills/review/SKILL.md").unwrap(),
            "skills_sreview_sSKILL.md"
        );
    }

    #[test]
    fn a_path_that_cannot_be_a_key_is_refused() {
        assert!(config_map_key(&"a".repeat(254)).is_none());
        assert!(config_map_key(&"a".repeat(253)).is_some());
    }

    #[test]
    fn components_round_trip() {
        for r in [Role::All, Role::Worker, Role::ControlPlane] {
            assert_eq!(role_of(component(r)), Some(r));
        }
        assert_eq!(role_of("other"), None);
    }
}
