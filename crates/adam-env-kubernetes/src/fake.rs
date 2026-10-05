//! A fake API server for the tests, and the tests of [`KubeEnvironment`](crate::KubeEnvironment)
//! that need one.
//!
//! The fake is a `tower-test` mock service behind a real [`kube::Client`], so everything the
//! environment sends (paths, methods, label selectors, bodies) goes through kube's own request
//! builders and parsers. It keeps pods in memory and answers `GET`, `POST`, `DELETE` and list for
//! `pods` of one namespace, with the answers a test turns on: a quota that refuses, a pod that takes
//! some polls to be ready, one that never schedules, an image that cannot be pulled. Commands in a
//! pod are another seam ([`PodExec`]) and have their own fake.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use adam_workspace::{
    EnvError, EnvProgress, EnvStep, EnvStepState, Environment, ExecId, ExecSpec, RunWorkspace,
    StaticToken, Workspaces,
};
use async_trait::async_trait;
use http::{Request, Response, StatusCode};
use k8s_openapi::api::core::v1::{
    ContainerState, ContainerStateWaiting, ContainerStatus, Pod, PodCondition, PodStatus,
};
use kube::Client;
use kube::client::Body;
use tower_test::mock;

use crate::exec::{Captured, ExecError, ExitStatus, PodExec};
use crate::template::tests::template;
use crate::{KubeEnvironment, Settings};

/// What the fake does, and what it saw.
#[derive(Default)]
pub(crate) struct State {
    pub(crate) pods: BTreeMap<String, Pod>,
    /// Every request, as `METHOD path?query`.
    pub(crate) requests: Vec<String>,
    /// Refuse the creation of a pod as the namespace's quota does.
    pub(crate) quota: bool,
    /// Refuse the creation of a pod with this status code and message.
    pub(crate) refuse: Option<(u16, String)>,
    /// How many times a new pod is read before it is ready.
    pub(crate) ready_after: u32,
    /// What the new pods' status becomes instead of ready (they never are).
    pub(crate) stuck: Option<PodStatus>,
    /// How many reads of a pod answer "not found" although it is there (a pod another worker made
    /// a moment ago, which this process has not seen).
    pub(crate) blind_gets: u32,
    /// Answer every request with 503.
    pub(crate) down: bool,
    /// Pods that were read, by name.
    reads: BTreeMap<String, u32>,
}

pub(crate) type Shared = Arc<StdMutex<State>>;

fn lock(state: &Shared) -> std::sync::MutexGuard<'_, State> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn json(code: StatusCode, value: &serde_json::Value) -> Response<Body> {
    Response::builder()
        .status(code)
        .header("content-type", "application/json")
        .body(Body::from(value.to_string().into_bytes()))
        .unwrap()
}

fn failure(code: u16, reason: &str, message: &str) -> Response<Body> {
    json(
        StatusCode::from_u16(code).unwrap(),
        &serde_json::json!({
            "kind": "Status", "apiVersion": "v1", "status": "Failure",
            "message": message, "reason": reason, "code": code
        }),
    )
}

fn ready_status() -> PodStatus {
    PodStatus {
        phase: Some("Running".to_owned()),
        conditions: Some(vec![PodCondition {
            type_: "Ready".to_owned(),
            status: "True".to_owned(),
            ..PodCondition::default()
        }]),
        ..PodStatus::default()
    }
}

/// A status of a pod that waits on an image it cannot pull.
pub(crate) fn pull_failure() -> PodStatus {
    PodStatus {
        phase: Some("Pending".to_owned()),
        container_statuses: Some(vec![ContainerStatus {
            name: "run".to_owned(),
            state: Some(ContainerState {
                waiting: Some(ContainerStateWaiting {
                    reason: Some("ImagePullBackOff".to_owned()),
                    message: Some("Back-off pulling image \"x\"".to_owned()),
                }),
                ..ContainerState::default()
            }),
            ..ContainerStatus::default()
        }]),
        ..PodStatus::default()
    }
}

/// A status of a pod no node has room for.
pub(crate) fn unschedulable() -> PodStatus {
    PodStatus {
        phase: Some("Pending".to_owned()),
        conditions: Some(vec![PodCondition {
            type_: "PodScheduled".to_owned(),
            status: "False".to_owned(),
            reason: Some("Unschedulable".to_owned()),
            message: Some("0/3 nodes are available: 3 Insufficient memory.".to_owned()),
            ..PodCondition::default()
        }]),
        ..PodStatus::default()
    }
}

fn matches_selector(pod: &Pod, selector: &str) -> bool {
    let labels = pod.metadata.labels.clone().unwrap_or_default();
    selector
        .split(',')
        .filter(|term| !term.is_empty())
        .all(|term| {
            term.split_once('=')
                .is_some_and(|(k, v)| labels.get(k).map(String::as_str) == Some(v))
        })
}

fn query_of(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| percent_decode(v))
    })
}

/// `application/x-www-form-urlencoded` decoding of a query value.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            byte => out.push(byte),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn answer(state: &Shared, request: Request<Body>) -> Response<Body> {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let query = request.uri().query().unwrap_or("").to_owned();
    let body = request
        .into_body()
        .collect_bytes()
        .await
        .unwrap_or_default();
    lock(state).requests.push(format!(
        "{method} {path}{}",
        if query.is_empty() {
            String::new()
        } else {
            format!("?{query}")
        }
    ));
    let Some(rest) = path.strip_prefix("/api/v1/namespaces/coder/pods") else {
        return failure(404, "NotFound", &format!("no such path {path}"));
    };
    let name = rest.strip_prefix('/');
    let mut st = lock(state);
    if st.down {
        return failure(503, "ServiceUnavailable", "the API server is down");
    }
    match (method.as_str(), name) {
        ("GET", None) => {
            let selector = query_of(&query, "labelSelector").unwrap_or_default();
            let items: Vec<&Pod> = st
                .pods
                .values()
                .filter(|pod| matches_selector(pod, &selector))
                .collect();
            json(
                StatusCode::OK,
                &serde_json::json!({
                    "kind": "PodList", "apiVersion": "v1", "metadata": {}, "items": items
                }),
            )
        }
        ("GET", Some(name)) if st.blind_gets > 0 => {
            st.blind_gets -= 1;
            failure(404, "NotFound", &format!("pods \"{name}\" not found"))
        }
        ("GET", Some(name)) => {
            let reads = {
                let n = st.reads.entry(name.to_owned()).or_insert(0);
                *n += 1;
                *n
            };
            let (ready_after, stuck) = (st.ready_after, st.stuck.clone());
            match st.pods.get_mut(name) {
                None => failure(404, "NotFound", &format!("pods \"{name}\" not found")),
                Some(pod) => {
                    // A pod that is already ended or going away keeps the status it was given.
                    if pod.status.is_none() {
                        pod.status = Some(match stuck {
                            Some(stuck) => stuck,
                            None if reads > ready_after => ready_status(),
                            None => PodStatus {
                                phase: Some("Pending".to_owned()),
                                ..PodStatus::default()
                            },
                        });
                    } else if let (None, Some(status)) = (&stuck, &mut pod.status)
                        && status.phase.as_deref() == Some("Pending")
                        && reads > ready_after
                    {
                        *status = ready_status();
                    }
                    json(StatusCode::OK, &serde_json::to_value(&*pod).unwrap())
                }
            }
        }
        ("POST", None) => {
            if st.quota {
                return failure(
                    403,
                    "Forbidden",
                    "pods \"x\" is forbidden: exceeded quota: run-pods, requested: pods=1, used: pods=4, limited: pods=4",
                );
            }
            if let Some((code, message)) = st.refuse.clone() {
                return failure(code, "Invalid", &message);
            }
            let pod: Pod = serde_json::from_slice(&body).unwrap();
            let name = pod.metadata.name.clone().unwrap();
            if st.pods.contains_key(&name) {
                return failure(
                    409,
                    "AlreadyExists",
                    &format!("pods \"{name}\" already exists"),
                );
            }
            let mut stored = pod;
            stored.status = None;
            st.pods.insert(name, stored.clone());
            json(StatusCode::CREATED, &serde_json::to_value(&stored).unwrap())
        }
        ("DELETE", Some(name)) => match st.pods.remove(name) {
            Some(pod) => json(StatusCode::OK, &serde_json::to_value(&pod).unwrap()),
            None => failure(404, "NotFound", &format!("pods \"{name}\" not found")),
        },
        _ => failure(405, "MethodNotAllowed", "not supported by the fake"),
    }
}

/// A client whose server is the fake: spawns its driver on the current runtime.
pub(crate) fn client() -> (Client, Shared) {
    let state: Shared = Arc::default();
    let (service, mut handle) = mock::pair::<Request<Body>, Response<Body>>();
    let driver = Arc::clone(&state);
    tokio::spawn(async move {
        while let Some((request, send)) = handle.next_request().await {
            send.send_response(answer(&driver, request).await);
        }
    });
    (Client::new(service, "coder"), state)
}

/// What the exec fake saw and what it answers.
#[derive(Default)]
pub(crate) struct ExecState {
    /// `pod: argv` of every command.
    pub(crate) commands: Vec<(String, Vec<String>)>,
    /// What `adam-exec active` answers: a count, or `None` to fail.
    pub(crate) active: Option<u32>,
    /// What every other command answers: its exit code.
    pub(crate) code: i32,
}

pub(crate) struct FakeExec(pub(crate) Arc<StdMutex<ExecState>>);

#[async_trait]
impl PodExec for FakeExec {
    async fn capture(
        &self,
        pod: &str,
        argv: Vec<String>,
        _timeout: Duration,
    ) -> Result<Captured, ExecError> {
        let mut state = self.0.lock().unwrap();
        state.commands.push((pod.to_owned(), argv.clone()));
        if argv.get(1).map(String::as_str) == Some("active") {
            return match state.active {
                Some(n) => Ok(Captured {
                    status: ExitStatus::Code(0),
                    stdout: format!("{n}\n"),
                    stderr: String::new(),
                }),
                None => Err(ExecError::Start("the pod is not there".to_owned())),
            };
        }
        Ok(Captured {
            status: ExitStatus::Code(state.code),
            stdout: String::new(),
            stderr: String::new(),
        })
    }
}

/// Collects the steps of an `ensure`.
#[derive(Default)]
pub(crate) struct Steps(pub(crate) StdMutex<Vec<EnvStep>>);

impl EnvProgress for Steps {
    fn step(&self, step: EnvStep) {
        self.0.lock().unwrap().push(step);
    }
}

pub(crate) const RUN: &str = "018f3a2b-7c1d-7000-8000-000000000001";
pub(crate) const OTHER: &str = "018f3a2b-7c1d-7000-8000-000000000002";

pub(crate) struct Fixture {
    pub(crate) env: KubeEnvironment,
    pub(crate) api: Shared,
    pub(crate) exec: Arc<StdMutex<ExecState>>,
    pub(crate) workspaces: Workspaces,
    _root: tempfile::TempDir,
}

pub(crate) fn settings() -> Settings {
    let mut settings = Settings::new("coder", "coder", "coder-0");
    settings.ready_poll = Duration::from_millis(5);
    settings.ready_timeout = Duration::from_millis(400);
    settings.exec_timeout = Duration::from_secs(1);
    settings.client_env =
        BTreeMap::from([("KUBERNETES_SERVICE_HOST".to_owned(), "10.0.0.1".to_owned())]);
    settings
}

pub(crate) fn fixture(settings: Settings) -> Fixture {
    let (client, api) = client();
    let exec = Arc::new(StdMutex::new(ExecState::default()));
    let pods = kube::Api::namespaced(client, "coder");
    let env = KubeEnvironment::over(
        pods,
        settings,
        template(),
        Arc::new(FakeExec(Arc::clone(&exec))),
    );
    let root = tempfile::tempdir().unwrap();
    let workspaces = Workspaces::new(
        root.path().to_owned(),
        Arc::new(StaticToken::new("unused-in-these-tests")),
    );
    Fixture {
        env,
        api,
        exec,
        workspaces,
        _root: root,
    }
}

impl Fixture {
    pub(crate) fn ws(&self, run: &str) -> RunWorkspace {
        self.workspaces.run(run).unwrap()
    }

    pub(crate) fn pod_names(&self) -> Vec<String> {
        lock(&self.api).pods.keys().cloned().collect()
    }

    pub(crate) fn requests(&self, prefix: &str) -> usize {
        lock(&self.api)
            .requests
            .iter()
            .filter(|r| r.starts_with(prefix))
            .count()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use adam_error::{Classify, ErrorClass};

    use crate::names::{pod_name, run_hash};

    #[tokio::test]
    async fn ensure_makes_the_pod_of_the_run_from_the_template_and_waits_for_it() {
        let fx = fixture(settings());
        lock(&fx.api).ready_after = 2;
        let steps = Steps::default();
        let session = fx.env.ensure(&fx.ws(RUN), &steps).await.unwrap();

        let name = pod_name(RUN);
        assert_eq!(fx.pod_names(), std::slice::from_ref(&name));
        let pod = lock(&fx.api).pods[&name].clone();
        let labels = pod.metadata.labels.unwrap();
        assert_eq!(labels["app.kubernetes.io/managed-by"], "adam-coder");
        assert_eq!(labels["app.kubernetes.io/instance"], "coder");
        assert_eq!(labels["adam.vymalo.com/run"], run_hash(RUN));
        let annotations = pod.metadata.annotations.unwrap();
        assert_eq!(annotations["adam.vymalo.com/run-id"], RUN);
        assert_eq!(annotations["adam.vymalo.com/worker"], "coder-0");
        // The template's own spec: image, resources, priority class.
        let spec = pod.spec.unwrap();
        assert_eq!(spec.priority_class_name.as_deref(), Some("coder-run"));
        assert_eq!(
            spec.containers[0]
                .resources
                .as_ref()
                .unwrap()
                .limits
                .as_ref()
                .unwrap()["memory"]
                .0,
            "2Gi"
        );

        let described = session.describe();
        assert!(described.summary.contains(&name), "{}", described.summary);
        assert!(matches!(
            described.kind,
            adam_workspace::EnvKind::Kubernetes { .. }
        ));

        // The steps: requested, then what it waited for, then ready.
        let steps = steps.0.lock().unwrap();
        assert!(steps.len() >= 2, "{steps:?}");
        assert!(steps.iter().all(|s| s.id == "run-pod"));
        assert_eq!(steps.first().unwrap().state, EnvStepState::Running);
        assert!(
            steps
                .first()
                .unwrap()
                .detail
                .as_deref()
                .unwrap()
                .contains("requesting")
        );
        let last = steps.last().unwrap();
        assert_eq!(last.state, EnvStepState::Completed);
        assert!(last.detail.as_deref().unwrap().contains("ready in"));
    }

    #[tokio::test]
    async fn ensure_twice_is_one_pod_and_the_second_says_nothing() {
        let fx = fixture(settings());
        let first = fx.env.ensure(&fx.ws(RUN), &Steps::default()).await.unwrap();
        let again = Steps::default();
        let second = fx.env.ensure(&fx.ws(RUN), &again).await.unwrap();
        assert_eq!(fx.pod_names().len(), 1);
        assert_eq!(fx.requests("POST"), 1, "the pod is made once");
        assert!(
            again.0.lock().unwrap().is_empty(),
            "a pod that is ready is reused with no step"
        );
        assert_eq!(first.describe(), second.describe());
    }

    #[tokio::test]
    async fn two_concurrent_ensures_of_a_run_make_one_pod() {
        let fx = fixture(settings());
        lock(&fx.api).ready_after = 3;
        let ws = fx.ws(RUN);
        let steps = Steps::default();
        let (a, b, c) = tokio::join!(
            fx.env.ensure(&ws, &steps),
            fx.env.ensure(&ws, &steps),
            fx.env.ensure(&ws, &steps),
        );
        a.unwrap();
        b.unwrap();
        c.unwrap();
        assert_eq!(fx.requests("POST"), 1);
        assert_eq!(fx.pod_names().len(), 1);
    }

    #[tokio::test]
    async fn a_pod_another_worker_made_is_reused_not_made_again() {
        let fx = fixture(settings());
        fx.env.ensure(&fx.ws(RUN), &Steps::default()).await.unwrap();
        let theirs = lock(&fx.api).pods[&pod_name(RUN)].clone();
        // The same pod, as a second environment over the same cluster (another worker) sees it.
        let other = fixture(settings());
        lock(&other.api).pods.insert(pod_name(RUN), {
            let mut pod = theirs;
            pod.status = Some(ready_status());
            pod
        });
        other
            .env
            .ensure(&other.ws(RUN), &Steps::default())
            .await
            .unwrap();
        assert_eq!(other.requests("POST"), 0);
    }

    #[tokio::test]
    async fn a_create_that_finds_the_pod_already_there_goes_on_with_it() {
        // This process looked a moment too early: the read said "not found", the create says
        // "already exists" (another worker, or a call whose future was dropped), and the pod is used.
        let fx = fixture(settings());
        fx.env.ensure(&fx.ws(RUN), &Steps::default()).await.unwrap();
        lock(&fx.api).blind_gets = 1;
        fx.env.ensure(&fx.ws(RUN), &Steps::default()).await.unwrap();
        assert_eq!(
            fx.requests("POST"),
            2,
            "the second create was refused as a duplicate"
        );
        assert_eq!(fx.pod_names().len(), 1);
    }

    #[tokio::test]
    async fn a_quota_is_unavailable_and_transient_and_makes_no_pod() {
        let fx = fixture(settings());
        lock(&fx.api).quota = true;
        let steps = Steps::default();
        let error = fx
            .env
            .ensure(&fx.ws(RUN), &steps)
            .await
            .err()
            .expect("quota");
        assert!(matches!(error, EnvError::Unavailable(_)), "{error}");
        assert_eq!(error.class(), ErrorClass::Transient);
        assert!(fx.pod_names().is_empty());
        {
            let steps = steps.0.lock().unwrap();
            let last = steps.last().unwrap();
            assert_eq!(last.state, EnvStepState::Failed);
            assert!(last.detail.as_deref().unwrap().contains("quota"));
        }
        // The quota passes: the next ensure works.
        lock(&fx.api).quota = false;
        fx.env.ensure(&fx.ws(RUN), &Steps::default()).await.unwrap();
    }

    #[tokio::test]
    async fn an_admission_refusal_is_refused_not_unavailable() {
        let fx = fixture(settings());
        lock(&fx.api).refuse = Some((
            422,
            "ValidatingAdmissionPolicy 'coder-run' with binding 'coder-run' denied request: the image is not allowed".to_owned(),
        ));
        let error = fx
            .env
            .ensure(&fx.ws(RUN), &Steps::default())
            .await
            .err()
            .expect("refused");
        assert!(matches!(error, EnvError::Refused(_)), "{error}");
        assert_eq!(error.class(), ErrorClass::Invalid);
        assert!(error.to_string().contains("denied request"));
    }

    #[tokio::test]
    async fn a_pod_that_never_gets_a_node_is_unavailable_and_deleted() {
        let fx = fixture(settings());
        lock(&fx.api).stuck = Some(unschedulable());
        let error = fx
            .env
            .ensure(&fx.ws(RUN), &Steps::default())
            .await
            .err()
            .expect("unschedulable");
        assert!(matches!(error, EnvError::Unavailable(_)), "{error}");
        assert!(error.to_string().contains("Insufficient memory"), "{error}");
        assert!(
            fx.pod_names().is_empty(),
            "a pod that cannot run is not left pending"
        );
    }

    #[tokio::test]
    async fn an_image_that_cannot_be_pulled_is_a_build_error_and_deleted() {
        let fx = fixture(settings());
        lock(&fx.api).stuck = Some(pull_failure());
        let error = fx
            .env
            .ensure(&fx.ws(RUN), &Steps::default())
            .await
            .err()
            .expect("pull");
        assert!(matches!(error, EnvError::Build { .. }), "{error}");
        assert_eq!(error.class(), ErrorClass::Invalid);
        assert!(error.to_string().contains("ImagePullBackOff"), "{error}");
        assert!(fx.pod_names().is_empty());
    }

    #[tokio::test]
    async fn a_pod_that_is_just_slow_is_a_timeout_and_stays() {
        let fx = fixture(settings());
        lock(&fx.api).stuck = Some(PodStatus {
            phase: Some("Pending".to_owned()),
            ..PodStatus::default()
        });
        let error = fx
            .env
            .ensure(&fx.ws(RUN), &Steps::default())
            .await
            .err()
            .expect("timeout");
        assert!(
            matches!(error, EnvError::Timeout { phase: "start", .. }),
            "{error}"
        );
        assert_eq!(
            fx.pod_names().len(),
            1,
            "it may still come up; the next ensure waits again"
        );
    }

    #[tokio::test]
    async fn a_pod_that_ended_is_replaced() {
        let fx = fixture(settings());
        fx.env.ensure(&fx.ws(RUN), &Steps::default()).await.unwrap();
        lock(&fx.api).pods.get_mut(&pod_name(RUN)).unwrap().status = Some(PodStatus {
            phase: Some("Failed".to_owned()),
            reason: Some("Evicted".to_owned()),
            ..PodStatus::default()
        });
        let steps = Steps::default();
        fx.env.ensure(&fx.ws(RUN), &steps).await.unwrap();
        assert_eq!(fx.requests("DELETE"), 1);
        assert_eq!(fx.requests("POST"), 2, "made again");
        assert!(
            steps
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|s| s.detail.as_deref().is_some_and(|d| d.contains("ended")))
        );
    }

    #[tokio::test]
    async fn a_name_that_collides_with_another_run_is_refused() {
        let fx = fixture(settings());
        fx.env.ensure(&fx.ws(RUN), &Steps::default()).await.unwrap();
        // Another run's pod under this run's name.
        lock(&fx.api)
            .pods
            .get_mut(&pod_name(RUN))
            .unwrap()
            .metadata
            .annotations
            .as_mut()
            .unwrap()
            .insert("adam.vymalo.com/run-id".to_owned(), OTHER.to_owned());
        let error = fx
            .env
            .ensure(&fx.ws(RUN), &Steps::default())
            .await
            .err()
            .expect("collision");
        assert!(matches!(error, EnvError::Refused(_)), "{error}");
    }

    #[tokio::test]
    async fn release_deletes_the_pod_and_is_idempotent() {
        let fx = fixture(settings());
        fx.env.ensure(&fx.ws(RUN), &Steps::default()).await.unwrap();
        fx.env
            .ensure(&fx.ws(OTHER), &Steps::default())
            .await
            .unwrap();
        fx.env.release(RUN).await.unwrap();
        assert_eq!(fx.pod_names(), [pod_name(OTHER)]);
        // Nothing held: not an error, any number of times.
        fx.env.release(RUN).await.unwrap();
        fx.env
            .release("018f3a2b-7c1d-7000-8000-0000000000ff")
            .await
            .unwrap();
        assert_eq!(fx.pod_names(), [pod_name(OTHER)]);
    }

    #[tokio::test]
    async fn rebuild_deletes_the_pod_so_that_the_next_ensure_makes_another() {
        let fx = fixture(settings());
        fx.env.ensure(&fx.ws(RUN), &Steps::default()).await.unwrap();
        assert!(fx.env.rebuild(RUN, false).await.unwrap());
        assert!(fx.pod_names().is_empty());
        fx.env.ensure(&fx.ws(RUN), &Steps::default()).await.unwrap();
        assert_eq!(fx.pod_names().len(), 1);
    }

    #[tokio::test]
    async fn held_runs_lists_the_run_ids_of_this_deployments_pods_only() {
        let fx = fixture(settings());
        fx.env
            .ensure(&fx.ws(OTHER), &Steps::default())
            .await
            .unwrap();
        fx.env.ensure(&fx.ws(RUN), &Steps::default()).await.unwrap();
        // A pod of another release, and a pod nobody labelled.
        {
            let mut st = lock(&fx.api);
            let mut foreign = st.pods[&pod_name(RUN)].clone();
            foreign.metadata.name = Some("adam-run-foreign".to_owned());
            foreign
                .metadata
                .labels
                .as_mut()
                .unwrap()
                .insert("app.kubernetes.io/instance".to_owned(), "other".to_owned());
            foreign
                .metadata
                .annotations
                .as_mut()
                .unwrap()
                .insert("adam.vymalo.com/run-id".to_owned(), "foreign".to_owned());
            st.pods.insert("adam-run-foreign".to_owned(), foreign);
            let mut bare = Pod::default();
            bare.metadata.name = Some("unrelated".to_owned());
            st.pods.insert("unrelated".to_owned(), bare);
        }
        let held = fx.env.held_runs().await.unwrap();
        assert_eq!(held, [RUN.to_owned(), OTHER.to_owned()]);
        // The listing asked for this deployment's label selector, not for everything.
        let list = lock(&fx.api)
            .requests
            .iter()
            .find(|r| r.starts_with("GET /api/v1/namespaces/coder/pods?"))
            .cloned()
            .unwrap();
        assert!(list.contains("labelSelector="), "{list}");
        assert!(list.contains("managed-by"), "{list}");
    }

    #[tokio::test]
    async fn a_cluster_that_is_down_is_unavailable_and_transient() {
        let fx = fixture(settings());
        lock(&fx.api).down = true;
        let error = fx.env.held_runs().await.unwrap_err();
        assert!(matches!(error, EnvError::Unavailable(_)), "{error}");
        assert_eq!(error.class(), ErrorClass::Transient);
        let error = fx
            .env
            .ensure(&fx.ws(RUN), &Steps::default())
            .await
            .err()
            .expect("down");
        assert!(matches!(error, EnvError::Unavailable(_)), "{error}");
        let error = fx.env.release(RUN).await.unwrap_err();
        assert!(matches!(error, EnvError::Unavailable(_)), "{error}");
    }

    // ----- the idle sweep -------------------------------------------------------------------------

    fn idle_settings(idle: Duration) -> Settings {
        let mut s = settings();
        s.idle = Some(idle);
        s
    }

    #[tokio::test]
    async fn an_idle_pod_is_deleted_and_the_next_ensure_makes_another() {
        let fx = fixture(idle_settings(Duration::from_millis(30)));
        fx.env.ensure(&fx.ws(RUN), &Steps::default()).await.unwrap();
        fx.exec.lock().unwrap().active = Some(0);
        // Used just now: kept.
        assert!(fx.env.reap_idle().await.unwrap().is_empty());
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(fx.env.reap_idle().await.unwrap(), [RUN.to_owned()]);
        assert!(fx.pod_names().is_empty());
        // The run comes back after its wait for a person: a new pod, the files are on the volume.
        fx.env.ensure(&fx.ws(RUN), &Steps::default()).await.unwrap();
        assert_eq!(fx.pod_names().len(), 1);
        // The sweep asked the pod whether it was busy before deleting it.
        let commands = fx.exec.lock().unwrap().commands.clone();
        assert_eq!(commands[0].0, pod_name(RUN));
        assert_eq!(
            commands[0].1,
            ["/opt/adam/bin/adam-exec", "active"].map(str::to_owned)
        );
    }

    #[tokio::test]
    async fn a_pod_that_runs_a_command_is_never_idle() {
        let fx = fixture(idle_settings(Duration::from_millis(20)));
        fx.env.ensure(&fx.ws(RUN), &Steps::default()).await.unwrap();
        fx.exec.lock().unwrap().active = Some(2);
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(fx.env.reap_idle().await.unwrap().is_empty());
        assert_eq!(fx.pod_names().len(), 1);
        // The clock restarted: it is not asked again until another idle period has passed.
        let asked = fx.exec.lock().unwrap().commands.len();
        assert!(fx.env.reap_idle().await.unwrap().is_empty());
        assert_eq!(fx.exec.lock().unwrap().commands.len(), asked);
    }

    #[tokio::test]
    async fn a_pod_that_cannot_be_asked_stays_for_the_next_sweep() {
        let fx = fixture(idle_settings(Duration::from_millis(20)));
        fx.env.ensure(&fx.ws(RUN), &Steps::default()).await.unwrap();
        fx.exec.lock().unwrap().active = None;
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(fx.env.reap_idle().await.unwrap().is_empty());
        assert_eq!(fx.pod_names().len(), 1);
    }

    #[tokio::test]
    async fn another_workers_pod_and_a_disabled_timeout_are_left_alone() {
        let fx = fixture(idle_settings(Duration::from_millis(10)));
        fx.env.ensure(&fx.ws(RUN), &Steps::default()).await.unwrap();
        lock(&fx.api)
            .pods
            .get_mut(&pod_name(RUN))
            .unwrap()
            .metadata
            .annotations
            .as_mut()
            .unwrap()
            .insert("adam.vymalo.com/worker".to_owned(), "coder-1".to_owned());
        fx.exec.lock().unwrap().active = Some(0);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(fx.env.reap_idle().await.unwrap().is_empty());

        let mut never = settings();
        never.idle = None;
        let fx = fixture(never);
        fx.env.ensure(&fx.ws(RUN), &Steps::default()).await.unwrap();
        assert!(fx.env.reap_idle().await.unwrap().is_empty());
        assert_eq!(fx.requests("GET /api/v1/namespaces/coder/pods?"), 0);
    }

    // ----- the session ----------------------------------------------------------------------------

    #[tokio::test]
    async fn prepare_makes_adam_kube_exec_with_the_pod_the_cwd_and_the_words() {
        let fx = fixture(settings());
        let ws = fx.ws(RUN);
        let session = fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        let cwd = ws.path().join("app");
        let spec = ExecSpec::shell("cargo test -- --nocapture 'a b'", &cwd)
            .env("CI", "1")
            .hide(["MODEL_API_KEY"]);
        let prepared = session.prepare(&spec).unwrap();

        assert_eq!(prepared.program, Path::new("adam-kube-exec"));
        assert_eq!(prepared.cwd, cwd);
        assert!(
            prepared.env_clear,
            "the client starts with nothing of the coder's"
        );
        assert_eq!(prepared.env.len(), 1);
        assert!(
            prepared
                .env
                .contains_key(std::ffi::OsStr::new("KUBERNETES_SERVICE_HOST"))
        );
        assert!(prepared.env_remove.is_empty());

        let args: Vec<String> = prepared
            .args
            .iter()
            .map(|a| a.to_str().unwrap().to_owned())
            .collect();
        let parsed = crate::Invocation::parse(args.clone()).unwrap();
        assert_eq!(parsed.namespace, "coder");
        assert_eq!(parsed.pod, pod_name(RUN));
        assert_eq!(parsed.container.as_deref(), Some("run"));
        assert_eq!(parsed.cwd, cwd.to_str().unwrap());
        assert_eq!(parsed.mode, crate::Mode::Shell);
        assert_eq!(parsed.words, [":cargo test -- --nocapture 'a b'"]);
        assert_eq!(parsed.unset, ["MODEL_API_KEY"]);
        assert!(parsed.env.contains(&("CI".to_owned(), "1".to_owned())));
        assert!(
            parsed
                .env
                .contains(&("GIT_CONFIG_KEY_0".to_owned(), "safe.directory".to_owned()))
        );
        assert_eq!(prepared.exec.as_str(), parsed.id);
        assert!(crate::is_exec_id(prepared.exec.as_str()));
        // No secret is anywhere in the command line: the model key is the pod's own variable.
        assert!(!args.iter().any(|a| a.contains("MODEL_API_KEY=")));
    }

    #[tokio::test]
    async fn prepare_quotes_nothing_it_passes_every_word_as_its_own_argument() {
        let fx = fixture(settings());
        let ws = fx.ws(RUN);
        let session = fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        let spec = ExecSpec::argv(
            [
                "git",
                "commit",
                "-m",
                "it's \"quoted\"; $(rm -rf /) `x`",
                "--",
            ],
            ws.path(),
        );
        let prepared = session.prepare(&spec).unwrap();
        let args: Vec<String> = prepared
            .args
            .iter()
            .map(|a| a.to_str().unwrap().to_owned())
            .collect();
        let parsed = crate::Invocation::parse(args).unwrap();
        assert_eq!(parsed.mode, crate::Mode::Run);
        assert_eq!(
            parsed.words,
            [
                ":git",
                ":commit",
                ":-m",
                ":it's \"quoted\"; $(rm -rf /) `x`",
                ":--"
            ]
        );
        // And the pod's command carries them as arguments of adam-exec, never through a shell.
        let remote = parsed.remote_argv();
        let at = remote
            .iter()
            .position(|w| w == "/opt/adam/bin/adam-exec")
            .unwrap();
        assert_eq!(remote[at + 1], "run");
        assert_eq!(remote.last().unwrap(), ":--");
    }

    #[tokio::test]
    async fn prepare_ids_are_unique_and_a_cwd_outside_the_run_is_refused() {
        let fx = fixture(settings());
        let ws = fx.ws(RUN);
        let session = fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        let a = session
            .prepare(&ExecSpec::shell("true", ws.path()))
            .unwrap();
        let b = session
            .prepare(&ExecSpec::shell("true", ws.path()))
            .unwrap();
        assert_ne!(a.exec, b.exec);
        for cwd in [
            Path::new("/work/workspaces/another-run"),
            Path::new("/etc"),
            Path::new("relative"),
        ] {
            let error = session.prepare(&ExecSpec::shell("true", cwd)).unwrap_err();
            assert!(matches!(error, EnvError::Refused(_)), "{error}");
        }
        let escape = ws.path().join("..").join("other");
        assert!(matches!(
            session
                .prepare(&ExecSpec::shell("true", escape))
                .unwrap_err(),
            EnvError::Refused(_)
        ));
        assert!(matches!(
            session
                .prepare(&ExecSpec::argv(Vec::<std::ffi::OsString>::new(), ws.path()))
                .unwrap_err(),
            EnvError::Refused(_)
        ));
        assert!(matches!(
            session
                .prepare(&ExecSpec::shell("true", ws.path()).env("BAD-NAME", "x"))
                .unwrap_err(),
            EnvError::Refused(_)
        ));
    }

    #[tokio::test]
    async fn kill_runs_adam_exec_kill_by_id_in_the_pod_and_never_fails() {
        let fx = fixture(settings());
        let session = fx.env.ensure(&fx.ws(RUN), &Steps::default()).await.unwrap();
        session.kill(&ExecId::new("kp-1-7")).await;
        let commands = fx.exec.lock().unwrap().commands.clone();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].0, pod_name(RUN));
        assert_eq!(
            commands[0].1,
            ["/opt/adam/bin/adam-exec", "kill", "kp-1-7"].map(str::to_owned)
        );
        // A failing kill is logged, not raised.
        fx.exec.lock().unwrap().code = 3;
        session.kill(&ExecId::new("kp-1-8")).await;
        fx.exec.lock().unwrap().active = None;
        session.kill(&ExecId::new("kp-1-9")).await;
    }

    #[tokio::test]
    async fn opencode_and_the_model_key_are_where_the_template_puts_them() {
        let fx = fixture(settings());
        let session = fx.env.ensure(&fx.ws(RUN), &Steps::default()).await.unwrap();
        assert_eq!(
            session.tool_path("opencode"),
            Some("/opt/adam/bin/opencode".into())
        );
        assert_eq!(session.tool_path("git"), None);
        assert_eq!(
            session.secret_ref("model-key"),
            Some(adam_workspace::SecretRef::Env("MODEL_API_KEY".to_owned()))
        );
        assert_eq!(session.secret_ref("github-token"), None);

        let mut bare = settings();
        bare.opencode = false;
        bare.model_key = false;
        let fx = fixture(bare);
        let session = fx.env.ensure(&fx.ws(RUN), &Steps::default()).await.unwrap();
        assert_eq!(session.tool_path("opencode"), None);
        assert_eq!(session.secret_ref("model-key"), None);
    }
}
