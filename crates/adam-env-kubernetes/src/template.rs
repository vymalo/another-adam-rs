//! The pod template: what a run pod is, as the deployment says it.
//!
//! The image, the resources, the priority class, the security context, the volumes, the variables
//! that come from Secrets and the node affinity are the **deployment's**: they are in a file the
//! chart mounts (`RUN_POD_TEMPLATE_FILE`), a Pod in YAML or JSON (a bare PodSpec is accepted too).
//! The code fills in only what a template cannot know: the pod's name and namespace, its labels
//! and annotations ([`names`](crate::names)). Nothing else is added, and no owner reference: a
//! pod outlives the coder pod that made it, and the sweep finds it by label.
//!
//! The file is read once, at startup, and checked: a template that names the run pod's own labels,
//! has no container with the name the exec client enters, or asks for what the cluster's admission
//! policy refuses (a host namespace, a `hostPath` volume, a privileged container, a service account
//! token) is a configuration error, so that it fails the coder's start and not a run.

use std::path::Path;

use adam_workspace::EnvError;
use k8s_openapi::api::core::v1::Pod;
use serde_json::Value;

use crate::names::{
    INSTANCE_LABEL, MANAGED_BY, MANAGED_BY_LABEL, RUN_ID_ANNOTATION, RUN_LABEL, WORKER_ANNOTATION,
    run_hash,
};

/// A checked pod template.
#[derive(Debug, Clone)]
pub struct PodTemplate {
    pod: Pod,
    container: String,
}

/// Where a run pod is made and who made it: what [`PodTemplate::render`] fills in.
#[derive(Debug, Clone, Copy)]
pub struct Placement<'a> {
    /// The namespace.
    pub namespace: &'a str,
    /// The release's instance label.
    pub instance: &'a str,
    /// The worker that makes the pod.
    pub worker: &'a str,
}

impl PodTemplate {
    /// Read the template in `text`. `source` is named in the errors; `container` is the name of the
    /// container the processes run in.
    ///
    /// # Errors
    ///
    /// [`EnvError::Config`] with `source` and what is wrong.
    pub fn parse(text: &str, source: &Path, container: &str) -> Result<Self, EnvError> {
        let wrong = |reason: String| EnvError::Config {
            file: source.to_owned(),
            reason,
        };
        let mut options = serde_saphyr::Options::default();
        options.strict_booleans = true;
        let mut value: Value = serde_saphyr::from_str_with_options(text, options)
            .map_err(|e| wrong(first_line(&e.to_string())))?;
        let Some(object) = value.as_object_mut() else {
            return Err(wrong("expected a Pod (or a PodSpec): a mapping".to_owned()));
        };
        // A bare PodSpec is wrapped; a Pod keeps its own `spec`.
        if !object.contains_key("spec") && object.contains_key("containers") {
            let spec = std::mem::take(object);
            object.insert("spec".to_owned(), Value::Object(spec));
        }
        for (key, expected) in [("apiVersion", "v1"), ("kind", "Pod")] {
            match object.get(key).and_then(Value::as_str) {
                None => {
                    object.insert(key.to_owned(), Value::String(expected.to_owned()));
                }
                Some(found) if found == expected => {}
                Some(found) => {
                    return Err(wrong(format!("{key} is {found:?}, expected {expected:?}")));
                }
            }
        }
        let pod: Pod =
            serde_json::from_value(value).map_err(|e| wrong(format!("not a Pod: {e}")))?;
        let template = Self {
            pod,
            container: container.to_owned(),
        };
        template.check().map_err(wrong)?;
        Ok(template)
    }

    /// Read the template file at `path`.
    ///
    /// # Errors
    ///
    /// [`EnvError::Config`] when the file cannot be read or is not a usable template.
    pub fn from_file(path: &Path, container: &str) -> Result<Self, EnvError> {
        let text = std::fs::read_to_string(path).map_err(|e| EnvError::Config {
            file: path.to_owned(),
            reason: format!("cannot read the pod template: {e}"),
        })?;
        Self::parse(&text, path, container)
    }

    /// The image of the container the processes run in, when the template names one.
    pub fn image(&self) -> Option<&str> {
        self.pod
            .spec
            .as_ref()?
            .containers
            .iter()
            .find(|c| c.name == self.container)?
            .image
            .as_deref()
    }

    /// The name of the container the processes run in.
    pub fn container(&self) -> &str {
        &self.container
    }

    /// The pod of `run`: the template with its name, namespace, labels and annotations filled in.
    pub fn render(&self, run: &str, name: &str, at: Placement<'_>) -> Pod {
        let mut pod = self.pod.clone();
        let meta = &mut pod.metadata;
        meta.name = Some(name.to_owned());
        meta.namespace = Some(at.namespace.to_owned());
        let labels = meta.labels.get_or_insert_with(Default::default);
        labels.insert(MANAGED_BY_LABEL.to_owned(), MANAGED_BY.to_owned());
        labels.insert(INSTANCE_LABEL.to_owned(), at.instance.to_owned());
        labels.insert(RUN_LABEL.to_owned(), run_hash(run));
        let annotations = meta.annotations.get_or_insert_with(Default::default);
        annotations.insert(RUN_ID_ANNOTATION.to_owned(), run.to_owned());
        annotations.insert(WORKER_ANNOTATION.to_owned(), at.worker.to_owned());
        pod
    }

    /// What is wrong with the template, in a sentence; `Ok` when nothing.
    fn check(&self) -> Result<(), String> {
        let meta = &self.pod.metadata;
        if meta.name.is_some() || meta.generate_name.is_some() {
            return Err(
                "metadata.name and metadata.generateName must be left out: the pod is named after its run"
                    .to_owned(),
            );
        }
        if meta
            .owner_references
            .as_ref()
            .is_some_and(|o| !o.is_empty())
        {
            return Err(
                "metadata.ownerReferences must be left out: a run pod outlives the coder pod that made it"
                    .to_owned(),
            );
        }
        for key in [MANAGED_BY_LABEL, INSTANCE_LABEL, RUN_LABEL] {
            if meta.labels.as_ref().is_some_and(|l| l.contains_key(key)) {
                return Err(format!("the label {key} is the coder's to set"));
            }
        }
        for key in [RUN_ID_ANNOTATION, WORKER_ANNOTATION] {
            if meta
                .annotations
                .as_ref()
                .is_some_and(|a| a.contains_key(key))
            {
                return Err(format!("the annotation {key} is the coder's to set"));
            }
        }
        let Some(spec) = &self.pod.spec else {
            return Err("there is no spec".to_owned());
        };
        if spec.containers.iter().all(|c| c.name != self.container) {
            return Err(format!(
                "no container is named {:?} (RUN_POD_CONTAINER): it is the one the commands run in",
                self.container
            ));
        }
        if spec.node_name.is_some() {
            return Err(
                "spec.nodeName pins the pod past the scheduler: use nodeSelector or affinity"
                    .to_owned(),
            );
        }
        for (field, on) in [
            ("hostNetwork", spec.host_network),
            ("hostPID", spec.host_pid),
            ("hostIPC", spec.host_ipc),
        ] {
            if on == Some(true) {
                return Err(format!(
                    "spec.{field} is refused: a run pod has its own namespaces"
                ));
            }
        }
        if spec.automount_service_account_token != Some(false) {
            return Err(
                "spec.automountServiceAccountToken must be false: a run pod holds no credential for the cluster"
                    .to_owned(),
            );
        }
        if spec
            .volumes
            .iter()
            .flatten()
            .any(|volume| volume.host_path.is_some())
        {
            return Err("a hostPath volume is refused".to_owned());
        }
        let containers = spec
            .containers
            .iter()
            .chain(spec.init_containers.iter().flatten());
        for container in containers {
            let security = container.security_context.as_ref();
            if security.is_some_and(|s| s.privileged == Some(true)) {
                return Err(format!("container {} is privileged", container.name));
            }
            if security.is_some_and(|s| s.allow_privilege_escalation == Some(true)) {
                return Err(format!(
                    "container {} allows privilege escalation",
                    container.name
                ));
            }
        }
        Ok(())
    }
}

/// The first line of an error: the YAML reader's message can carry a rendered snippet.
fn first_line(message: &str) -> String {
    message.lines().next().unwrap_or(message).trim().to_owned()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A template as the chart renders it, cut to what the checks look at.
    pub(crate) const TEMPLATE: &str = r"
apiVersion: v1
kind: Pod
metadata:
  labels:
    team: coder
spec:
  automountServiceAccountToken: false
  priorityClassName: coder-run
  initContainers:
    - name: tools
      image: ghcr.io/vymalo/another-adam-rs/coder:sha-abc1234
      command: [sh, -c, 'cp -L /opt/adam/bin/* /tools/']
  containers:
    - name: run
      image: ghcr.io/vymalo/another-agentic-images/workspace:1.98.1-ee2273e
      command: [tini, --, sleep, infinity]
      env:
        - name: CARGO_BUILD_JOBS
          value: '2'
      resources:
        requests: {cpu: 250m, memory: 512Mi}
        limits: {memory: 2Gi}
";

    pub(crate) fn template() -> PodTemplate {
        PodTemplate::parse(TEMPLATE, Path::new("pod.yaml"), "run").unwrap()
    }

    fn refused(text: &str) -> String {
        match PodTemplate::parse(text, Path::new("pod.yaml"), "run").unwrap_err() {
            EnvError::Config { file, reason } => {
                assert_eq!(file, Path::new("pod.yaml"));
                reason
            }
            other => panic!("{other}"),
        }
    }

    #[test]
    fn a_pod_is_read_and_its_image_is_the_run_containers() {
        let t = template();
        assert_eq!(
            t.image(),
            Some("ghcr.io/vymalo/another-agentic-images/workspace:1.98.1-ee2273e")
        );
        assert_eq!(t.container(), "run");
    }

    #[test]
    fn a_bare_pod_spec_and_json_are_read_too() {
        let spec = r#"{"automountServiceAccountToken": false, "containers": [{"name": "run", "image": "img"}]}"#;
        let t = PodTemplate::parse(spec, Path::new("spec.json"), "run").unwrap();
        assert_eq!(t.image(), Some("img"));
    }

    #[test]
    fn render_fills_in_the_name_the_labels_and_the_annotations_and_nothing_else() {
        let t = template();
        let run = "018f3a2b-7c1d-7000-8000-000000000001";
        let pod = t.render(
            run,
            "adam-run-abc",
            Placement {
                namespace: "coder-ns",
                instance: "coder",
                worker: "coder-0",
            },
        );
        assert_eq!(pod.metadata.name.as_deref(), Some("adam-run-abc"));
        assert_eq!(pod.metadata.namespace.as_deref(), Some("coder-ns"));
        let labels = pod.metadata.labels.unwrap();
        assert_eq!(labels["app.kubernetes.io/managed-by"], "adam-coder");
        assert_eq!(labels["app.kubernetes.io/instance"], "coder");
        assert_eq!(labels["adam.vymalo.com/run"], run_hash(run));
        assert_eq!(labels["team"], "coder", "the template's own labels stay");
        let annotations = pod.metadata.annotations.unwrap();
        assert_eq!(annotations["adam.vymalo.com/run-id"], run);
        assert_eq!(annotations["adam.vymalo.com/worker"], "coder-0");
        assert!(pod.metadata.owner_references.is_none());
        // The spec is the template's, untouched.
        assert_eq!(pod.spec, t.pod.spec);
        assert_eq!(
            pod.spec.unwrap().priority_class_name.as_deref(),
            Some("coder-run")
        );
    }

    #[test]
    fn what_the_coder_sets_cannot_be_in_the_template() {
        let with = |meta: &str| {
            format!(
                "metadata:\n{meta}\nspec:\n  automountServiceAccountToken: false\n  containers:\n    - {{name: run, image: i}}\n"
            )
        };
        assert!(refused(&with("  name: x")).contains("named after its run"));
        assert!(refused(&with("  generateName: x-")).contains("named after its run"));
        assert!(
            refused(&with("  labels: {adam.vymalo.com/run: x}")).contains("adam.vymalo.com/run")
        );
        assert!(
            refused(&with("  labels: {app.kubernetes.io/instance: x}"))
                .contains("app.kubernetes.io/instance")
        );
        assert!(
            refused(&with("  annotations: {adam.vymalo.com/run-id: x}"))
                .contains("adam.vymalo.com/run-id")
        );
        assert!(
            refused(&with(
                "  ownerReferences: [{apiVersion: v1, kind: Pod, name: o, uid: u}]"
            ))
            .contains("ownerReferences")
        );
    }

    #[test]
    fn a_template_the_cluster_would_refuse_is_refused_at_startup() {
        let spec = |extra: &str| {
            format!(
                "spec:\n  automountServiceAccountToken: false\n{extra}  containers:\n    - {{name: run, image: i}}\n"
            )
        };
        assert!(refused(&spec("  hostNetwork: true\n")).contains("hostNetwork"));
        assert!(refused(&spec("  hostPID: true\n")).contains("hostPID"));
        assert!(refused(&spec("  hostIPC: true\n")).contains("hostIPC"));
        assert!(refused(&spec("  nodeName: n1\n")).contains("nodeName"));
        assert!(
            refused(&spec("  volumes:\n    - {name: v, hostPath: {path: /}}\n"))
                .contains("hostPath")
        );
        assert!(
            refused(
                "spec:\n  automountServiceAccountToken: false\n  containers:\n    - {name: run, image: i, securityContext: {privileged: true}}\n"
            )
            .contains("privileged")
        );
        assert!(
            refused(
                "spec:\n  automountServiceAccountToken: false\n  initContainers:\n    - {name: t, image: i, securityContext: {allowPrivilegeEscalation: true}}\n  containers:\n    - {name: run, image: i}\n"
            )
            .contains("privilege escalation")
        );
        assert!(
            refused("spec:\n  containers:\n    - {name: run, image: i}\n")
                .contains("automountServiceAccountToken")
        );
    }

    #[test]
    fn the_container_the_commands_run_in_must_exist() {
        let reason = refused(
            "spec:\n  automountServiceAccountToken: false\n  containers:\n    - {name: other, image: i}\n",
        );
        assert!(reason.contains("\"run\""), "{reason}");
    }

    #[test]
    fn a_file_that_is_not_a_pod_says_what_it_is() {
        assert!(refused("- a\n- b\n").contains("mapping"));
        assert!(refused("apiVersion: v1\nkind: Service\nspec: {}\n").contains("Service"));
        assert!(refused("spec: {containers: 3}\n").contains("not a Pod"));
        let missing = PodTemplate::from_file(Path::new("/no/such/pod.yaml"), "run").unwrap_err();
        assert!(
            missing.to_string().contains("/no/such/pod.yaml"),
            "{missing}"
        );
    }
}
