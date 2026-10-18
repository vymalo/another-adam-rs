//! `KubeEnvironment` and `adam-kube-exec` against a real cluster: a pod made from the chart's own
//! template, a command run in it, killed in it, swept when idle and released, and the chart's
//! admission policy and quota refusing what they should.
//!
//! **Gated**: it runs only with `ADAM_TEST_KUBECONFIG` set, and then needs
//!
//! * `ADAM_TEST_KUBECONFIG`: a kubeconfig whose user is **the coder's ServiceAccount** (the admission
//!   policy is matched to it, so a cluster-admin kubeconfig would test nothing);
//! * `ADAM_TEST_KUBE_TEMPLATE`: the pod template file, as the chart renders it (`runPods.enabled`);
//! * `ADAM_TEST_KUBE_NAMESPACE` (default `default`), `ADAM_TEST_KUBE_INSTANCE` (default `coder`), the
//!   release's namespace and instance label, and `ADAM_TEST_KUBE_WORKER` (default `ci-0`);
//! * optionally `ADAM_TEST_KUBE_QUOTA_PODS`, the `pods` of the chart's ResourceQuota, which turns on the
//!   test of the quota, and `ADAM_TEST_KUBE_MODEL_KEY`, the value of the model key the template's
//!   Secret holds, which turns on the test of what a spec hides;
//! * optionally `ADAM_TEST_KUBE_ISOLATION_MIRROR` and `ADAM_TEST_KUBE_ISOLATION_ADMIN`, which turn on the
//!   test that a run pod sees only its own run. The volume must hold, made by whoever sets the cluster
//!   up (`kind-run-pods.sh` does): a bare mirror at the first path, a worktree of it for the run
//!   `kind-iso-a` at `/work/workspaces/kind-iso-a/r` whose directory in the mirror is the second path, a
//!   worktree of the same mirror for the run `kind-iso-b` with a file `private.txt` in it,
//!   `/work/meta/kind-iso-b/r.json`, and another mirror at `/work/git/example.test/p/other.git`.
//!
//! The template's `/work` must be a volume the pod's user can write, and its image must have `sh`,
//! `env`, `cat` and `sleep` and carry `adam-exec` at `/opt/adam/bin` (the chart's init container); the
//! isolation test also needs `git`.
//!
//! `ADAM_TEST_REQUIRE_KUBERNETES=1` makes a run that would skip fail instead (the `kind` job sets it).
//! The job that sets all of this up is `.github/workflows/ci.yml` (`run-pods`), and
//! `deploy/coder/tests/kind-run-pods.sh` is what it runs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use adam_env_kubernetes::{
    KubeEnvironment, Placement, PodTemplate, Settings, WorkspaceMounts, WorktreeDirs, pod_name,
    run_hash,
};
use adam_workspace::{
    EnvError, EnvProgress, EnvSession, EnvStep, Environment, ExecId, ExecSpec, NoProgress,
    StaticToken, Workspaces,
};
use k8s_openapi::api::core::v1::{
    EnvFromSource, HostPathVolumeSource, Pod, SecretEnvSource, SecretVolumeSource, SecurityContext,
    Volume,
};
use kube::api::{AttachParams, DeleteParams, PostParams};
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::{Api, Client, Config};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const CLIENT: &str = env!("CARGO_BIN_EXE_adam-kube-exec");

struct Cluster {
    client: Client,
    kubeconfig: PathBuf,
    namespace: String,
    instance: String,
    worker: String,
    template_file: PathBuf,
    quota_pods: Option<usize>,
    model_key: Option<String>,
}

async fn cluster() -> Option<Cluster> {
    let Some(kubeconfig) = std::env::var_os("ADAM_TEST_KUBECONFIG") else {
        assert!(
            std::env::var("ADAM_TEST_REQUIRE_KUBERNETES").as_deref() != Ok("1"),
            "ADAM_TEST_KUBECONFIG is not set, and ADAM_TEST_REQUIRE_KUBERNETES=1 says the cluster test must run"
        );
        eprintln!("skipped: ADAM_TEST_KUBECONFIG is not set (see the header of this file)");
        return None;
    };
    let get =
        |name: &str, default: &str| std::env::var(name).unwrap_or_else(|_| default.to_owned());
    let kubeconfig = PathBuf::from(kubeconfig);
    let config = Config::from_custom_kubeconfig(
        Kubeconfig::read_from(&kubeconfig).expect("the kubeconfig"),
        &KubeConfigOptions::default(),
    )
    .await
    .expect("a client configuration");
    Some(Cluster {
        client: Client::try_from(config).expect("a client"),
        kubeconfig,
        namespace: get("ADAM_TEST_KUBE_NAMESPACE", "default"),
        instance: get("ADAM_TEST_KUBE_INSTANCE", "coder"),
        worker: get("ADAM_TEST_KUBE_WORKER", "ci-0"),
        template_file: PathBuf::from(
            std::env::var("ADAM_TEST_KUBE_TEMPLATE").expect("ADAM_TEST_KUBE_TEMPLATE"),
        ),
        quota_pods: std::env::var("ADAM_TEST_KUBE_QUOTA_PODS")
            .ok()
            .and_then(|n| n.parse().ok()),
        model_key: std::env::var("ADAM_TEST_KUBE_MODEL_KEY").ok(),
    })
}

#[derive(Default)]
struct Steps(std::sync::Mutex<Vec<EnvStep>>);

impl EnvProgress for Steps {
    fn step(&self, step: EnvStep) {
        // As they come, so that a pod that never starts says in the log where it was.
        eprintln!("step: {step:?}");
        self.0.lock().unwrap().push(step);
    }
}

impl Cluster {
    fn template(&self) -> PodTemplate {
        PodTemplate::from_file(&self.template_file, "run", Path::new("/work"))
            .expect("the chart's template")
    }

    fn pods(&self) -> Api<Pod> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    fn settings(&self) -> Settings {
        let mut settings = Settings::new(&self.namespace, &self.instance, &self.worker);
        settings.exec_client = PathBuf::from(CLIENT);
        settings.ready_poll = Duration::from_millis(500);
        settings.ready_timeout = Duration::from_secs(240);
        // The client gets only what it needs to reach the cluster: here, the kubeconfig.
        settings.client_env = BTreeMap::from([
            (
                "KUBECONFIG".to_owned(),
                self.kubeconfig.to_string_lossy().into_owned(),
            ),
            ("PATH".to_owned(), std::env::var("PATH").unwrap_or_default()),
            ("HOME".to_owned(), std::env::var("HOME").unwrap_or_default()),
        ]);
        settings
    }

    fn environment(&self, settings: Settings) -> KubeEnvironment {
        KubeEnvironment::new(self.client.clone(), settings, self.template())
    }

    fn workspaces(&self) -> Workspaces {
        Workspaces::new(
            "/work".into(),
            Arc::new(StaticToken::new("unused-in-these-tests")),
        )
    }

    /// A run id no other test of this file or earlier run used.
    fn run_id(&self, what: &str) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("kt-{what}-{:x}", nanos & 0xffff_ffff_ffff)
    }

    /// Run `argv` in the pod of `run`, through the API directly (not through `adam-kube-exec`): its
    /// exit code and stdout. For the setup of a test and for looking at what it did.
    async fn exec_in(&self, run: &str, argv: &[&str]) -> (i32, String) {
        let params = AttachParams::default()
            .container("run")
            .stdin(false)
            .stdout(true)
            .stderr(false);
        let mut process = self
            .pods()
            .exec(&pod_name(run), argv.iter().copied(), &params)
            .await
            .expect("exec");
        let status = process.take_status().expect("a status");
        let mut out = process.stdout().expect("stdout");
        let mut text = String::new();
        out.read_to_string(&mut text).await.unwrap();
        let status = status.await.expect("the end of the command");
        let code = if status.status.as_deref() == Some("Success") {
            0
        } else {
            status
                .details
                .and_then(|d| d.causes)
                .into_iter()
                .flatten()
                .find(|c| c.reason.as_deref() == Some("ExitCode"))
                .and_then(|c| c.message?.parse().ok())
                .unwrap_or(-1)
        };
        (code, text)
    }

    /// How many commands `adam-exec` started are alive in the pod of `run`.
    async fn active(&self, run: &str) -> u32 {
        let (code, out) = self
            .exec_in(run, &["/opt/adam/bin/adam-exec", "active"])
            .await;
        assert_eq!(code, 0);
        out.trim().parse().unwrap()
    }

    async fn wait_for(&self, what: &str, secs: u64, mut done: impl AsyncFnMut() -> bool) {
        let started = Instant::now();
        while !done().await {
            assert!(
                started.elapsed() < Duration::from_secs(secs),
                "timed out: {what}"
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn gone(&self, run: &str) -> bool {
        self.pods().get_opt(&pod_name(run)).await.unwrap().is_none()
    }

    async fn make_workdir(&self, run: &str) -> PathBuf {
        let dir = PathBuf::from(format!("/work/workspaces/{run}/app"));
        let (code, _) = self
            .exec_in(run, &["mkdir", "-p", dir.to_str().unwrap()])
            .await;
        assert_eq!(code, 0, "the pod's /work is writable by its user");
        dir
    }
}

/// What a command ended with.
struct Done {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Spawn `command` as the coder does (a process group of its own, stdin as asked) and wait.
async fn run(
    session: &Arc<dyn EnvSession>,
    spec: &ExecSpec,
    stdin: Option<&str>,
) -> (Done, ExecId) {
    let prepared = session.prepare(spec).unwrap();
    let mut command = prepared.command();
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    command.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    let mut child = command.spawn().unwrap();
    if let Some(text) = stdin {
        let mut pipe = child.stdin.take().unwrap();
        pipe.write_all(text.as_bytes()).await.unwrap();
        drop(pipe);
    }
    let output = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .expect("the command ends")
        .unwrap();
    (
        Done {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        },
        prepared.exec,
    )
}

#[tokio::test]
async fn against_a_real_cluster() {
    let Some(cluster) = cluster().await else {
        return;
    };
    // One after the other: the quota counts every pod of the namespace.
    commands_run_in_the_runs_own_pod(&cluster).await;
    kill_stops_what_the_client_started(&cluster).await;
    an_idle_pod_is_deleted_and_a_busy_one_is_not(&cluster).await;
    release_and_held_runs(&cluster).await;
    a_run_pod_sees_only_its_own_run(&cluster).await;
    the_admission_policy_refuses_everything_but_the_intended_pod(&cluster).await;
    if cluster.quota_pods.is_some() {
        the_quota_refuses_a_pod_beyond_its_limit(&cluster).await;
    } else {
        eprintln!("skipped the quota scenario: ADAM_TEST_KUBE_QUOTA_PODS is not set");
    }
}

async fn commands_run_in_the_runs_own_pod(c: &Cluster) {
    eprintln!("--- commands run in the run's own pod");
    let env = c.environment(c.settings());
    let run_id = c.run_id("cmd");
    let ws = c.workspaces().run(&run_id).unwrap();
    let steps = Steps::default();
    let session = env.ensure(&ws, &steps).await.expect("a pod of its own");
    assert!(
        steps.0.lock().unwrap().iter().any(|s| s.id == "run-pod"),
        "making the pod was reported"
    );
    assert!(session.describe().summary.contains(&pod_name(&run_id)));
    // Idempotent: the same pod, and nothing to report.
    let again = Steps::default();
    env.ensure(&ws, &again).await.unwrap();
    assert!(again.0.lock().unwrap().is_empty());
    let pod = c.pods().get(&pod_name(&run_id)).await.unwrap();
    let labels = pod.metadata.labels.unwrap();
    assert_eq!(labels["adam.vymalo.com/run"], run_hash(&run_id));
    assert_eq!(labels["app.kubernetes.io/managed-by"], "adam-coder");

    let cwd = c.make_workdir(&run_id).await;

    // Output on both streams, and the exit code.
    let (done, _) = run(
        &session,
        &ExecSpec::shell("echo out; echo err >&2; exit 3", &cwd),
        None,
    )
    .await;
    assert_eq!(
        (done.code, done.stdout.as_str(), done.stderr.as_str()),
        (Some(3), "out\n", "err\n")
    );
    // The working directory, and a variable the caller set.
    let (done, _) = run(
        &session,
        &ExecSpec::shell("pwd; printf '%s' \"$X\"", &cwd).env("X", "1 2"),
        None,
    )
    .await;
    assert_eq!(done.stdout, format!("{}\n1 2", cwd.display()));
    // Every word its own argument: nothing is split, expanded or run.
    let (done, _) = run(
        &session,
        &ExecSpec::argv(
            [
                "sh",
                "-c",
                "printf '%s|' \"$@\"",
                "sh",
                "a b",
                "c\"d",
                "$(echo no)",
                "--version",
            ],
            &cwd,
        ),
        None,
    )
    .await;
    assert_eq!(done.stdout, "a b|c\"d|$(echo no)|--version|");
    // A program that is not there is the shell's 127, not a failure of the client.
    let (done, _) = run(
        &session,
        &ExecSpec::argv(["no-such-program-anywhere"], &cwd),
        None,
    )
    .await;
    assert_ne!(done.code, Some(0));
    // stdin is passed on, and its end ends the command's.
    let (done, _) = run(&session, &ExecSpec::argv(["cat"], &cwd), Some("ping\n")).await;
    assert_eq!((done.code, done.stdout.as_str()), (Some(0), "ping\n"));
    // stdin closed (/dev/null): the command has none, so `cat` ends at once.
    let (done, _) = run(&session, &ExecSpec::argv(["cat"], &cwd), None).await;
    assert_eq!((done.code, done.stdout.as_str()), (Some(0), ""));
    // A big output arrives whole.
    let (done, _) = run(
        &session,
        &ExecSpec::shell("head -c 3000000 /dev/zero | tr '\\0' x", &cwd),
        None,
    )
    .await;
    assert_eq!(done.stdout.len(), 3_000_000);

    // What the spec hides is not in the command's environment; what it does not is.
    if let Some(key) = &c.model_key {
        let show = "printf '%s' \"${MODEL_API_KEY-unset}\"";
        let (done, _) = run(&session, &ExecSpec::shell(show, &cwd), None).await;
        assert_eq!(&done.stdout, key, "the pod has the key the Secret holds");
        let (done, _) = run(
            &session,
            &ExecSpec::shell(show, &cwd).hide(["MODEL_API_KEY"]),
            None,
        )
        .await;
        assert_eq!(done.stdout, "unset");
    }
    // The coder's own secrets are not in the pod at all.
    let (done, _) = run(
        &session,
        &ExecSpec::shell(
            "printf '%s' \"${GITHUB_TOKEN-}${DATABASE_URL-}${A2A_BEARER_TOKENS-}\"",
            &cwd,
        ),
        None,
    )
    .await;
    assert_eq!(done.stdout, "");
    // A directory outside the run's workspace is refused before anything starts.
    assert!(matches!(
        session.prepare(&ExecSpec::shell("true", "/etc")),
        Err(EnvError::Refused(_))
    ));
    env.release(&run_id).await.unwrap();
}

async fn kill_stops_what_the_client_started(c: &Cluster) {
    eprintln!("--- kill");
    let env = c.environment(c.settings());
    let run_id = c.run_id("kill");
    let ws = c.workspaces().run(&run_id).unwrap();
    let session = env.ensure(&ws, &NoProgress).await.unwrap();
    let cwd = c.make_workdir(&run_id).await;

    // The coder kills the client, then the session. Killing the client alone leaves the command.
    let prepared = session
        .prepare(&ExecSpec::argv(["sleep", "600"], &cwd))
        .unwrap();
    let mut client = prepared.command();
    client
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .process_group(0);
    let mut client = client.spawn().unwrap();
    c.wait_for("the command to be alive in the pod", 60, async || {
        c.active(&run_id).await == 1
    })
    .await;
    client.kill().await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        c.active(&run_id).await,
        1,
        "killing the client does not stop the command in the pod"
    );
    session.kill(&prepared.exec).await;
    c.wait_for("the command to be gone", 30, async || {
        c.active(&run_id).await == 0
    })
    .await;

    // And a kill of a command whose client is still there ends the client too.
    let prepared = session
        .prepare(&ExecSpec::argv(["sleep", "600"], &cwd))
        .unwrap();
    let mut client = prepared.command();
    client
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .process_group(0);
    let mut client = client.spawn().unwrap();
    c.wait_for("the second command", 60, async || {
        c.active(&run_id).await == 1
    })
    .await;
    session.kill(&prepared.exec).await;
    let ended = tokio::time::timeout(Duration::from_secs(30), client.wait())
        .await
        .expect("the client ends when its command is killed")
        .unwrap();
    assert!(
        !ended.success(),
        "a killed command is not a success: {ended}"
    );
    // A kill of nothing is no error.
    session.kill(&ExecId::new("kp-never-started")).await;
    env.release(&run_id).await.unwrap();
}

async fn an_idle_pod_is_deleted_and_a_busy_one_is_not(c: &Cluster) {
    eprintln!("--- idle");
    let mut settings = c.settings();
    settings.idle = Some(Duration::from_secs(2));
    let env = c.environment(settings);
    let run_id = c.run_id("idle");
    let ws = c.workspaces().run(&run_id).unwrap();
    let session = env.ensure(&ws, &NoProgress).await.unwrap();
    let cwd = c.make_workdir(&run_id).await;

    let prepared = session
        .prepare(&ExecSpec::argv(["sleep", "600"], &cwd))
        .unwrap();
    let mut client = prepared.command();
    client
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .process_group(0);
    let _client = client.spawn().unwrap();
    c.wait_for("the command to be alive", 60, async || {
        c.active(&run_id).await == 1
    })
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        !env.reap_idle().await.unwrap().contains(&run_id),
        "a pod that runs a command is not idle"
    );
    session.kill(&prepared.exec).await;
    c.wait_for("the command to be gone", 30, async || {
        c.active(&run_id).await == 0
    })
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(env.reap_idle().await.unwrap().contains(&run_id));
    c.wait_for("the pod to be gone", 60, async || c.gone(&run_id).await)
        .await;
    // The run comes back: a new pod.
    env.ensure(&ws, &NoProgress).await.unwrap();
    assert!(!c.gone(&run_id).await);
    env.release(&run_id).await.unwrap();
}

async fn release_and_held_runs(c: &Cluster) {
    eprintln!("--- release");
    let env = c.environment(c.settings());
    let run_id = c.run_id("rel");
    let ws = c.workspaces().run(&run_id).unwrap();
    env.ensure(&ws, &NoProgress).await.unwrap();
    assert!(env.held_runs().await.unwrap().contains(&run_id));
    env.release(&run_id).await.unwrap();
    c.wait_for("the pod to be gone", 60, async || c.gone(&run_id).await)
        .await;
    assert!(!env.held_runs().await.unwrap().contains(&run_id));
    // Idempotent.
    env.release(&run_id).await.unwrap();
}

/// What the chart's ValidatingAdmissionPolicy refuses: each change to a pod the coder would make is
/// refused, and the unchanged pod is accepted (so that a refusal is the change's).
async fn the_admission_policy_refuses_everything_but_the_intended_pod(c: &Cluster) {
    eprintln!("--- admission");
    let template = c.template();
    let placement = Placement {
        namespace: &c.namespace,
        instance: &c.instance,
        worker: &c.worker,
    };
    let pods = c.pods();

    // The pod `ensure` makes for a run that has no worktree: the template with the run's own mount.
    let good = |run: &str| {
        let run_dir = PathBuf::from(format!("/work/workspaces/{run}"));
        let mounts = WorkspaceMounts::plan(template.workspace_path(), &run_dir, &[]).unwrap();
        template.render(run, &pod_name(run), placement, &mounts)
    };
    let accepted = c.run_id("adm-ok");
    pods.create(&PostParams::default(), &good(&accepted))
        .await
        .expect("the intended pod is accepted");
    pods.delete(&pod_name(&accepted), &DeleteParams::default())
        .await
        .unwrap();

    type Change = Box<dyn Fn(&mut Pod)>;
    let changes: Vec<(&str, Change)> = vec![
        (
            "no run label",
            Box::new(|p| {
                p.metadata
                    .labels
                    .as_mut()
                    .unwrap()
                    .remove("adam.vymalo.com/run");
            }),
        ),
        (
            "another priority class",
            Box::new(|p| {
                p.spec.as_mut().unwrap().priority_class_name =
                    Some("system-cluster-critical".into());
            }),
        ),
        (
            "no priority class",
            Box::new(|p| p.spec.as_mut().unwrap().priority_class_name = None),
        ),
        (
            "an image that is not allowed",
            Box::new(|p| {
                p.spec.as_mut().unwrap().containers[0].image =
                    Some("docker.io/library/alpine:latest".into());
            }),
        ),
        (
            "a hostPath volume",
            Box::new(|p| {
                p.spec
                    .as_mut()
                    .unwrap()
                    .volumes
                    .get_or_insert_default()
                    .push(Volume {
                        name: "host".into(),
                        host_path: Some(HostPathVolumeSource {
                            path: "/".into(),
                            type_: None,
                        }),
                        ..Volume::default()
                    });
            }),
        ),
        (
            "another Secret",
            Box::new(|p| {
                p.spec
                    .as_mut()
                    .unwrap()
                    .volumes
                    .get_or_insert_default()
                    .push(Volume {
                        name: "other".into(),
                        secret: Some(SecretVolumeSource {
                            secret_name: Some("kube-root-ca.crt".into()),
                            ..SecretVolumeSource::default()
                        }),
                        ..Volume::default()
                    });
            }),
        ),
        (
            "another key of the Secret the model key is in",
            Box::new(|p| {
                let mut reference = model_key_reference(p);
                reference.key = "GITHUB_TOKEN".into();
                p.spec.as_mut().unwrap().containers[0]
                    .env
                    .get_or_insert_default()
                    .push(k8s_openapi::api::core::v1::EnvVar {
                        name: "TOKEN".into(),
                        value_from: Some(k8s_openapi::api::core::v1::EnvVarSource {
                            secret_key_ref: Some(reference),
                            ..Default::default()
                        }),
                        ..Default::default()
                    });
            }),
        ),
        (
            "the Secret the model key is in, as a volume",
            Box::new(|p| {
                let name = model_key_reference(p).name;
                p.spec
                    .as_mut()
                    .unwrap()
                    .volumes
                    .get_or_insert_default()
                    .push(Volume {
                        name: "model-secret".into(),
                        secret: Some(SecretVolumeSource {
                            secret_name: Some(name),
                            ..SecretVolumeSource::default()
                        }),
                        ..Volume::default()
                    });
            }),
        ),
        (
            "every key of the Secret the model key is in, as envFrom",
            Box::new(|p| {
                let name = model_key_reference(p).name;
                p.spec.as_mut().unwrap().containers[0]
                    .env_from
                    .get_or_insert_default()
                    .push(EnvFromSource {
                        secret_ref: Some(SecretEnvSource {
                            name,
                            optional: None,
                        }),
                        ..EnvFromSource::default()
                    });
            }),
        ),
        (
            "a privileged container",
            Box::new(|p| {
                p.spec.as_mut().unwrap().containers[0].security_context = Some(SecurityContext {
                    privileged: Some(true),
                    run_as_user: Some(10001),
                    run_as_non_root: Some(true),
                    ..SecurityContext::default()
                });
            }),
        ),
        (
            "privilege escalation",
            Box::new(|p| {
                p.spec.as_mut().unwrap().containers[0].security_context = Some(SecurityContext {
                    allow_privilege_escalation: Some(true),
                    run_as_user: Some(10001),
                    run_as_non_root: Some(true),
                    ..SecurityContext::default()
                });
            }),
        ),
        (
            "root",
            Box::new(|p| {
                p.spec.as_mut().unwrap().containers[0].security_context = Some(SecurityContext {
                    run_as_user: Some(0),
                    allow_privilege_escalation: Some(false),
                    ..SecurityContext::default()
                });
                let spec = p.spec.as_mut().unwrap();
                if let Some(pod_security) = spec.security_context.as_mut() {
                    pod_security.run_as_user = Some(0);
                    pod_security.run_as_non_root = Some(false);
                }
            }),
        ),
        (
            "the host network",
            Box::new(|p| p.spec.as_mut().unwrap().host_network = Some(true)),
        ),
        (
            "the host PID namespace",
            Box::new(|p| p.spec.as_mut().unwrap().host_pid = Some(true)),
        ),
        (
            "the host IPC namespace",
            Box::new(|p| p.spec.as_mut().unwrap().host_ipc = Some(true)),
        ),
        (
            "a service account token",
            Box::new(|p| p.spec.as_mut().unwrap().automount_service_account_token = Some(true)),
        ),
    ];
    for (what, change) in changes {
        let run = c.run_id("adm");
        let mut pod = good(&run);
        change(&mut pod);
        let refused = pods
            .create(&PostParams::default(), &pod)
            .await
            .expect_err(&format!("{what} must be refused"));
        let kube::Error::Api(status) = &refused else {
            panic!("{what}: {refused}");
        };
        assert!(
            matches!(status.code, 403 | 422),
            "{what}: HTTP {} {}",
            status.code,
            status.message
        );
        assert!(
            status.message.contains("denied request")
                || status.message.contains("ValidatingAdmissionPolicy"),
            "{what}: refused, but not by the policy: {}",
            status.message
        );
        eprintln!("refused {what}: HTTP {}", status.code);
    }
}

/// The reference to the model key the template's run container has, whose Secret is the one Secret
/// the policy allows.
fn model_key_reference(pod: &Pod) -> k8s_openapi::api::core::v1::SecretKeySelector {
    pod.spec
        .as_ref()
        .unwrap()
        .containers
        .iter()
        .flat_map(|c| c.env.iter().flatten())
        .find(|e| e.name == "MODEL_API_KEY")
        .and_then(|e| e.value_from.as_ref()?.secret_key_ref.clone())
        .expect("the template gives the run container MODEL_API_KEY from a Secret")
}

/// A run pod sees its own run and the git directories its worktree needs, and nothing of any other run:
/// not their files, not `meta/`, not another mirror, and it cannot write the mirror.
///
/// The pod is made as `ensure` makes it (the template with the run's mounts), by the coder's
/// ServiceAccount, so the policy admits it too. Needs the volume `kind-run-pods.sh` seeds: see the header.
async fn a_run_pod_sees_only_its_own_run(c: &Cluster) {
    eprintln!("--- a run pod sees only its own run");
    let (Ok(mirror), Ok(admin)) = (
        std::env::var("ADAM_TEST_KUBE_ISOLATION_MIRROR"),
        std::env::var("ADAM_TEST_KUBE_ISOLATION_ADMIN"),
    ) else {
        eprintln!(
            "skipped the isolation scenario: ADAM_TEST_KUBE_ISOLATION_MIRROR and _ADMIN are not set"
        );
        return;
    };
    const RUN: &str = "kind-iso-a";
    let template = c.template();
    let mounts = WorkspaceMounts::plan(
        template.workspace_path(),
        Path::new("/work/workspaces/kind-iso-a"),
        &[WorktreeDirs {
            mirror: PathBuf::from(&mirror),
            admin: PathBuf::from(&admin),
            generation: None,
        }],
    )
    .expect("the mounts");
    let pod = template.render(
        RUN,
        &pod_name(RUN),
        Placement {
            namespace: &c.namespace,
            instance: &c.instance,
            worker: &c.worker,
        },
        &mounts,
    );
    let pods = c.pods();
    // A leftover of an earlier attempt is not this attempt's pod.
    let _ = pods.delete(&pod_name(RUN), &DeleteParams::default()).await;
    c.wait_for("an earlier pod to be gone", 60, async || c.gone(RUN).await)
        .await;
    pods.create(&PostParams::default(), &pod)
        .await
        .expect("the policy admits the pod with the run's own mounts");
    c.wait_for("the pod to be ready", 240, async || {
        pods.get_opt(&pod_name(RUN))
            .await
            .unwrap()
            .and_then(|p| p.status)
            .and_then(|s| s.conditions)
            .is_some_and(|conditions| {
                conditions
                    .iter()
                    .any(|cond| cond.type_ == "Ready" && cond.status == "True")
            })
    })
    .await;

    let sh = async |script: &str| {
        let script = format!("export HOME=/tmp GIT_CONFIG_GLOBAL=/dev/null; {script}");
        c.exec_in(RUN, &["sh", "-c", &script]).await
    };
    let (code, out) = sh("ls /work").await;
    assert_eq!(
        (code, out.as_str()),
        (0, "git\nworkspaces\n"),
        "the volume's root"
    );
    let (code, out) = sh("ls /work/workspaces").await;
    assert_eq!((code, out.as_str()), (0, "kind-iso-a\n"), "no other run");
    // The mirror of the worktree, and not the other mirror the volume holds (`p/other.git`).
    let (code, out) =
        sh("ls /work/git; ls /work/git/example.test; ls /work/git/example.test/o").await;
    assert_eq!(
        (code, out.as_str()),
        (0, "example.test\no\nr.git\n"),
        "only the mirror of the worktree"
    );
    for hidden in [
        "/work/meta",
        "/work/workspaces/kind-iso-b",
        "/work/workspaces/kind-iso-b/r/private.txt",
    ] {
        let (code, out) = sh(&format!("test -e {hidden} || cat {hidden}")).await;
        assert_ne!(code, 0, "{hidden} is visible in the pod: {out}");
    }

    // Git works in the run's own worktree: the mirror is read, the worktree's own directory is written.
    let (code, out) = sh("cd /work/workspaces/kind-iso-a/r && git status --short --branch").await;
    assert_eq!(code, 0, "git status: {out}");
    assert!(out.contains("agent/kind-iso-a"), "{out}");
    let (code, out) = sh("cd /work/workspaces/kind-iso-a/r && echo more >> README.md && git status --short && git diff --stat | tail -1").await;
    assert_eq!(code, 0, "git status after a change: {out}");
    assert!(out.contains("README.md"), "{out}");
    let (code, out) = sh("cd /work/workspaces/kind-iso-a/r && git log --oneline").await;
    assert_eq!(code, 0, "git log: {out}");
    assert!(out.contains("seed"), "{out}");
    let (code, out) = sh(&format!(
        "touch {admin}/probe && echo y > /work/workspaces/kind-iso-a/y"
    ))
    .await;
    assert_eq!(code, 0, "the run's own directories are writable: {out}");

    // The mirror is read-only: its configuration, its hooks, its refs and its objects are the coder's.
    for write in [
        format!("echo x >> {mirror}/config"),
        format!("echo x > {mirror}/hooks/post-checkout"),
        format!("mkdir {mirror}/refs/heads/evil"),
        format!("mkdir {mirror}/objects/ff"),
        format!("mkdir {mirror}/worktrees/new"),
        "cd /work/workspaces/kind-iso-a/r && git commit -qam x".to_owned(),
        "cd /work/workspaces/kind-iso-a/r && git tag evil".to_owned(),
    ] {
        let (code, out) = sh(&write).await;
        assert_ne!(code, 0, "`{write}` wrote the mirror: {out}");
    }

    // What the coder's git follows out of the worktree's directory, or acts on, is read-only inside
    // it: a `HEAD` that names another run's branch is the branch the coder resets, a `commondir`
    // that names another repository moves the refs the coder reads and writes, a link at
    // `logs/HEAD` is appended to by git, and none is the pod's to write, rename over, replace or
    // remove, though the directory itself is writable and the files are its own uid's.
    for write in [
        format!("echo 'ref: refs/heads/agent/kind-iso-b' > {admin}/HEAD"),
        format!("echo 'ref: refs/heads/agent/kind-iso-b' >> {admin}/HEAD"),
        format!(
            "echo 'ref: refs/heads/agent/kind-iso-b' > {admin}/h2 && mv {admin}/h2 {admin}/HEAD"
        ),
        format!("rm {admin}/HEAD"),
        format!("ln -sf /etc/passwd {admin}/HEAD"),
        "cd /work/workspaces/kind-iso-a/r && git symbolic-ref HEAD refs/heads/agent/kind-iso-b"
            .to_owned(),
        "cd /work/workspaces/kind-iso-a/r && git switch --detach".to_owned(),
        format!("echo /elsewhere > {admin}/commondir"),
        format!("echo x >> {admin}/commondir"),
        format!("echo /elsewhere > {admin}/c2 && mv {admin}/c2 {admin}/commondir"),
        format!("rm {admin}/commondir"),
        format!("ln -sf /etc/passwd {admin}/commondir"),
        format!("echo /work/workspaces/kind-iso-b/r/.git > {admin}/gitdir"),
        format!("echo x > {admin}/g2 && mv {admin}/g2 {admin}/gitdir"),
        format!("rm {admin}/gitdir"),
        format!("mv {admin}/logs {admin}/logs.moved"),
        format!("rm {admin}/logs/HEAD && ln -s /etc/passwd {admin}/logs/HEAD"),
        format!("touch {admin}/logs/new"),
    ] {
        let (code, out) = sh(&write).await;
        assert_ne!(
            code, 0,
            "`{write}` changed the worktree's directory in the mirror: {out}"
        );
    }
    let (code, out) = sh(&format!("cat {admin}/commondir")).await;
    assert_eq!(
        (code, out.trim()),
        (0, "../.."),
        "commondir is what git wrote"
    );
    let (code, out) = sh(&format!("cat {admin}/HEAD")).await;
    assert_eq!(
        (code, out.trim()),
        (0, "ref: refs/heads/agent/kind-iso-a"),
        "HEAD is what git wrote"
    );
    let (code, out) = sh(&format!(
        "cd /work/workspaces/kind-iso-a/r && git status --short && git log --oneline | head -1 && touch {admin}/probe2"
    ))
    .await;
    assert_eq!(code, 0, "git works with the entries read-only: {out}");

    pods.delete(&pod_name(RUN), &DeleteParams::default())
        .await
        .unwrap();
    c.wait_for("the pod to be gone", 60, async || c.gone(RUN).await)
        .await;
}

async fn the_quota_refuses_a_pod_beyond_its_limit(c: &Cluster) {
    eprintln!("--- quota");
    let limit = c.quota_pods.unwrap();
    let env = c.environment(c.settings());
    let mut runs = Vec::new();
    for n in 0..limit {
        let run = c.run_id(&format!("q{n}"));
        let ws = c.workspaces().run(&run).unwrap();
        env.ensure(&ws, &NoProgress).await.expect("a slot");
        runs.push(run);
    }
    let over = c.run_id("qover");
    let ws = c.workspaces().run(&over).unwrap();
    let refused = env.ensure(&ws, &NoProgress).await.err().expect("no slot");
    assert!(
        matches!(&refused, EnvError::Unavailable(m) if m.contains("quota")),
        "{refused}"
    );
    assert!(c.gone(&over).await, "no pod was made");
    // A slot is freed: the same run now gets one.
    env.release(&runs[0]).await.unwrap();
    c.wait_for("the first pod to be gone", 60, async || {
        c.gone(&runs[0]).await
    })
    .await;
    env.ensure(&ws, &NoProgress).await.expect("a slot again");
    for run in runs.iter().skip(1).chain(std::iter::once(&over)) {
        env.release(run).await.unwrap();
    }
}
