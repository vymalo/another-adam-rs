//! The pod template: what a run pod is, as the deployment says it.
//!
//! The image, the resources, the priority class, the security context, the volumes, the variables
//! that come from Secrets and the node affinity are the **deployment's**: they are in a file the
//! chart mounts (`RUN_POD_TEMPLATE_FILE`), a Pod in YAML or JSON (a bare PodSpec is accepted too).
//! The code fills in only what a template cannot know: the pod's name and namespace, its labels
//! and annotations ([`names`](crate::names)), and **the mounts of the workspace volume**: the
//! template mounts the volume once, at the workspace root, and the code replaces that one mount by
//! the mounts of the run's own directory and git directories ([`WorkspaceMounts`]), so that a run
//! pod never sees another run. Nothing else is added, and no owner reference: a pod outlives the
//! coder pod that made it, and the sweep finds it by label.
//!
//! The file is read once, at startup, and checked: a template that names the run pod's own labels,
//! has no container with the name the exec client enters, has no single mount of a volume at the
//! workspace root for the code to replace, or asks for what the cluster's admission policy refuses
//! (a host namespace, a `hostPath` volume, a privileged container, a service account token) is a
//! configuration error, so that it fails the coder's start and not a run.

use std::path::{Path, PathBuf};

use adam_workspace::{EnvError, RunWorkspace, SandboxShape};
use k8s_openapi::api::core::v1::{Container, Pod, PodSpec, VolumeMount};
use serde_json::Value;

use crate::mounts::WorkspaceMounts;
use crate::names::{
    INSTANCE_LABEL, MANAGED_BY, MANAGED_BY_LABEL, MOUNTS_ANNOTATION, RUN_ID_ANNOTATION, RUN_LABEL,
    WORKER_ANNOTATION, run_hash,
};
use crate::quantity;

/// A checked pod template.
#[derive(Debug, Clone)]
pub struct PodTemplate {
    pod: Pod,
    container: String,
    workspace: WorkspaceVolume,
    shape: SandboxShape,
}

/// The one mount of the workspace volume the template has, which the code replaces.
#[derive(Debug, Clone)]
struct WorkspaceVolume {
    /// The volume's name.
    volume: String,
    /// Where the run container mounts it: the workspace root, or a directory above it.
    path: PathBuf,
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
    /// container the processes run in; `workspace_root` is the coder's workspace root
    /// (`WORKSPACE_ROOT`, or the folder of the worker under it), which the run container must mount a
    /// volume at, or above, **once and as a whole**: that mount is the one the code replaces.
    ///
    /// # Errors
    ///
    /// [`EnvError::Config`] with `source` and what is wrong.
    pub fn parse(
        text: &str,
        source: &Path,
        container: &str,
        workspace_root: &Path,
    ) -> Result<Self, EnvError> {
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
        check(&pod, container).map_err(wrong)?;
        let root = std::path::absolute(workspace_root).unwrap_or_else(|_| workspace_root.into());
        let workspace = locate_workspace(&pod, container, &root).map_err(wrong)?;
        let shape = requests_of(&pod, container).map_err(wrong)?;
        Ok(Self {
            pod,
            container: container.to_owned(),
            workspace,
            shape,
        })
    }

    /// Read the template file at `path`; `container` and `workspace_root` are as for
    /// [`parse`](Self::parse).
    ///
    /// # Errors
    ///
    /// [`EnvError::Config`] when the file cannot be read or is not a usable template.
    pub fn from_file(
        path: &Path,
        container: &str,
        workspace_root: &Path,
    ) -> Result<Self, EnvError> {
        let text = std::fs::read_to_string(path).map_err(|e| EnvError::Config {
            file: path.to_owned(),
            reason: format!("cannot read the pod template: {e}"),
        })?;
        Self::parse(&text, path, container, workspace_root)
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

    /// What the pod reserves: the **requests** of the container the processes run in, which is
    /// what the scheduler reserves and the namespace's quota counts. A resource with a limit and no
    /// request is its limit, which Kubernetes copies into the request when it admits the pod. A
    /// resource with neither is `None`, not zero. A command's time in the pod is weighted by this
    /// ([`EnvSession::sandbox_shape`](adam_workspace::EnvSession::sandbox_shape)).
    pub fn sandbox_shape(&self) -> SandboxShape {
        self.shape
    }

    /// Where the template mounts the workspace volume: the volume's root is this directory, and the
    /// sub path of a [`Mount`](crate::Mount) is relative to it.
    pub fn workspace_path(&self) -> &Path {
        &self.workspace.path
    }

    /// The mounts `workspace` needs now: its own directory, and the git directories of its worktrees
    /// ([`WorkspaceMounts`]).
    ///
    /// # Errors
    ///
    /// [`EnvError::Refused`] when a path of the workspace is not below where the template mounts the
    /// volume (`WORKSPACE_ROOT` and the template disagree); [`EnvError::Unavailable`] when its slots
    /// cannot be listed.
    pub async fn mounts_for(&self, workspace: &RunWorkspace) -> Result<WorkspaceMounts, EnvError> {
        WorkspaceMounts::of(&self.workspace.path, workspace).await
    }

    /// The pod of `run`: the template with its name, namespace, labels and annotations filled in, and
    /// the run container's mount of the workspace volume replaced by `mounts`. Nothing else of the
    /// template changes.
    pub fn render(
        &self,
        run: &str,
        name: &str,
        at: Placement<'_>,
        mounts: &WorkspaceMounts,
    ) -> Pod {
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
        annotations.insert(MOUNTS_ANNOTATION.to_owned(), mounts.encode());
        let whole = self.workspace.path.to_string_lossy();
        if let Some(container) = pod
            .spec
            .iter_mut()
            .flat_map(|spec| spec.containers.iter_mut())
            .find(|c| c.name == self.container)
        {
            let volume_mounts = container.volume_mounts.get_or_insert_with(Vec::new);
            if let Some(index) = volume_mounts
                .iter()
                .position(|m| m.name == self.workspace.volume && m.mount_path == whole)
            {
                volume_mounts.splice(
                    index..=index,
                    mounts.iter().map(|mount| VolumeMount {
                        name: self.workspace.volume.clone(),
                        mount_path: mount.path().to_string_lossy().into_owned(),
                        sub_path: Some(mount.sub_path().to_owned()),
                        read_only: mount.read_only().then_some(true),
                        ..VolumeMount::default()
                    }),
                );
            }
        }
        pod
    }
}

/// What is wrong with the pod, in a sentence; `Ok` when nothing.
fn check(pod: &Pod, container: &str) -> Result<(), String> {
    let meta = &pod.metadata;
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
    for key in [RUN_ID_ANNOTATION, WORKER_ANNOTATION, MOUNTS_ANNOTATION] {
        if meta
            .annotations
            .as_ref()
            .is_some_and(|a| a.contains_key(key))
        {
            return Err(format!("the annotation {key} is the coder's to set"));
        }
    }
    let Some(spec) = &pod.spec else {
        return Err("there is no spec".to_owned());
    };
    if spec.containers.iter().all(|c| c.name != container) {
        return Err(format!(
            "no container is named {container:?} (RUN_POD_CONTAINER): it is the one the commands run in"
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

/// The requests of the run container as a shape, a missing request taking the limit as Kubernetes
/// does. A quantity that is not one is an error, so that it fails the coder's start (the cluster
/// would refuse the pod, run after run).
fn requests_of(pod: &Pod, container: &str) -> Result<SandboxShape, String> {
    let resources = pod
        .spec
        .iter()
        .flat_map(|spec| &spec.containers)
        .find(|c| c.name == container)
        .and_then(|c| c.resources.as_ref());
    let read = |name: &str, parse: fn(&str) -> Option<u64>| -> Result<Option<u64>, String> {
        let request = resources
            .and_then(|r| r.requests.as_ref())
            .and_then(|requests| requests.get(name));
        let limit = resources
            .and_then(|r| r.limits.as_ref())
            .and_then(|limits| limits.get(name));
        request
            .or(limit)
            .map(|quantity| {
                parse(&quantity.0).ok_or_else(|| {
                    format!(
                        "the container {container:?} asks for {name} {:?}, which is not a Kubernetes quantity",
                        quantity.0
                    )
                })
            })
            .transpose()
    };
    Ok(SandboxShape::new(
        read("cpu", quantity::cpu_millis)?,
        read("memory", quantity::bytes)?,
    ))
}

/// The run container's mount of the workspace volume, which the code replaces by the mounts of the run.
///
/// It is the mount whose path is the workspace root or the closest directory above it. It must be a
/// mount of the whole volume that the container can write, of a volume the pod has, and no other mount
/// of that volume may exist in the pod (another mount of the volume, or a sidecar's, would show what
/// the replacement hides).
fn locate_workspace(pod: &Pod, container: &str, root: &Path) -> Result<WorkspaceVolume, String> {
    let spec: &PodSpec = pod.spec.as_ref().ok_or("there is no spec")?;
    let run: &Container = spec
        .containers
        .iter()
        .find(|c| c.name == container)
        .ok_or_else(|| format!("no container is named {container:?}"))?;
    let at_root = || {
        format!(
            "the container {container:?} must mount the workspace volume at the workspace root {} (WORKSPACE_ROOT), or above it, whole: the coder replaces that mount by the mounts of the run's own directory so that a run pod sees no other run",
            root.display()
        )
    };
    let mount = run
        .volume_mounts
        .iter()
        .flatten()
        .filter(|m| root.starts_with(&m.mount_path))
        .max_by_key(|m| Path::new(&m.mount_path).components().count())
        .ok_or_else(at_root)?;
    let path = PathBuf::from(&mount.mount_path);
    if path.parent().is_none() {
        return Err("the workspace volume cannot be mounted at /".to_owned());
    }
    if mount.sub_path.is_some() || mount.sub_path_expr.is_some() {
        return Err(format!(
            "the mount of {:?} at {} has a subPath: it must be the whole volume, so that the paths of the coder mean the same here",
            mount.name, mount.mount_path
        ));
    }
    if mount.read_only == Some(true) {
        return Err(format!(
            "the mount of {:?} at {} is read-only: a run writes its files there",
            mount.name, mount.mount_path
        ));
    }
    if !spec
        .volumes
        .iter()
        .flatten()
        .any(|volume| volume.name == mount.name)
    {
        return Err(format!(
            "the container mounts {:?} but the pod has no such volume",
            mount.name
        ));
    }
    let others = spec
        .containers
        .iter()
        .chain(spec.init_containers.iter().flatten())
        .flat_map(|c| {
            c.volume_mounts
                .iter()
                .flatten()
                .filter(|m| m.name == mount.name)
                .map(move |m| (c, m))
        })
        .filter(|(c, m)| !(c.name == run.name && m.mount_path == mount.mount_path));
    if let Some((c, m)) = others.into_iter().next() {
        return Err(format!(
            "the workspace volume {:?} is also mounted by the container {:?} at {}: only the run container mounts it, once, because the coder narrows that mount to the run",
            mount.name, c.name, m.mount_path
        ));
    }
    Ok(WorkspaceVolume {
        volume: mount.name.clone(),
        path,
    })
}

/// The first line of an error: the YAML reader's message can carry a rendered snippet.
fn first_line(message: &str) -> String {
    message.lines().next().unwrap_or(message).trim().to_owned()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::mounts::{SlotMount, WorktreeDirs};

    /// A template as the chart renders it, cut to what the checks look at. `{ROOT}` is the workspace
    /// root the volume `work` is mounted at.
    const TEMPLATE_TEXT: &str = r"
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
      volumeMounts:
        - {name: tools, mountPath: /tools}
  containers:
    - name: run
      image: ghcr.io/vymalo/another-agentic-images/workspace:1.98.1-e6a528f
      command: [tini, --, sleep, infinity]
      env:
        - name: CARGO_BUILD_JOBS
          value: '2'
        - name: HTTPS_PROXY
          value: 'http://coder-egress.coder-ns.svc:3128'
        - name: NO_PROXY
          value: 'localhost,127.0.0.1'
      resources:
        requests: {cpu: 250m, memory: 512Mi}
        limits: {memory: 2Gi}
      volumeMounts:
        - {name: work, mountPath: '{ROOT}'}
        - {name: cache, mountPath: /cache}
        - {name: tools, mountPath: /opt/adam/bin, readOnly: true}
  volumes:
    - {name: work, persistentVolumeClaim: {claimName: work-coder-0}}
    - {name: cache, persistentVolumeClaim: {claimName: coder-cache}}
    - {name: tools, emptyDir: {}}
";

    /// The template, with the workspace volume mounted at `root`.
    pub(crate) fn template_text(root: &str) -> String {
        TEMPLATE_TEXT.replace("{ROOT}", root)
    }

    pub(crate) fn template_at(root: &Path) -> PodTemplate {
        PodTemplate::parse(
            &template_text(&root.to_string_lossy()),
            Path::new("pod.yaml"),
            "run",
            root,
        )
        .unwrap()
    }

    pub(crate) fn template() -> PodTemplate {
        template_at(Path::new("/work"))
    }

    fn refused_at(text: &str, root: &str) -> String {
        match PodTemplate::parse(text, Path::new("pod.yaml"), "run", Path::new(root)).unwrap_err() {
            EnvError::Config { file, reason } => {
                assert_eq!(file, Path::new("pod.yaml"));
                reason
            }
            other => panic!("{other}"),
        }
    }

    fn refused(text: &str) -> String {
        refused_at(text, "/work")
    }

    /// A pod with a run container `run` and these `volumeMounts` of it (YAML flow items), and the
    /// volumes `work` and `other`.
    fn with_mounts(mounts: &str) -> String {
        format!(
            "spec:\n  automountServiceAccountToken: false\n  containers:\n    - name: run\n      image: i\n      volumeMounts: [{mounts}]\n  volumes:\n    - {{name: work, emptyDir: {{}}}}\n    - {{name: other, emptyDir: {{}}}}\n"
        )
    }

    fn placement() -> Placement<'static> {
        Placement {
            namespace: "coder-ns",
            instance: "coder",
            worker: "coder-0",
        }
    }

    fn mounts_of(pod: &Pod) -> Vec<VolumeMount> {
        let spec = pod.spec.as_ref().unwrap();
        spec.containers
            .iter()
            .find(|c| c.name == "run")
            .unwrap()
            .volume_mounts
            .clone()
            .unwrap()
    }

    fn one_worktree() -> WorkspaceMounts {
        WorkspaceMounts::plan(
            Path::new("/work"),
            Path::new("/work/workspaces/r1"),
            &[SlotMount {
                path: "/work/workspaces/r1/r".into(),
                generation: Some("1a-0".to_owned()),
            }],
            &[WorktreeDirs {
                mirror: "/work/git/github.com/o/r.git".into(),
                admin: "/work/git/github.com/o/r.git/worktrees/r".into(),
                generation: None,
            }],
        )
        .unwrap()
    }

    #[test]
    fn a_pod_is_read_and_its_image_is_the_run_containers() {
        let t = template();
        assert_eq!(
            t.image(),
            Some("ghcr.io/vymalo/another-agentic-images/workspace:1.98.1-e6a528f")
        );
        assert_eq!(t.container(), "run");
        assert_eq!(t.workspace_path(), Path::new("/work"));
    }

    #[test]
    fn the_shape_is_the_run_containers_requests_or_their_limits_and_never_a_sidecars() {
        // 250m and 512Mi are the requests; the limit of 2Gi is not a cost.
        assert_eq!(
            template().sandbox_shape(),
            SandboxShape::new(Some(250), Some(512 * 1024 * 1024))
        );

        // A resource with neither a request nor a limit is absent, not zero; a request wins over a limit.
        let cpu_only = template_text("/work")
            .replace(
                "requests: {cpu: 250m, memory: 512Mi}",
                "requests: {cpu: '1.5'}",
            )
            .replace("limits: {memory: 2Gi}", "limits: {cpu: '2'}");
        let t = PodTemplate::parse(&cpu_only, Path::new("pod.yaml"), "run", Path::new("/work"))
            .unwrap();
        assert_eq!(t.sandbox_shape(), SandboxShape::new(Some(1500), None));
        let bare = template_text("/work")
            .replace("requests: {cpu: 250m, memory: 512Mi}\n", "")
            .replace("limits: {memory: 2Gi}", "limits: {cpu: '4', memory: 2Gi}");
        let t =
            PodTemplate::parse(&bare, Path::new("pod.yaml"), "run", Path::new("/work")).unwrap();
        assert_eq!(
            t.sandbox_shape(),
            SandboxShape::new(Some(4000), Some(2 * 1024 * 1024 * 1024)),
            "only limits: Kubernetes copies each limit into the missing request"
        );

        // The init container's requests are not the run container's.
        let sidecar = template_text("/work").replace(
            "      command: [sh, -c, 'cp -L /opt/adam/bin/* /tools/']",
            "      command: [sh, -c, 'cp -L /opt/adam/bin/* /tools/']\n      resources: {requests: {cpu: 8, memory: 64Gi}}",
        );
        let t =
            PodTemplate::parse(&sidecar, Path::new("pod.yaml"), "run", Path::new("/work")).unwrap();
        assert_eq!(
            t.sandbox_shape(),
            SandboxShape::new(Some(250), Some(512 * 1024 * 1024))
        );
    }

    #[test]
    fn a_request_that_is_not_a_quantity_fails_the_start_and_says_which() {
        for (wrong, name) in [
            ("{cpu: 250x, memory: 512Mi}", "cpu"),
            ("{cpu: 250m, memory: 512mb}", "memory"),
            ("{cpu: -1}", "cpu"),
        ] {
            let text = template_text("/work").replace("{cpu: 250m, memory: 512Mi}", wrong);
            let reason = refused(&text);
            assert!(
                reason.contains(&format!("asks for {name}"))
                    && reason.contains("not a Kubernetes quantity"),
                "{reason}"
            );
        }
    }

    #[test]
    fn a_bare_pod_spec_and_json_are_read_too() {
        let spec = r#"{"automountServiceAccountToken": false, "containers": [{"name": "run", "image": "img", "volumeMounts": [{"name": "w", "mountPath": "/work"}]}], "volumes": [{"name": "w", "emptyDir": {}}]}"#;
        let t =
            PodTemplate::parse(spec, Path::new("spec.json"), "run", Path::new("/work")).unwrap();
        assert_eq!(t.image(), Some("img"));
    }

    #[test]
    fn render_fills_in_the_name_the_labels_the_annotations_and_the_workspace_mounts_and_nothing_else()
     {
        let t = template();
        let run = "018f3a2b-7c1d-7000-8000-000000000001";
        let mounts = one_worktree();
        let pod = t.render(run, "adam-run-abc", placement(), &mounts);
        assert_eq!(pod.metadata.name.as_deref(), Some("adam-run-abc"));
        assert_eq!(pod.metadata.namespace.as_deref(), Some("coder-ns"));
        let labels = pod.metadata.labels.clone().unwrap();
        assert_eq!(labels["app.kubernetes.io/managed-by"], "adam-coder");
        assert_eq!(labels["app.kubernetes.io/instance"], "coder");
        assert_eq!(labels["adam.vymalo.com/run"], run_hash(run));
        assert_eq!(labels["team"], "coder", "the template's own labels stay");
        let annotations = pod.metadata.annotations.clone().unwrap();
        assert_eq!(annotations["adam.vymalo.com/run-id"], run);
        assert_eq!(annotations["adam.vymalo.com/worker"], "coder-0");
        assert_eq!(
            annotations["adam.vymalo.com/workspace-mounts"],
            mounts.encode()
        );
        assert!(pod.metadata.owner_references.is_none());

        // The spec is the template's, but for the run container's one mount of the volume `work`.
        let mut expected = t.pod.spec.clone().unwrap();
        let run_container = expected
            .containers
            .iter_mut()
            .find(|c| c.name == "run")
            .unwrap();
        let mut got = pod.spec.clone().unwrap();
        let got_mounts = got
            .containers
            .iter_mut()
            .find(|c| c.name == "run")
            .unwrap()
            .volume_mounts
            .take();
        run_container.volume_mounts = None;
        got.containers
            .iter_mut()
            .find(|c| c.name == "run")
            .unwrap()
            .volume_mounts = None;
        assert_eq!(got, expected);
        // The environment is the template's, the chart's proxy variables among it (ADR 0042): a command in the pod reaches the
        // internet through the egress proxy or not at all, and only the template says where the proxy is.
        let env: Vec<(&str, &str)> = got
            .containers
            .iter()
            .find(|c| c.name == "run")
            .unwrap()
            .env
            .iter()
            .flatten()
            .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
            .collect();
        assert_eq!(
            env,
            [
                ("CARGO_BUILD_JOBS", "2"),
                ("HTTPS_PROXY", "http://coder-egress.coder-ns.svc:3128"),
                ("NO_PROXY", "localhost,127.0.0.1"),
            ]
        );
        assert_eq!(
            pod.spec.unwrap().priority_class_name.as_deref(),
            Some("coder-run")
        );

        // The mounts: the other mounts in their places, the whole-volume one replaced.
        let got_mounts = got_mounts.unwrap();
        let shape: Vec<(&str, &str, Option<&str>, Option<bool>)> = got_mounts
            .iter()
            .map(|m| {
                (
                    m.name.as_str(),
                    m.mount_path.as_str(),
                    m.sub_path.as_deref(),
                    m.read_only,
                )
            })
            .collect();
        assert_eq!(
            shape,
            [
                (
                    "work",
                    "/work/git/github.com/o/r.git",
                    Some("git/github.com/o/r.git"),
                    Some(true)
                ),
                (
                    "work",
                    "/work/git/github.com/o/r.git/worktrees/r",
                    Some("git/github.com/o/r.git/worktrees/r"),
                    None
                ),
                (
                    "work",
                    "/work/git/github.com/o/r.git/worktrees/r/HEAD",
                    Some("git/github.com/o/r.git/worktrees/r/HEAD"),
                    Some(true)
                ),
                (
                    "work",
                    "/work/git/github.com/o/r.git/worktrees/r/commondir",
                    Some("git/github.com/o/r.git/worktrees/r/commondir"),
                    Some(true)
                ),
                (
                    "work",
                    "/work/git/github.com/o/r.git/worktrees/r/gitdir",
                    Some("git/github.com/o/r.git/worktrees/r/gitdir"),
                    Some(true)
                ),
                (
                    "work",
                    "/work/git/github.com/o/r.git/worktrees/r/logs",
                    Some("git/github.com/o/r.git/worktrees/r/logs"),
                    Some(true)
                ),
                ("work", "/work/workspaces/r1", Some("workspaces/r1"), None),
                (
                    "work",
                    "/work/workspaces/r1/r",
                    Some("workspaces/r1/r"),
                    None
                ),
                ("cache", "/cache", None, None),
                ("tools", "/opt/adam/bin", None, Some(true)),
            ]
        );
        assert!(
            !got_mounts
                .iter()
                .any(|m| m.name == "work" && m.sub_path.is_none()),
            "no mount of the whole volume is left"
        );
    }

    #[test]
    fn a_run_without_worktrees_mounts_only_its_own_directory() {
        let t = template();
        let mounts = WorkspaceMounts::plan(
            t.workspace_path(),
            Path::new("/work/workspaces/r1"),
            &[],
            &[],
        )
        .unwrap();
        let pod = t.render("r1", "adam-run-r1", placement(), &mounts);
        let work: Vec<_> = mounts_of(&pod)
            .into_iter()
            .filter(|m| m.name == "work")
            .map(|m| (m.mount_path, m.sub_path, m.read_only))
            .collect();
        assert_eq!(
            work,
            [(
                "/work/workspaces/r1".to_owned(),
                Some("workspaces/r1".to_owned()),
                None
            )]
        );
    }

    #[test]
    fn the_workspace_root_may_be_below_where_the_volume_is_mounted() {
        // `affinity` placement: the worker's folder `/work/coder-1` is the root; the volume is at `/work`.
        let t = PodTemplate::parse(
            &template_text("/work"),
            Path::new("pod.yaml"),
            "run",
            Path::new("/work/coder-1"),
        )
        .unwrap();
        assert_eq!(t.workspace_path(), Path::new("/work"));
        let mounts = WorkspaceMounts::plan(
            t.workspace_path(),
            Path::new("/work/coder-1/workspaces/r1"),
            &[],
            &[],
        )
        .unwrap();
        let pod = t.render("r1", "adam-run-r1", placement(), &mounts);
        assert!(mounts_of(&pod).iter().any(|m| {
            m.mount_path == "/work/coder-1/workspaces/r1"
                && m.sub_path.as_deref() == Some("coder-1/workspaces/r1")
        }));
    }

    #[test]
    fn what_the_coder_sets_cannot_be_in_the_template() {
        let with = |meta: &str| {
            format!(
                "metadata:\n{meta}\nspec:\n  automountServiceAccountToken: false\n  containers:\n    - {{name: run, image: i, volumeMounts: [{{name: work, mountPath: /work}}]}}\n  volumes:\n    - {{name: work, emptyDir: {{}}}}\n"
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
                "  annotations: {adam.vymalo.com/workspace-mounts: x}"
            ))
            .contains("adam.vymalo.com/workspace-mounts")
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
    fn a_template_whose_workspace_mount_cannot_be_found_is_refused_at_startup() {
        // No mount at all, and a mount that is not at (or above) the workspace root.
        let none = refused(
            "spec:\n  automountServiceAccountToken: false\n  containers:\n    - {name: run, image: i}\n",
        );
        assert!(
            none.contains("mount the workspace volume at the workspace root /work"),
            "{none}"
        );
        assert!(refused(&with_mounts("{name: work, mountPath: /data}")).contains("workspace root"));
        assert!(
            refused(&with_mounts("{name: work, mountPath: /work/deeper}"))
                .contains("workspace root")
        );
        // The workspace root decides which mount it is.
        assert!(
            refused_at(&with_mounts("{name: work, mountPath: /work}"), "/data").contains("/data")
        );
        // A mount that is there but cannot be replaced by the run's own.
        assert!(
            refused(&with_mounts(
                "{name: work, mountPath: /work, subPath: pool}"
            ))
            .contains("subPath")
        );
        assert!(
            refused(&with_mounts(
                "{name: work, mountPath: /work, subPathExpr: $(X)}"
            ))
            .contains("subPath")
        );
        assert!(
            refused(&with_mounts(
                "{name: work, mountPath: /work, readOnly: true}"
            ))
            .contains("read-only")
        );
        assert!(refused(&with_mounts("{name: gone, mountPath: /work}")).contains("no such volume"));
        assert!(
            refused_at(&with_mounts("{name: work, mountPath: /}"), "/work")
                .contains("cannot be mounted at /")
        );
    }

    #[test]
    fn the_workspace_volume_may_be_mounted_once_and_by_the_run_container_alone() {
        // Twice by the run container: the second would show what the replacement hides.
        let twice = refused(&with_mounts(
            "{name: work, mountPath: /work}, {name: work, mountPath: /elsewhere}",
        ));
        assert!(
            twice.contains("also mounted") && twice.contains("/elsewhere"),
            "{twice}"
        );
        // By a sidecar or an init container.
        let sidecar = "spec:\n  automountServiceAccountToken: false\n  containers:\n    - {name: run, image: i, volumeMounts: [{name: work, mountPath: /work}]}\n    - {name: side, image: i, volumeMounts: [{name: work, mountPath: /w}]}\n  volumes:\n    - {name: work, emptyDir: {}}\n";
        assert!(refused(sidecar).contains("\"side\""));
        let init = "spec:\n  automountServiceAccountToken: false\n  initContainers:\n    - {name: seed, image: i, volumeMounts: [{name: work, mountPath: /w}]}\n  containers:\n    - {name: run, image: i, volumeMounts: [{name: work, mountPath: /work}]}\n  volumes:\n    - {name: work, emptyDir: {}}\n";
        assert!(refused(init).contains("\"seed\""));
        // Another volume in another container is nobody's business.
        let fine = "spec:\n  automountServiceAccountToken: false\n  initContainers:\n    - {name: seed, image: i, volumeMounts: [{name: other, mountPath: /w}]}\n  containers:\n    - {name: run, image: i, volumeMounts: [{name: work, mountPath: /work}]}\n  volumes:\n    - {name: work, emptyDir: {}}\n    - {name: other, emptyDir: {}}\n";
        assert!(PodTemplate::parse(fine, Path::new("pod.yaml"), "run", Path::new("/work")).is_ok());
    }

    #[test]
    fn a_file_that_is_not_a_pod_says_what_it_is() {
        assert!(refused("- a\n- b\n").contains("mapping"));
        assert!(refused("apiVersion: v1\nkind: Service\nspec: {}\n").contains("Service"));
        assert!(refused("spec: {containers: 3}\n").contains("not a Pod"));
        let missing =
            PodTemplate::from_file(Path::new("/no/such/pod.yaml"), "run", Path::new("/work"))
                .unwrap_err();
        assert!(
            missing.to_string().contains("/no/such/pod.yaml"),
            "{missing}"
        );
    }
}
