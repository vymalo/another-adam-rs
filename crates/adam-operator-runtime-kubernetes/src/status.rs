//! What the workloads and pods say, as a `RuntimeStatus`. Pure: the objects come in, the status goes
//! out, so every row of the issue table below is a unit test on a crafted Pod.
//!
//! | Issue | Seen as |
//! |---|---|
//! | `ConfigRejected` | a container that exits with code 78 (adam: the configuration was refused) |
//! | `DependencyUnavailable` | exit code 69 (adam: a dependency is unreachable) |
//! | `MissingSecret { name }` | `CreateContainerConfigError` about a Secret or one of its keys |
//! | `ImagePull` | `ErrImagePull`, `ImagePullBackOff` (and `InvalidImageName` and its kin) |
//! | `CrashLoop` | `CrashLoopBackOff` after any other exit |
//!
//! `NameConflict` is not read from pods: the adoption guard reports it.
//!
//! **No kubelet text is copied into an issue message.** A kubelet's message names Secrets and
//! images; an issue says what is wrong in words of its own, and the Secret's name is in the reason.

use std::collections::BTreeSet;

use adam_operator_ports::{Issue, IssueReason, Phase, Role, RuntimeStatus};
use k8s_openapi::api::apps::v1::{Deployment, StatefulSet};
use k8s_openapi::api::core::v1::{ContainerStatus, Pod};

use crate::names::{COMPONENT_LABEL, role_of};

/// adam's exit code for a configuration it refuses (`adam_service`, `bin/adam-agent/README.md`).
pub const EXIT_CONFIG: i32 = 78;
/// adam's exit code for a dependency it cannot reach.
pub const EXIT_UNAVAILABLE: i32 = 69;

/// One workload as the API server reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkloadObservation {
    /// The object's name.
    pub name: String,
    /// The role it runs (its `component` label).
    pub role: Role,
    /// Replicas asked for.
    pub desired: i32,
    /// Replicas that are ready.
    pub ready: i32,
    /// Every replica runs the current pod template, and the controller has seen the current spec.
    pub rolled_out: bool,
}

fn role(labels: Option<&std::collections::BTreeMap<String, String>>) -> Option<Role> {
    role_of(labels?.get(COMPONENT_LABEL)?)
}

/// Read a StatefulSet. `None` when it has no role label: it is not one of ours.
pub fn observe_stateful_set(s: &StatefulSet) -> Option<WorkloadObservation> {
    let role = role(s.metadata.labels.as_ref())?;
    let desired = s.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
    let st = s.status.as_ref();
    let seen = st.is_some_and(|st| {
        st.observed_generation.unwrap_or(0) >= s.metadata.generation.unwrap_or(0)
    });
    let ready = st.and_then(|st| st.ready_replicas).unwrap_or(0);
    let total = st.map_or(0, |st| st.replicas);
    let updated = st.and_then(|st| st.updated_replicas).unwrap_or(0);
    let same_revision = st.is_some_and(|st| {
        st.current_revision.is_some() && st.current_revision == st.update_revision
    });
    let rolled_out = seen
        && total == desired
        && ready == desired
        && if desired == 0 {
            true
        } else {
            updated == desired && same_revision
        };
    Some(WorkloadObservation {
        name: s.metadata.name.clone().unwrap_or_default(),
        role,
        desired,
        ready,
        rolled_out,
    })
}

/// Read a Deployment. `None` when it has no role label.
pub fn observe_deployment(d: &Deployment) -> Option<WorkloadObservation> {
    let role = role(d.metadata.labels.as_ref())?;
    let desired = d.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
    let st = d.status.as_ref();
    let seen = st.is_some_and(|st| {
        st.observed_generation.unwrap_or(0) >= d.metadata.generation.unwrap_or(0)
    });
    let ready = st.and_then(|st| st.ready_replicas).unwrap_or(0);
    let total = st.and_then(|st| st.replicas).unwrap_or(0);
    let updated = st.and_then(|st| st.updated_replicas).unwrap_or(0);
    // `total == desired` is what says no pod of an old template is left.
    let rolled_out = seen && total == desired && ready == desired && updated == desired;
    Some(WorkloadObservation {
        name: d.metadata.name.clone().unwrap_or_default(),
        role,
        desired,
        ready,
        rolled_out,
    })
}

/// The status of a runtime from its workloads and its pods.
///
/// * no workload: `Absent`;
/// * every workload asked to run nothing: `Suspended`, with no replica, whatever is still stopping;
/// * every workload rolled out and ready: `Ready`;
/// * otherwise `Failed` when a pod shows an issue (a rollout cannot finish without a change), and
///   `Provisioning` when nothing is wrong yet.
pub fn compute(workloads: &[WorkloadObservation], pods: &[Pod]) -> RuntimeStatus {
    if workloads.is_empty() {
        return RuntimeStatus::absent();
    }
    if workloads.iter().all(|w| w.desired == 0) {
        return RuntimeStatus {
            phase: Phase::Suspended,
            replicas: 0,
            issues: Vec::new(),
        };
    }
    let replicas = workloads
        .iter()
        .filter(|w| w.role.runs_workers())
        .map(|w| u32::try_from(w.ready).unwrap_or(0))
        .sum();
    if workloads.iter().all(|w| w.rolled_out) {
        return RuntimeStatus {
            phase: Phase::Ready,
            replicas,
            issues: Vec::new(),
        };
    }
    let issues = issues(pods);
    RuntimeStatus {
        phase: if issues.is_empty() {
            Phase::Provisioning
        } else {
            Phase::Failed
        },
        replicas,
        issues,
    }
}

/// The issues of the pods, once per role and reason however many replicas show it.
pub fn issues(pods: &[Pod]) -> Vec<Issue> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for pod in pods {
        if pod.metadata.deletion_timestamp.is_some() {
            continue;
        }
        let Some(role) = role(pod.metadata.labels.as_ref()) else {
            continue;
        };
        for issue in pod_issues(pod, role) {
            let key = format!("{:?}/{:?}", issue.role, issue.reason);
            if seen.insert(key) {
                out.push(issue);
            }
        }
    }
    out
}

/// What is wrong with one pod, in the order of its containers (the native sidecars first).
pub fn pod_issues(pod: &Pod, role: Role) -> Vec<Issue> {
    let Some(status) = &pod.status else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let init = status.init_container_statuses.iter().flatten();
    let main = status.container_statuses.iter().flatten();
    for cs in init.chain(main) {
        if let Some((reason, message)) = container_issue(pod, cs) {
            out.push(Issue {
                role,
                reason,
                message,
            });
        }
    }
    out
}

fn container_issue(pod: &Pod, cs: &ContainerStatus) -> Option<(IssueReason, String)> {
    let name = &cs.name;
    let state = cs.state.as_ref()?;
    if let Some(waiting) = &state.waiting {
        let message = waiting.message.as_deref().unwrap_or_default();
        return match waiting.reason.as_deref().unwrap_or_default() {
            "CreateContainerConfigError" => Some(config_error(pod, name, message)),
            "ErrImagePull"
            | "ImagePullBackOff"
            | "InvalidImageName"
            | "ImageInspectError"
            | "RegistryUnavailable"
            | "ErrImageNeverPull" => Some((
                IssueReason::ImagePull,
                format!("the image of container {name} cannot be pulled"),
            )),
            "CrashLoopBackOff" => {
                let code = cs
                    .last_state
                    .as_ref()
                    .and_then(|s| s.terminated.as_ref())
                    .map(|t| t.exit_code);
                Some(exit_issue(name, code))
            }
            _ => None,
        };
    }
    // A container that has just exited with one of adam's codes, before the kubelet backs off.
    let code = state.terminated.as_ref()?.exit_code;
    matches!(code, EXIT_CONFIG | EXIT_UNAVAILABLE).then(|| exit_issue(name, Some(code)))
}

fn exit_issue(name: &str, code: Option<i32>) -> (IssueReason, String) {
    match code {
        Some(EXIT_CONFIG) => (
            IssueReason::ConfigRejected,
            format!(
                "container {name} exited with code {EXIT_CONFIG}: the agent refused its configuration"
            ),
        ),
        Some(EXIT_UNAVAILABLE) => (
            IssueReason::DependencyUnavailable,
            format!(
                "container {name} exited with code {EXIT_UNAVAILABLE}: a dependency of the agent is unreachable"
            ),
        ),
        _ => (
            IssueReason::CrashLoop,
            format!("container {name} keeps exiting"),
        ),
    }
}

/// `CreateContainerConfigError`: a missing Secret or key is `MissingSecret`; any other reason the
/// kubelet could not build the container is a configuration it rejected.
fn config_error(pod: &Pod, container: &str, kubelet: &str) -> (IssueReason, String) {
    let lower = kubelet.to_ascii_lowercase();
    let about_secret = lower.contains("secret");
    if about_secret {
        return (
            IssueReason::MissingSecret {
                name: secret_name(kubelet).unwrap_or_else(|| first_secret_of(pod, container)),
            },
            format!("a Secret or a key that container {container} references does not exist"),
        );
    }
    (
        IssueReason::ConfigRejected,
        format!("the kubelet could not build container {container} from its configuration"),
    )
}

/// The Secret a kubelet message names: `secret "NAME" not found`, or
/// `couldn't find key KEY in Secret NAMESPACE/NAME`.
fn secret_name(message: &str) -> Option<String> {
    if let Some(at) = message.find("secret \"") {
        let rest = &message[at + "secret \"".len()..];
        let name = rest.split('"').next()?;
        if !name.is_empty() {
            return Some(name.to_owned());
        }
    }
    if let Some(at) = message.find("in Secret ") {
        let rest = &message[at + "in Secret ".len()..];
        let token = rest.split_whitespace().next()?;
        let name = token
            .rsplit('/')
            .next()?
            .trim_matches(|c| c == '"' || c == '\'');
        if !name.is_empty() {
            return Some(name.to_owned());
        }
    }
    None
}

/// When the message does not name it: the first Secret the container's environment references.
fn first_secret_of(pod: &Pod, container: &str) -> String {
    let Some(spec) = &pod.spec else {
        return String::new();
    };
    spec.init_containers
        .iter()
        .flatten()
        .chain(&spec.containers)
        .filter(|c| c.name == container)
        .flat_map(|c| c.env.iter().flatten())
        .filter_map(|e| e.value_from.as_ref()?.secret_key_ref.as_ref())
        .map(|s| s.name.clone())
        .next()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn pod(role: &str, statuses: Value) -> Pod {
        serde_json::from_value(json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "p-0", "namespace": "ns", "labels": {COMPONENT_LABEL: role}},
            "spec": {"containers": [{"name": "agent", "image": "i", "env": [
                {"name": "A", "valueFrom": {"secretKeyRef": {"name": "from-spec", "key": "k"}}}
            ]}]},
            "status": statuses
        }))
        .unwrap()
    }

    fn waiting(reason: &str, message: &str, last_exit: Option<i32>) -> Value {
        let mut cs = json!({
            "name": "agent", "ready": false, "restartCount": 3, "image": "i", "imageID": "",
            "state": {"waiting": {"reason": reason, "message": message}}
        });
        if let Some(code) = last_exit {
            cs["lastState"] = json!({"terminated": {"exitCode": code, "reason": "Error"}});
        }
        json!({"containerStatuses": [cs]})
    }

    fn only(p: &Pod) -> Issue {
        let mut issues = pod_issues(p, Role::All);
        assert_eq!(issues.len(), 1, "{issues:?}");
        issues.remove(0)
    }

    #[test]
    fn exit_78_in_a_crash_loop_is_config_rejected() {
        let i = only(&pod(
            "agent",
            waiting("CrashLoopBackOff", "back-off", Some(78)),
        ));
        assert_eq!(i.reason, IssueReason::ConfigRejected);
        assert_eq!(i.role, Role::All);
    }

    #[test]
    fn exit_69_in_a_crash_loop_is_dependency_unavailable() {
        let i = only(&pod("worker", waiting("CrashLoopBackOff", "", Some(69))));
        assert_eq!(i.reason, IssueReason::DependencyUnavailable);
    }

    #[test]
    fn any_other_exit_in_a_crash_loop_is_a_crash_loop() {
        for code in [Some(1), Some(137), Some(0), None] {
            let i = only(&pod("agent", waiting("CrashLoopBackOff", "", code)));
            assert_eq!(i.reason, IssueReason::CrashLoop, "{code:?}");
        }
    }

    #[test]
    fn a_container_that_just_exited_with_an_adam_code_is_read_before_the_back_off() {
        let status = |code: i32| {
            json!({"containerStatuses": [{
                "name": "agent", "ready": false, "restartCount": 0, "image": "i", "imageID": "",
                "state": {"terminated": {"exitCode": code, "reason": "Error"}}
            }]})
        };
        assert_eq!(
            only(&pod("agent", status(78))).reason,
            IssueReason::ConfigRejected
        );
        assert_eq!(
            only(&pod("agent", status(69))).reason,
            IssueReason::DependencyUnavailable
        );
        // Another exit is nothing yet: the kubelet restarts it, and only a loop is an issue.
        assert!(pod_issues(&pod("agent", status(1)), Role::All).is_empty());
    }

    #[test]
    fn image_pull_failures_are_image_pull() {
        for reason in ["ErrImagePull", "ImagePullBackOff", "InvalidImageName"] {
            let i = only(&pod(
                "agent",
                waiting(reason, "secret \"leak\" is in this text", None),
            ));
            assert_eq!(i.reason, IssueReason::ImagePull, "{reason}");
            assert!(!i.message.contains("leak"), "{}", i.message);
        }
    }

    #[test]
    fn a_missing_secret_names_the_secret_and_the_message_does_not() {
        let i = only(&pod(
            "agent",
            waiting(
                "CreateContainerConfigError",
                "secret \"coder-secrets\" not found",
                None,
            ),
        ));
        assert_eq!(
            i.reason,
            IssueReason::MissingSecret {
                name: "coder-secrets".to_owned()
            }
        );
        assert!(!i.message.contains("coder-secrets"), "{}", i.message);
    }

    #[test]
    fn a_missing_key_names_its_secret() {
        let i = only(&pod(
            "agent",
            waiting(
                "CreateContainerConfigError",
                "couldn't find key MODEL_API_KEY in Secret ns/coder-secrets",
                None,
            ),
        ));
        assert_eq!(
            i.reason,
            IssueReason::MissingSecret {
                name: "coder-secrets".to_owned()
            }
        );
    }

    #[test]
    fn a_message_that_names_no_secret_falls_back_to_the_pods_own_reference() {
        let i = only(&pod(
            "agent",
            waiting("CreateContainerConfigError", "Secret lookup failed", None),
        ));
        assert_eq!(
            i.reason,
            IssueReason::MissingSecret {
                name: "from-spec".to_owned()
            }
        );
    }

    #[test]
    fn another_config_error_is_a_rejected_configuration() {
        let i = only(&pod(
            "agent",
            waiting(
                "CreateContainerConfigError",
                "container has runAsNonRoot and image will run as root",
                None,
            ),
        ));
        assert_eq!(i.reason, IssueReason::ConfigRejected);
    }

    #[test]
    fn a_sidecar_that_cannot_start_is_an_issue_of_the_pod() {
        let p = pod(
            "worker",
            json!({"initContainerStatuses": [{
                "name": "github-mcp", "ready": false, "restartCount": 5, "image": "i", "imageID": "",
                "state": {"waiting": {"reason": "ImagePullBackOff"}}
            }]}),
        );
        let i = only(&p);
        assert_eq!(i.reason, IssueReason::ImagePull);
        assert_eq!(i.role, Role::All, "only(): the role is the one given");
        assert!(i.message.contains("github-mcp"));
    }

    #[test]
    fn a_pod_that_is_starting_has_no_issue() {
        for state in [
            json!({"waiting": {"reason": "ContainerCreating"}}),
            json!({"waiting": {"reason": "PodInitializing"}}),
            json!({"running": {}}),
        ] {
            let p = pod(
                "agent",
                json!({"containerStatuses": [{"name": "agent", "ready": false, "restartCount": 0, "image": "i", "imageID": "", "state": state}]}),
            );
            assert!(pod_issues(&p, Role::All).is_empty());
        }
        assert!(pod_issues(&pod("agent", json!({})), Role::All).is_empty());
    }

    #[test]
    fn replicas_with_the_same_trouble_are_one_issue_and_terminating_pods_are_ignored() {
        let a = pod("agent", waiting("CrashLoopBackOff", "", Some(1)));
        let b = a.clone();
        let mut gone = a.clone();
        gone.metadata.deletion_timestamp =
            Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                k8s_openapi::jiff::Timestamp::UNIX_EPOCH,
            ));
        assert_eq!(issues(&[a.clone(), b]).len(), 1);
        assert!(issues(&[gone]).is_empty());
        let front = pod("front", waiting("CrashLoopBackOff", "", Some(1)));
        let both = issues(&[a, front]);
        assert_eq!(both.len(), 2, "one per role");
        assert_eq!(both[1].role, Role::ControlPlane);
    }

    // ------------------------------------------------------------ phases

    fn obs(role: Role, desired: i32, ready: i32, rolled_out: bool) -> WorkloadObservation {
        WorkloadObservation {
            name: "w".to_owned(),
            role,
            desired,
            ready,
            rolled_out,
        }
    }

    #[test]
    fn no_workload_is_absent() {
        assert_eq!(compute(&[], &[]), RuntimeStatus::absent());
    }

    #[test]
    fn nothing_asked_to_run_is_suspended_with_no_replica() {
        let s = compute(&[obs(Role::All, 0, 1, false)], &[]);
        assert_eq!(s.phase, Phase::Suspended);
        assert_eq!(s.replicas, 0, "a pod still stopping is not a replica");
    }

    #[test]
    fn ready_counts_the_workers_and_not_the_front() {
        let s = compute(
            &[
                obs(Role::Worker, 2, 2, true),
                obs(Role::ControlPlane, 3, 3, true),
            ],
            &[],
        );
        assert_eq!(s.phase, Phase::Ready);
        assert_eq!(s.replicas, 2);
    }

    #[test]
    fn a_rollout_in_progress_is_provisioning_and_a_stuck_one_is_failed() {
        let rolling = [obs(Role::All, 2, 1, false)];
        let s = compute(&rolling, &[]);
        assert_eq!((s.phase, s.replicas), (Phase::Provisioning, 1));
        let stuck = pod("agent", waiting("CrashLoopBackOff", "", Some(78)));
        let s = compute(&rolling, &[stuck]);
        assert_eq!(s.phase, Phase::Failed);
        assert_eq!(s.issues[0].reason, IssueReason::ConfigRejected);
    }

    #[test]
    fn the_front_not_being_ready_keeps_the_runtime_from_ready() {
        let s = compute(
            &[
                obs(Role::Worker, 2, 2, true),
                obs(Role::ControlPlane, 2, 1, false),
            ],
            &[],
        );
        assert_eq!(s.phase, Phase::Provisioning);
    }

    // ------------------------------------------------------------ workloads

    fn sts(spec_replicas: i32, generation: i64, status: Value) -> StatefulSet {
        serde_json::from_value(json!({
            "apiVersion": "apps/v1", "kind": "StatefulSet",
            "metadata": {"name": "w", "generation": generation, "labels": {COMPONENT_LABEL: "agent"}},
            "spec": {"replicas": spec_replicas, "selector": {}, "serviceName": "w",
                     "template": {"spec": {"containers": []}}},
            "status": status
        }))
        .unwrap()
    }

    #[test]
    fn a_stateful_set_is_rolled_out_when_every_pod_is_on_the_new_revision() {
        let done = json!({"observedGeneration": 4, "replicas": 2, "readyReplicas": 2,
                          "updatedReplicas": 2, "currentRevision": "r2", "updateRevision": "r2"});
        let o = observe_stateful_set(&sts(2, 4, done)).unwrap();
        assert!(o.rolled_out);
        assert_eq!((o.desired, o.ready, o.role), (2, 2, Role::All));

        let rolling = json!({"observedGeneration": 4, "replicas": 2, "readyReplicas": 2,
                             "updatedReplicas": 1, "currentRevision": "r1", "updateRevision": "r2"});
        assert!(
            !observe_stateful_set(&sts(2, 4, rolling))
                .unwrap()
                .rolled_out
        );

        let unseen = json!({"observedGeneration": 3, "replicas": 2, "readyReplicas": 2,
                            "updatedReplicas": 2, "currentRevision": "r2", "updateRevision": "r2"});
        assert!(
            !observe_stateful_set(&sts(2, 4, unseen)).unwrap().rolled_out,
            "the controller has not seen the new spec"
        );
        assert!(
            !observe_stateful_set(&sts(2, 1, json!(null)))
                .unwrap()
                .rolled_out
        );
    }

    #[test]
    fn a_stateful_set_asked_for_nothing_is_rolled_out_when_nothing_runs() {
        let stopped = json!({"observedGeneration": 2, "replicas": 0});
        let o = observe_stateful_set(&sts(0, 2, stopped)).unwrap();
        assert!(o.rolled_out);
        assert_eq!(o.desired, 0);
    }

    #[test]
    fn a_deployment_with_an_old_pod_left_is_not_rolled_out() {
        let deploy = |status: Value| -> Deployment {
            serde_json::from_value(json!({
                "apiVersion": "apps/v1", "kind": "Deployment",
                "metadata": {"name": "d", "generation": 2, "labels": {COMPONENT_LABEL: "front"}},
                "spec": {"replicas": 2, "selector": {}, "template": {"spec": {"containers": []}}},
                "status": status
            }))
            .unwrap()
        };
        let done = json!({"observedGeneration": 2, "replicas": 2, "readyReplicas": 2, "updatedReplicas": 2});
        let o = observe_deployment(&deploy(done)).unwrap();
        assert!(o.rolled_out);
        assert_eq!(o.role, Role::ControlPlane);
        let old_left = json!({"observedGeneration": 2, "replicas": 3, "readyReplicas": 2, "updatedReplicas": 2});
        assert!(!observe_deployment(&deploy(old_left)).unwrap().rolled_out);
    }

    #[test]
    fn an_object_without_a_role_label_is_not_ours_to_read() {
        let mut s = sts(1, 1, json!(null));
        s.metadata.labels = None;
        assert!(observe_stateful_set(&s).is_none());
    }
}
