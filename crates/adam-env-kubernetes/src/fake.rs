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
use kube::api::WatchEvent;
use kube::client::Body;
use tower_test::mock;

use crate::environment::is_ready;
use crate::exec::{Captured, ExecError, ExitStatus, PodExec};
use crate::template::tests::template_at;
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
    /// Answer watch requests with 503, as an API that does not serve them.
    pub(crate) watch_down: bool,
    /// Answer a watch with an `ERROR` event (a version the server no longer has, or a 5xx).
    pub(crate) watch_error: bool,
    /// How long a watch answer waits before it is sent: a pod that takes time to come up.
    pub(crate) watch_delay: Duration,
    /// The fake's `resourceVersion` counter: a status that changes is a new version.
    version: u64,
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

/// Read a pod the way the API would: the fake's clock is a count of observations (`reads`), and a
/// pod with no status yet is given one. Both `GET` and a watch event observe. A status that
/// actually changes is a new `resourceVersion`, as it is on the cluster.
fn observe(st: &mut State, name: &str) -> Option<Pod> {
    let reads = {
        let n = st.reads.entry(name.to_owned()).or_insert(0);
        *n += 1;
        *n
    };
    let (ready_after, stuck) = (st.ready_after, st.stuck.clone());
    let pod = st.pods.get_mut(name)?;
    let mut changed = false;
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
        changed = true;
    } else if let (None, Some(status)) = (&stuck, &mut pod.status)
        && status.phase.as_deref() == Some("Pending")
        && reads > ready_after
    {
        *status = ready_status();
        changed = true;
    }
    if changed {
        st.version += 1;
        pod.metadata.resource_version = Some(st.version.to_string());
    }
    Some(pod.clone())
}

/// The watch event the fake answers with when a test asks for an API error (a version the server
/// no longer has, or a 5xx).
fn watch_error_event() -> serde_json::Value {
    serde_json::json!({
        "type": "ERROR",
        "object": {
            "kind": "Status", "apiVersion": "v1", "status": "Failure",
            "message": "too old resource version: 1 (2)", "reason": "Expired", "code": 410
        }
    })
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
    // A watch answer may be made to wait, for a pod that takes its time to come up. The delay is
    // read before the lock is taken for the answer: a guard must not be held across an await.
    if method.as_str() == "GET" && query_of(&query, "watch").as_deref() == Some("true") {
        let delay = lock(state).watch_delay;
        tokio::time::sleep(delay).await;
    }
    let mut st = lock(state);
    if st.down {
        return failure(503, "ServiceUnavailable", "the API server is down");
    }
    match (method.as_str(), name) {
        // A watch of the collection, narrowed by `metadata.name` (which is how `Api::watch` asks
        // for one object). The pod is the only one a field selector can name here.
        ("GET", None) if query_of(&query, "watch").as_deref() == Some("true") => {
            if st.watch_down {
                return failure(503, "ServiceUnavailable", "watching pods is not allowed");
            }
            if st.watch_error {
                let body = format!("{}\n", watch_error_event());
                return Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "application/json")
                    .body(Body::from(body.into_bytes()))
                    .unwrap();
            }
            let Some(name) = query_of(&query, "fieldSelector")
                .and_then(|selector| selector.strip_prefix("metadata.name=").map(str::to_owned))
            else {
                return Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "application/json")
                    .body(Body::empty())
                    .unwrap();
            };
            let Some(pod) = st.pods.get(&name).cloned() else {
                // Gone: the client reads it, finds nothing and makes it again.
                return Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "application/json")
                    .body(Body::empty())
                    .unwrap();
            };
            let requested = query_of(&query, "resourceVersion").unwrap_or_default();
            let current = pod.metadata.resource_version.clone().unwrap_or_default();
            let mut events: Vec<WatchEvent<Pod>> = Vec::new();
            if requested.is_empty() || requested == "0" {
                // What `resourceVersion=0` means: the state as it is now, as an `Added`, before
                // any change. It is not a change, and the fake's clock does not move for it.
                events.push(WatchEvent::Added(pod));
            } else if requested == current {
                // The client is up to date: a watch waits for the next change and then delivers
                // it. The fake's clock is its observations, so it moves the clock to that change
                // (bounded, so a pod that never changes ends the answer instead of looping).
                let mut budget = st.ready_after.saturating_add(1);
                while budget > 0 {
                    budget -= 1;
                    let Some(next) = observe(&mut st, &name) else {
                        break;
                    };
                    if next.metadata.resource_version != pod.metadata.resource_version {
                        events.push(WatchEvent::Modified(next));
                        break;
                    }
                }
            } else {
                // A version the pod has moved past: the state the client missed.
                events.push(WatchEvent::Modified(pod));
            }
            let lines: Vec<String> = events
                .iter()
                .map(|event| serde_json::to_string(event).unwrap())
                .collect();
            let body = format!("{}\n", lines.join("\n"));
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .body(Body::from(body.into_bytes()))
                .unwrap()
        }
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
        ("GET", Some(name)) => match observe(&mut st, name) {
            None => failure(404, "NotFound", &format!("pods \"{name}\" not found")),
            Some(pod) => json(StatusCode::OK, &serde_json::to_value(&pod).unwrap()),
        },
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
            // A real apiserver reports `Pending` on the create; the fake's first status is a change,
            // which is why the fixed client reads 3 times, not 2.
            stored.status = None;
            st.version += 1;
            stored.metadata.resource_version = Some(st.version.to_string());
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
        // One task per request, as an API server answers them: a watch that is made to wait (see
        // `State::watch_delay`) must not hold up the reads that come after it.
        while let Some((request, send)) = handle.next_request().await {
            let driver = Arc::clone(&driver);
            tokio::spawn(async move {
                send.send_response(answer(&driver, request).await);
            });
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
    /// How many of `active` are commands that outlive their call (`bg-` ids): `active-fg` leaves
    /// them out.
    pub(crate) background: u32,
    /// An `adam-exec` of an older image: it has no `active-fg` and answers with its usage line.
    pub(crate) without_active_fg: bool,
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
        if argv.get(1).map(String::as_str) == Some("active-fg") {
            if state.without_active_fg {
                return Ok(Captured {
                    status: ExitStatus::Code(2),
                    stdout: String::new(),
                    stderr: "adam-exec: usage: adam-exec run|shell|kill|chown|active ...\n"
                        .to_owned(),
                });
            }
            return match state.active {
                Some(n) => Ok(Captured {
                    status: ExitStatus::Code(0),
                    stdout: format!("{}\n", n.saturating_sub(state.background)),
                    stderr: String::new(),
                }),
                None => Err(ExecError::Start("the pod is not there".to_owned())),
            };
        }
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
    /// Where the repositories the runs add come from.
    remotes: tempfile::TempDir,
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
    // The workspace root is a directory of this machine; the template mounts the volume there.
    let root = tempfile::tempdir().unwrap();
    let workspaces = Workspaces::new(
        root.path().to_owned(),
        Arc::new(StaticToken::new("unused-in-these-tests")),
    );
    let env = KubeEnvironment::over(
        pods,
        settings,
        template_at(workspaces.root()),
        Arc::new(FakeExec(Arc::clone(&exec))),
    );
    Fixture {
        env,
        api,
        exec,
        workspaces,
        remotes: tempfile::tempdir().unwrap(),
        _root: root,
    }
}

/// `git` in `dir`, which must succeed; its output.
fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Seed")
        .env("GIT_AUTHOR_EMAIL", "seed@example.com")
        .env("GIT_COMMITTER_NAME", "Seed")
        .env("GIT_COMMITTER_EMAIL", "seed@example.com")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

impl Fixture {
    pub(crate) fn ws(&self, run: &str) -> RunWorkspace {
        self.workspaces.run(run).unwrap()
    }

    /// A local bare repository called `name` with a commit on `main`: what a slot is made from.
    pub(crate) fn remote(&self, name: &str) -> adam_workspace::RepoRef {
        let remote = self.remotes.path().join(format!("{name}.git"));
        let seed = self.remotes.path().join(format!("{name}-seed"));
        if remote.exists() {
            return adam_workspace::RepoRef::new(remote.to_str().unwrap(), "main");
        }
        std::fs::create_dir_all(&remote).unwrap();
        std::fs::create_dir_all(&seed).unwrap();
        git(
            &remote,
            &["init", "--bare", "--quiet", "--initial-branch=main"],
        );
        git(&seed, &["init", "--quiet", "--initial-branch=main"]);
        std::fs::write(seed.join("README.md"), "hi\n").unwrap();
        git(&seed, &["add", "-A"]);
        git(&seed, &["commit", "--quiet", "-m", "seed"]);
        git(
            &seed,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&seed, &["push", "--quiet", "origin", "main"]);
        adam_workspace::RepoRef::new(remote.to_str().unwrap(), "main")
    }

    /// The annotation of a pod, as the fake keeps it.
    pub(crate) fn annotation(&self, pod: &str, key: &str) -> Option<String> {
        lock(&self.api)
            .pods
            .get(pod)?
            .metadata
            .annotations
            .as_ref()?
            .get(key)
            .cloned()
    }

    /// `(mount path, sub path, read-only)` of the run container's mounts of the volume `work`.
    pub(crate) fn work_mounts(&self, pod: &str) -> Vec<(String, String, bool)> {
        let st = lock(&self.api);
        st.pods[pod]
            .spec
            .as_ref()
            .unwrap()
            .containers
            .iter()
            .find(|c| c.name == "run")
            .unwrap()
            .volume_mounts
            .iter()
            .flatten()
            .filter(|m| m.name == "work")
            .map(|m| {
                (
                    m.mount_path.clone(),
                    m.sub_path.clone().unwrap_or_default(),
                    m.read_only == Some(true),
                )
            })
            .collect()
    }

    pub(crate) fn pod_names(&self) -> Vec<String> {
        lock(&self.api).pods.keys().cloned().collect()
    }

    /// How many plain reads of `pod` the fake answered. A watch is not one: it carries its events.
    pub(crate) fn gets(&self, pod: &str) -> usize {
        let path = format!("GET /api/v1/namespaces/coder/pods/{pod}");
        lock(&self.api)
            .requests
            .iter()
            .filter(|r| **r == path)
            .count()
    }

    /// The list requests the fake answered (a watch of the collection is not one).
    pub(crate) fn lists(&self) -> usize {
        lock(&self.api)
            .requests
            .iter()
            .filter(|r| {
                r.starts_with("GET /api/v1/namespaces/coder/pods?") && !r.contains("watch=true")
            })
            .count()
    }

    /// The watch requests the fake answered.
    pub(crate) fn watches(&self) -> Vec<String> {
        lock(&self.api)
            .requests
            .iter()
            .filter(|r| r.contains("watch=true"))
            .cloned()
            .collect()
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
    use std::path::{Path, PathBuf};

    use super::*;
    use adam_error::{Classify, ErrorClass};

    use crate::names::{pod_name, run_hash};

    /// A log sink for the tests: what the environment logged at `warn` and above.
    #[derive(Clone, Default)]
    struct Capture(Arc<StdMutex<Vec<u8>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
        type Writer = Capture;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    impl Capture {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }

        fn count(&self, needle: &str) -> usize {
            self.text().matches(needle).count()
        }
    }

    /// Capture the `warn`-and-above logs for as long as the guard lives. The default subscriber is
    /// per thread, and the test's future stays on this one (a current-thread runtime).
    fn capture() -> (Capture, tracing::subscriber::DefaultGuard) {
        let capture = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .finish();
        (capture, tracing::subscriber::set_default(subscriber))
    }

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

    /// A pod that takes a while to come up is waited on with one watch of that pod, not a read per
    /// `ready_poll`: the fake's pod becomes ready after 10 observations, which polling would spend
    /// ten sleeps on and the watch delivers at once.
    #[tokio::test]
    async fn a_pod_that_takes_its_time_is_watched_not_read_over_and_over() {
        // Long enough for the old spin (one read per round trip) to be counted, not timed out.
        let mut settings = settings();
        settings.ready_timeout = Duration::from_secs(2);
        let fx = fixture(settings);
        lock(&fx.api).ready_after = 10;
        lock(&fx.api).watch_delay = Duration::from_millis(60);
        let started = std::time::Instant::now();
        fx.env.ensure(&fx.ws(RUN), &Steps::default()).await.unwrap();
        assert!(
            started.elapsed() >= Duration::from_millis(60),
            "the wait was real: {:?}",
            started.elapsed()
        );
        let name = pod_name(RUN);
        assert_eq!(
            fx.gets(&name),
            3,
            "not found, the pod's first status, then the one the watch said is ready (a real create \
             is already Pending; the fake's first status is a change, hence 3 and not 2): {:?}",
            lock(&fx.api).requests
        );
        let watches = fx.watches();
        assert_eq!(
            watches.len(),
            2,
            "the first status, then the change: {watches:?}"
        );
        assert!(
            watches
                .iter()
                .all(|w| w.contains("fieldSelector=metadata.name")),
            "one pod, by name: {watches:?}"
        );
        // The watch resumes from the version the read returned: `0` would replay the pod the
        // loop already has, and every replay would be another round trip.
        assert!(
            watches.iter().all(|w| !w.contains("resourceVersion=0")),
            "a version, never 0: {watches:?}"
        );
        assert!(
            watches[0].contains("resourceVersion=1"),
            "the create's version: {}",
            watches[0]
        );
        assert_eq!(fx.pod_names(), [name]);
    }

    /// An API that refuses the watch is the poll's, as it always was: the pod still comes up, and
    /// the refusal is reported once (a missing `watch` RBAC is then visible without a line every
    /// few seconds).
    #[tokio::test]
    async fn a_pod_comes_up_when_the_watch_is_refused() {
        let fx = fixture(settings());
        lock(&fx.api).ready_after = 2;
        lock(&fx.api).watch_down = true;
        let (log, _guard) = capture();
        fx.env.ensure(&fx.ws(RUN), &Steps::default()).await.unwrap();
        assert!(!fx.watches().is_empty(), "it tried the watch");
        let name = pod_name(RUN);
        assert_eq!(
            fx.gets(&name),
            3,
            "the reads the poll made: {:?}",
            lock(&fx.api).requests
        );
        assert_eq!(
            log.count("cannot be kept"),
            1,
            "one warning for two refusals: {}",
            log.text()
        );
    }

    /// An API that never answers the watch does not outlive `ready_timeout`: the wait is bounded
    /// by the same deadline as everything else.
    #[tokio::test]
    async fn a_watch_that_never_answers_ends_at_the_deadline() {
        let mut settings = settings();
        settings.ready_timeout = Duration::from_millis(200);
        let fx = fixture(settings);
        lock(&fx.api).ready_after = 100;
        lock(&fx.api).watch_delay = Duration::from_secs(30);
        let started = std::time::Instant::now();
        let error = fx
            .env
            .ensure(&fx.ws(RUN), &Steps::default())
            .await
            .err()
            .expect("the watch never answers");
        assert!(
            matches!(error, EnvError::Timeout { phase: "start", .. }),
            "{error}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "bounded by ready_timeout, not by the watch: {:?}",
            started.elapsed()
        );
    }

    /// An `ERROR` event (a version the server no longer has) is reported and the poll takes over:
    /// the pod still comes up, and the event is not mistaken for a change of the pod.
    #[tokio::test]
    async fn an_error_event_is_reported_and_the_poll_takes_over() {
        let fx = fixture(settings());
        lock(&fx.api).ready_after = 2;
        lock(&fx.api).watch_error = true;
        let (log, _guard) = capture();
        fx.env.ensure(&fx.ws(RUN), &Steps::default()).await.unwrap();
        assert!(!fx.watches().is_empty(), "it tried the watch");
        assert_eq!(
            fx.gets(&pod_name(RUN)),
            3,
            "the reads the poll made: {:?}",
            lock(&fx.api).requests
        );
        assert_eq!(
            log.count("cannot be kept"),
            1,
            "one warning for two error events: {}",
            log.text()
        );
    }

    /// The fake's watch protocol, which the client's loop is written against: `resourceVersion=0`
    /// replays the pod as it is now (one `Added`, no change), and the version a read returned gets
    /// the next change.
    #[tokio::test]
    async fn the_watch_replays_at_zero_and_delivers_the_next_change_from_a_version() {
        use futures::TryStreamExt as _;
        use kube::api::{PostParams, WatchParams};

        let (client, api) = client();
        lock(&api).ready_after = 3;
        let pods: kube::Api<Pod> = kube::Api::namespaced(client, "coder");
        let pod: Pod = serde_json::from_value(serde_json::json!({
            "apiVersion": "v1", "kind": "Pod", "metadata": {"name": "p"}
        }))
        .unwrap();
        pods.create(&PostParams::default(), &pod).await.unwrap();
        let read = pods.get("p").await.unwrap();
        let rv = read.metadata.resource_version.clone().unwrap();
        let at_read = read.status.clone();

        let params = WatchParams::default().fields("metadata.name=p").timeout(30);
        let at_zero = pods.watch(&params, "0").await.unwrap();
        tokio::pin!(at_zero);
        match at_zero.try_next().await.unwrap().unwrap() {
            WatchEvent::Added(added) => {
                assert_eq!(
                    added.metadata.resource_version.as_deref(),
                    Some(rv.as_str())
                );
                assert_eq!(added.status, at_read, "the replay is the pod as it is now");
            }
            other => panic!("expected Added, got {other:?}"),
        }
        let from_rv = pods.watch(&params, &rv).await.unwrap();
        tokio::pin!(from_rv);
        match from_rv.try_next().await.unwrap().unwrap() {
            WatchEvent::Modified(modified) => {
                assert!(
                    is_ready(&modified),
                    "the next change is the pod becoming ready"
                );
                assert_ne!(
                    modified.metadata.resource_version.as_deref(),
                    Some(rv.as_str()),
                    "a change is a new version"
                );
            }
            other => panic!("expected Modified, got {other:?}"),
        }
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
        // The pod another worker made a moment ago, as this process finds it: made from the same template
        // for the same workspace, and not yet seen here.
        let other = fixture(settings());
        let ws = other.ws(RUN);
        let mounts = other.env.inner.template.mounts_for(&ws).await.unwrap();
        let mut theirs = other.env.inner.template.render(
            RUN,
            &pod_name(RUN),
            crate::template::Placement {
                namespace: "coder",
                instance: "coder",
                worker: "coder-1",
            },
            &mounts,
            None,
        );
        theirs.status = Some(ready_status());
        lock(&other.api).pods.insert(pod_name(RUN), theirs);
        other.env.ensure(&ws, &Steps::default()).await.unwrap();
        assert_eq!(other.requests("POST"), 0);
        assert_eq!(other.requests("DELETE"), 0);
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

    // ----- the run's image (ADR 0052) -------------------------------------------------------------

    /// The image of the pod's run container.
    fn run_image(fx: &Fixture, pod: &str) -> Option<String> {
        lock(&fx.api).pods[pod]
            .spec
            .as_ref()
            .unwrap()
            .containers
            .iter()
            .find(|c| c.name == "run")
            .unwrap()
            .image
            .clone()
    }

    #[tokio::test]
    async fn a_run_that_chose_an_image_has_its_pod_made_again_with_it() {
        let fx = fixture(settings());
        let ws = fx.ws(RUN);
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        fx.exec.lock().unwrap().active = Some(0);
        assert_eq!(fx.requests("POST"), 1);
        let pod = pod_name(RUN);
        let template_image = run_image(&fx, &pod).unwrap();

        fx.env
            .set_image(RUN, Some("ghcr.io/acme/dev:1"))
            .await
            .unwrap();
        let steps = Steps::default();
        let session = fx.env.ensure(&ws, &steps).await.unwrap();

        assert_eq!(
            (fx.requests("POST"), fx.requests("DELETE")),
            (2, 1),
            "the old pod is deleted and the new one made"
        );
        assert_eq!(run_image(&fx, &pod).as_deref(), Some("ghcr.io/acme/dev:1"));
        assert_eq!(
            fx.annotation(&pod, "adam.vymalo.com/repository-image")
                .as_deref(),
            Some("ghcr.io/acme/dev:1"),
            "the pod records the choice"
        );
        // The init container keeps the deployment's image: it copies adam-exec.
        assert_eq!(
            lock(&fx.api).pods[&pod]
                .spec
                .as_ref()
                .unwrap()
                .init_containers
                .as_ref()
                .unwrap()[0]
                .image
                .as_deref(),
            Some("ghcr.io/vymalo/another-adam-rs/coder:sha-abc1234")
        );
        assert_eq!(
            session.describe().kind,
            adam_workspace::EnvKind::Kubernetes {
                pod: pod.clone(),
                image: Some("ghcr.io/acme/dev:1".to_owned()),
            }
        );
        assert!(steps.0.lock().unwrap().iter().any(|s| {
            s.detail.as_deref().is_some_and(|d| {
                d.contains("making the pod again") && d.contains("ghcr.io/acme/dev:1")
            })
        }));
        // The pod that was made is the pod now: nothing more to replace.
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        assert_eq!((fx.requests("POST"), fx.requests("DELETE")), (2, 1));
        assert_ne!(template_image, "ghcr.io/acme/dev:1");
    }

    #[tokio::test]
    async fn a_second_repository_image_replaces_the_pod_again() {
        let fx = fixture(settings());
        let ws = fx.ws(RUN);
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        fx.exec.lock().unwrap().active = Some(0);
        fx.env
            .set_image(RUN, Some("ghcr.io/acme/dev:1"))
            .await
            .unwrap();
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        fx.env
            .set_image(RUN, Some("ghcr.io/acme/dev:2"))
            .await
            .unwrap();
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        assert_eq!(
            (fx.requests("POST"), fx.requests("DELETE")),
            (3, 2),
            "the pod is made again for the second image too"
        );
        assert_eq!(
            run_image(&fx, &pod_name(RUN)).as_deref(),
            Some("ghcr.io/acme/dev:2")
        );
    }

    #[tokio::test]
    async fn going_back_to_the_deployment_image_replaces_the_pods_image_again() {
        let fx = fixture(settings());
        let ws = fx.ws(RUN);
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        fx.exec.lock().unwrap().active = Some(0);
        let pod = pod_name(RUN);
        let template_image = run_image(&fx, &pod).unwrap();
        fx.env
            .set_image(RUN, Some("ghcr.io/acme/dev:1"))
            .await
            .unwrap();
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        assert_eq!(fx.requests("DELETE"), 1);

        fx.env.set_image(RUN, None).await.unwrap();
        let session = fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        assert_eq!(
            (fx.requests("POST"), fx.requests("DELETE")),
            (3, 2),
            "made again, on the deployment's image"
        );
        assert_eq!(run_image(&fx, &pod), Some(template_image.clone()));
        assert_eq!(
            fx.annotation(&pod, "adam.vymalo.com/repository-image"),
            None,
            "no choice, no annotation"
        );
        assert_eq!(
            session.describe().kind,
            adam_workspace::EnvKind::Kubernetes {
                pod,
                image: Some(template_image),
            }
        );
    }

    #[tokio::test]
    async fn a_pod_that_runs_a_command_is_not_replaced_for_another_image() {
        let fx = fixture(settings());
        let ws = fx.ws(RUN);
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        let pod = pod_name(RUN);
        let template_image = run_image(&fx, &pod).unwrap();
        fx.env
            .set_image(RUN, Some("ghcr.io/acme/dev:1"))
            .await
            .unwrap();

        fx.exec.lock().unwrap().active = Some(1);
        let error = fx
            .env
            .ensure(&ws, &Steps::default())
            .await
            .err()
            .expect("a command runs");
        assert!(matches!(error, EnvError::Unavailable(_)), "{error}");
        assert_eq!(error.class(), ErrorClass::Transient);
        assert!(
            error.to_string().contains("command is running")
                && error.to_string().contains("ghcr.io/acme/dev:1"),
            "{error}"
        );
        assert_eq!(fx.requests("DELETE"), 0, "the command is not killed");
        assert_eq!(run_image(&fx, &pod), Some(template_image.clone()));

        // A pod that cannot be asked is waited for too.
        fx.exec.lock().unwrap().active = None;
        let error = fx
            .env
            .ensure(&ws, &Steps::default())
            .await
            .err()
            .expect("cannot ask");
        assert!(matches!(error, EnvError::Unavailable(_)), "{error}");
        assert_eq!(fx.requests("DELETE"), 0);

        // The command ends: the pod is made again, with the image.
        fx.exec.lock().unwrap().active = Some(0);
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        assert_eq!((fx.requests("POST"), fx.requests("DELETE")), (2, 1));
        assert_eq!(run_image(&fx, &pod).as_deref(), Some("ghcr.io/acme/dev:1"));
    }

    /// The pod's annotation is the durable record: a coder that restarts with nothing in its memory
    /// finds the pod another worker made and keeps the repository's image instead of replacing it
    /// with the deployment's.
    #[tokio::test]
    async fn a_pod_another_worker_made_with_a_repository_image_is_adopted() {
        let other = fixture(settings());
        let ws = other.ws(RUN);
        let mounts = other.env.inner.template.mounts_for(&ws).await.unwrap();
        let mut theirs = other.env.inner.template.render(
            RUN,
            &pod_name(RUN),
            crate::template::Placement {
                namespace: "coder",
                instance: "coder",
                worker: "coder-1",
            },
            &mounts,
            Some("ghcr.io/acme/dev:1"),
        );
        theirs.status = Some(ready_status());
        lock(&other.api).pods.insert(pod_name(RUN), theirs);

        let session = other.env.ensure(&ws, &Steps::default()).await.unwrap();
        assert_eq!(
            (other.requests("POST"), other.requests("DELETE")),
            (0, 0),
            "the pod is the choice: it is kept"
        );
        assert_eq!(
            session.describe().kind,
            adam_workspace::EnvKind::Kubernetes {
                pod: pod_name(RUN),
                image: Some("ghcr.io/acme/dev:1".to_owned()),
            }
        );
    }

    #[tokio::test]
    async fn an_empty_image_is_refused() {
        let fx = fixture(settings());
        let error = fx.env.set_image(RUN, Some("  ")).await.unwrap_err();
        assert!(matches!(error, EnvError::Refused(_)), "{error}");
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
            .find(|r| {
                r.starts_with("GET /api/v1/namespaces/coder/pods?") && !r.contains("watch=true")
            })
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

    /// ADR 0052: the image choice is the run's, not the pod's. The idle sweep deletes the pod, and
    /// the next command must make another with the same image, not the template's.
    #[tokio::test]
    async fn an_idle_pod_swept_keeps_the_runs_image_choice() {
        let fx = fixture(idle_settings(Duration::from_millis(30)));
        let ws = fx.ws(RUN);
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        fx.env
            .set_image(RUN, Some("ghcr.io/acme/dev:1"))
            .await
            .unwrap();
        fx.exec.lock().unwrap().active = Some(0);
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        let pod = pod_name(RUN);
        assert_eq!(run_image(&fx, &pod).as_deref(), Some("ghcr.io/acme/dev:1"));
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(fx.env.reap_idle().await.unwrap(), [RUN.to_owned()]);
        assert!(fx.pod_names().is_empty());
        // The run comes back: its repository image, not the deployment's.
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        assert_eq!(
            run_image(&fx, &pod).as_deref(),
            Some("ghcr.io/acme/dev:1"),
            "the choice is the run's and outlives its pod"
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
        assert_eq!(fx.lists(), 0, "no listing: a watch is not one");
    }

    // ----- the workspace mounts -------------------------------------------------------------------

    /// What the run container of the pod mounts of the volume `work`, as `(path, sub path, read-only)`
    /// for a path under the workspace root.
    fn expected(fx: &Fixture, mounts: &[(&Path, bool)]) -> Vec<(String, String, bool)> {
        let root = fx.workspaces.root();
        let mut all: Vec<(String, String, bool)> = mounts
            .iter()
            .map(|(path, read_only)| {
                (
                    path.display().to_string(),
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    *read_only,
                )
            })
            .collect();
        all.sort();
        all
    }

    /// The mounts of one worktree's git directory: the directory read-write, and on top of it what
    /// the pod may not change, read-only.
    fn admin_mounts(admin: &Path) -> Vec<(PathBuf, bool)> {
        let mut mounts = vec![(admin.to_owned(), false)];
        mounts
            .extend(["HEAD", "commondir", "gitdir", "logs"].map(|entry| (admin.join(entry), true)));
        mounts
    }

    /// The mounts of a run pod with the given slots (each a mount of its own, read-write) and
    /// worktrees (`(mirror, admin)`).
    fn expected_with(
        fx: &Fixture,
        run: &Path,
        slots: &[&Path],
        worktrees: &[(&Path, &Path)],
    ) -> Vec<(String, String, bool)> {
        let mut all: Vec<(PathBuf, bool)> = vec![(run.to_owned(), false)];
        all.extend(slots.iter().map(|slot| ((*slot).to_owned(), false)));
        let mut mirrors = std::collections::BTreeSet::new();
        for (mirror, admin) in worktrees {
            if mirrors.insert(mirror.to_owned()) {
                all.push(((*mirror).to_owned(), true));
            }
            all.extend(admin_mounts(admin));
        }
        let borrowed: Vec<(&Path, bool)> = all.iter().map(|(p, r)| (p.as_path(), *r)).collect();
        expected(fx, &borrowed)
    }

    fn sorted(mut mounts: Vec<(String, String, bool)>) -> Vec<(String, String, bool)> {
        mounts.sort();
        mounts
    }

    /// The directory of `slot` in its mirror's `worktrees/`, as the coder's metadata has it.
    fn admin_of(slot: &adam_workspace::Slot) -> std::path::PathBuf {
        slot.worktree().unwrap().git_dir().unwrap().to_owned()
    }

    #[tokio::test]
    async fn a_run_pod_mounts_the_runs_own_directory_and_none_of_the_rest_of_the_volume() {
        let fx = fixture(settings());
        let ws = fx.ws(RUN);
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        let pod = pod_name(RUN);
        assert_eq!(
            fx.work_mounts(&pod),
            expected(&fx, &[(ws.path(), false)]),
            "no mount of the whole volume"
        );
        assert!(
            ws.path().is_dir(),
            "the coder made the directory the pod mounts"
        );
        assert_eq!(
            fx.annotation(&pod, "adam.vymalo.com/workspace-mounts"),
            Some(format!("rw {}", ws.path().display()))
        );
        // The rest of the template is the deployment's: the cache and the tools are where it put them.
        let st = lock(&fx.api);
        let spec = st.pods[&pod].spec.as_ref().unwrap();
        let container = spec.containers.iter().find(|c| c.name == "run").unwrap();
        let others: Vec<_> = container
            .volume_mounts
            .iter()
            .flatten()
            .filter(|m| m.name != "work")
            .map(|m| (m.name.as_str(), m.mount_path.as_str()))
            .collect();
        assert_eq!(others, [("cache", "/cache"), ("tools", "/opt/adam/bin")]);
        assert_eq!(spec.volumes.as_ref().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn a_worktree_slot_adds_its_mirror_read_only_and_its_own_directory_in_it_read_write() {
        let fx = fixture(settings());
        let ws = fx.ws(RUN);
        let slot = ws.add_repository(&fx.remote("app")).await.unwrap();
        let mirror = slot.worktree().unwrap().mirror().to_owned();
        let admin = admin_of(&slot);
        assert!(admin.starts_with(mirror.join("worktrees")));
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        assert_eq!(
            sorted(fx.work_mounts(&pod_name(RUN))),
            expected_with(&fx, ws.path(), &[slot.path()], &[(&mirror, &admin)])
        );
        // What git follows out of the worktree's directory is read-only in it.
        let mounts = fx.work_mounts(&pod_name(RUN));
        for entry in ["HEAD", "commondir", "gitdir", "logs"] {
            let path = admin.join(entry).display().to_string();
            assert!(
                mounts.iter().any(|(p, _, ro)| *p == path && *ro),
                "{entry} is read-only: {mounts:?}"
            );
        }
        // Nothing of `meta/` and no other run is mounted.
        assert!(
            fx.work_mounts(&pod_name(RUN))
                .iter()
                .all(|(path, sub, _)| !sub.starts_with("meta") && !path.ends_with(OTHER))
        );
    }

    #[tokio::test]
    async fn several_worktrees_and_a_scratch_project_are_mounted_by_what_each_needs() {
        let fx = fixture(settings());
        let ws = fx.ws(RUN);
        let a = ws.add_repository(&fx.remote("alpha")).await.unwrap();
        let b = ws.add_repository(&fx.remote("beta")).await.unwrap();
        let notes = ws
            .add_scratch(
                "notes",
                &adam_workspace::GitIdentity::new("Adam", "a@example.com"),
            )
            .await
            .unwrap();
        // Another run's worktree of the same repository: its directory in the mirror is not this run's.
        let other = fx.ws(OTHER);
        let theirs = other.add_repository(&fx.remote("alpha")).await.unwrap();
        assert_ne!(admin_of(&a), admin_of(&theirs));

        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        let (ma, mb) = (
            a.worktree().unwrap().mirror().to_owned(),
            b.worktree().unwrap().mirror().to_owned(),
        );
        let (aa, ab) = (admin_of(&a), admin_of(&b));
        assert_eq!(
            sorted(fx.work_mounts(&pod_name(RUN))),
            expected_with(
                &fx,
                ws.path(),
                &[a.path(), b.path(), notes.path()],
                &[(&ma, &aa), (&mb, &ab)]
            ),
            "each slot is a mount of its own, the scratch project too, and it needs no git directory"
        );
        assert!(
            !fx.work_mounts(&pod_name(RUN))
                .iter()
                .any(|(path, _, _)| Path::new(path) == admin_of(&theirs)),
            "another run's worktree directory is not mounted read-write"
        );
    }

    /// A reference of a run's workspace is mounted read-only in the run's pod, with its directory in
    /// the mirror, and a repository slot beside it is not. A pod made before the reference was added is
    /// made again for it.
    #[tokio::test]
    async fn a_reference_is_mounted_read_only_and_a_pod_is_made_again_when_one_is_added() {
        let fx = fixture(settings());
        let ws = fx.ws(RUN);
        let app = ws.add_repository(&fx.remote("app")).await.unwrap();
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        fx.exec.lock().unwrap().active = Some(0);
        let before = fx.annotation(&pod_name(RUN), "adam.vymalo.com/workspace-mounts");

        let design = ws
            .add_reference(&fx.remote("design"), "design")
            .await
            .unwrap();
        assert!(!design.writable());
        // The stale pod is deleted by the seal, and the next command makes it with the new slot.
        fx.env.seal(&ws).await.unwrap();
        assert!(fx.pod_names().is_empty(), "the stale pod is deleted");
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        let after = fx.annotation(&pod_name(RUN), "adam.vymalo.com/workspace-mounts");
        assert_ne!(before, after, "the pod follows the new slot");

        let mounts = fx.work_mounts(&pod_name(RUN));
        let read_only = |path: &Path| {
            mounts
                .iter()
                .find(|(mounted, _, _)| Path::new(mounted) == path)
                .map(|(_, _, ro)| *ro)
        };
        let reference = design.reference().unwrap();
        assert_eq!(read_only(design.path()), Some(true), "the reference");
        assert_eq!(
            read_only(reference.git_dir().unwrap()),
            Some(true),
            "its directory in the mirror, whole"
        );
        assert_eq!(read_only(app.path()), Some(false), "the repository's slot");
        assert_eq!(
            read_only(app.worktree().unwrap().git_dir().unwrap()),
            Some(false),
            "its own directory in the mirror stays writable"
        );
        assert!(
            after
                .unwrap()
                .contains(&format!("ro {}", design.path().display())),
            "the annotation says so"
        );
    }

    /// A run that has a reference and no repository gets a pod: the reference is mounted read-only
    /// and a command runs in it.
    #[tokio::test]
    async fn a_run_with_only_a_reference_gets_a_pod_and_runs_commands_in_it() {
        let fx = fixture(settings());
        let ws = fx.ws(RUN);
        let design = ws
            .add_reference(&fx.remote("design"), "design")
            .await
            .unwrap();
        let session = fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        let mounts = fx.work_mounts(&pod_name(RUN));
        assert!(
            mounts
                .iter()
                .any(|(path, _, ro)| Path::new(path) == design.path() && *ro),
            "the reference is a read-only mount: {mounts:?}"
        );
        let prepared = session
            .prepare(&ExecSpec::shell("cat README.md", design.path()))
            .unwrap();
        let args: Vec<String> = prepared
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.iter()
                .any(|a| a.contains(&design.path().display().to_string())),
            "the command runs in the reference's directory: {args:?}"
        );
    }

    #[tokio::test]
    async fn the_legacy_worktree_of_a_run_is_outside_its_directory_and_gets_no_mount() {
        let fx = fixture(settings());
        let ws = fx.ws(RUN);
        let legacy = fx.workspaces.prepare(&fx.remote("old"), RUN).await.unwrap();
        assert!(!legacy.path().starts_with(ws.path()));
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        assert_eq!(
            fx.work_mounts(&pod_name(RUN)),
            expected(&fx, &[(ws.path(), false)])
        );
    }

    #[tokio::test]
    async fn a_run_that_rewrites_its_git_file_does_not_get_another_runs_directory_mounted() {
        let fx = fixture(settings());
        let (ws, other) = (fx.ws(RUN), fx.ws(OTHER));
        let mine = ws.add_repository(&fx.remote("app")).await.unwrap();
        let theirs = other.add_repository(&fx.remote("app")).await.unwrap();
        let (my_dir, their_dir) = (admin_of(&mine), admin_of(&theirs));
        assert_ne!(my_dir, their_dir);
        // The commands of this run write its `.git` file: it names the other run's directory.
        std::fs::write(
            mine.path().join(".git"),
            format!("gitdir: {}\n", their_dir.display()),
        )
        .unwrap();

        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        let mirror = mine.worktree().unwrap().mirror().to_owned();
        assert_eq!(
            sorted(fx.work_mounts(&pod_name(RUN))),
            expected_with(&fx, ws.path(), &[mine.path()], &[(&mirror, &my_dir)]),
            "the mounts come from the coder's metadata"
        );
        assert!(
            !fx.work_mounts(&pod_name(RUN))
                .iter()
                .any(|(path, _, _)| Path::new(path) == their_dir),
            "the other run's directory is not mounted"
        );
    }

    #[tokio::test]
    async fn a_slot_whose_directory_in_the_mirror_cannot_be_told_gets_no_git_mounts_and_the_run_goes_on()
     {
        let fx = fixture(settings());
        let ws = fx.ws(RUN);
        let slot = ws.add_repository(&fx.remote("app")).await.unwrap();
        let mirror = slot.worktree().unwrap().mirror().to_owned();
        let admin = admin_of(&slot);
        let meta = fx
            .workspaces
            .root()
            .join("meta")
            .join(RUN)
            .join(format!("{}.json", slot.dir()));
        let forget = || {
            let mut json: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&meta).unwrap()).unwrap();
            json.as_object_mut().unwrap().remove("admin");
            std::fs::write(&meta, serde_json::to_vec_pretty(&json).unwrap()).unwrap();
        };
        // The slot is its own mount whatever git can be told of it; its git directories are not.
        let only_the_run = expected(&fx, &[(ws.path(), false), (slot.path(), false)]);

        // A slot from before the directory was recorded, with two directories that name it.
        forget();
        let forged = mirror.join("worktrees/forged");
        std::fs::create_dir_all(&forged).unwrap();
        std::fs::write(
            forged.join("gitdir"),
            format!("{}\n", slot.path().join(".git").display()),
        )
        .unwrap();
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        assert_eq!(fx.work_mounts(&pod_name(RUN)), only_the_run, "ambiguous");

        // With none naming it.
        fx.env.release(RUN).await.unwrap();
        std::fs::remove_dir_all(&forged).unwrap();
        std::fs::remove_file(admin.join("gitdir")).unwrap();
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        assert_eq!(fx.work_mounts(&pod_name(RUN)), only_the_run, "absent");

        // With exactly one, the mirror's answer is taken (and recorded).
        fx.env.release(RUN).await.unwrap();
        std::fs::write(
            admin.join("gitdir"),
            format!("{}\n", slot.path().join(".git").display()),
        )
        .unwrap();
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        assert_eq!(
            sorted(fx.work_mounts(&pod_name(RUN))),
            expected_with(&fx, ws.path(), &[slot.path()], &[(&mirror, &admin)]),
            "unambiguous"
        );
    }

    #[tokio::test]
    async fn a_worktree_directory_whose_entries_are_not_what_git_wrote_gets_no_git_mounts() {
        let fx = fixture(settings());
        let ws = fx.ws(RUN);
        let slot = ws.add_repository(&fx.remote("app")).await.unwrap();
        let mirror = slot.worktree().unwrap().mirror().to_owned();
        let admin = admin_of(&slot);
        let only_the_run = expected(&fx, &[(ws.path(), false), (slot.path(), false)]);
        let whole = expected_with(&fx, ws.path(), &[slot.path()], &[(&mirror, &admin)]);

        // A link where a read-only file would be mounted: the mount would follow it to a place
        // the coder did not name.
        let elsewhere = fx.workspaces.root().join("elsewhere");
        std::fs::write(&elsewhere, "x").unwrap();
        for entry in ["HEAD", "commondir", "gitdir"] {
            let file = admin.join(entry);
            let kept = std::fs::read(&file).unwrap();
            std::fs::remove_file(&file).unwrap();
            std::os::unix::fs::symlink(&elsewhere, &file).unwrap();
            fx.env.ensure(&ws, &Steps::default()).await.unwrap();
            assert_eq!(
                fx.work_mounts(&pod_name(RUN)),
                only_the_run,
                "{entry} is a link"
            );
            fx.env.release(RUN).await.unwrap();
            std::fs::remove_file(&file).unwrap();
            std::fs::write(&file, kept).unwrap();
        }
        // The same for the directory of the reflog.
        let logs = admin.join("logs");
        std::fs::rename(&logs, admin.join("logs.moved")).unwrap();
        std::os::unix::fs::symlink(fx.workspaces.root(), &logs).unwrap();
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        assert_eq!(
            fx.work_mounts(&pod_name(RUN)),
            only_the_run,
            "logs is a link"
        );
        fx.env.release(RUN).await.unwrap();

        // A missing `logs` is made by the coder (the kubelet would make it as root), and then the
        // directory is mounted whole.
        std::fs::remove_file(&logs).unwrap();
        std::fs::remove_dir_all(admin.join("logs.moved")).unwrap();
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        assert!(logs.is_dir());
        assert_eq!(sorted(fx.work_mounts(&pod_name(RUN))), whole);
    }

    #[tokio::test]
    async fn a_workspace_outside_where_the_template_mounts_the_volume_is_refused() {
        let fx = fixture(settings());
        let elsewhere = tempfile::tempdir().unwrap();
        let other = Workspaces::new(
            elsewhere.path().to_owned(),
            Arc::new(StaticToken::new("unused-in-these-tests")),
        );
        let error = fx
            .env
            .ensure(&other.run(RUN).unwrap(), &Steps::default())
            .await
            .err()
            .expect("outside the volume");
        assert!(matches!(error, EnvError::Refused(_)), "{error}");
        assert!(error.to_string().contains("not below"), "{error}");
        assert!(fx.pod_names().is_empty(), "no pod was made");
    }

    #[tokio::test]
    async fn a_worktree_added_after_the_pod_exists_is_mounted_by_making_the_pod_again() {
        let fx = fixture(settings());
        let ws = fx.ws(RUN);
        let a = ws.add_repository(&fx.remote("alpha")).await.unwrap();
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        fx.exec.lock().unwrap().active = Some(0);

        // The workspace did not change: the pod is reused, and nothing is asked of it.
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        assert_eq!((fx.requests("POST"), fx.requests("DELETE")), (1, 0));
        assert!(fx.exec.lock().unwrap().commands.is_empty());

        let b = ws.add_repository(&fx.remote("beta")).await.unwrap();
        let steps = Steps::default();
        fx.env.ensure(&ws, &steps).await.unwrap();
        assert_eq!(
            (fx.requests("POST"), fx.requests("DELETE")),
            (2, 1),
            "made again"
        );
        let (ma, mb) = (
            a.worktree().unwrap().mirror().to_owned(),
            b.worktree().unwrap().mirror().to_owned(),
        );
        let (aa, ab) = (admin_of(&a), admin_of(&b));
        assert_eq!(
            sorted(fx.work_mounts(&pod_name(RUN))),
            expected_with(
                &fx,
                ws.path(),
                &[a.path(), b.path()],
                &[(&ma, &aa), (&mb, &ab)]
            )
        );
        // It asked the old pod whether a command ran before it was deleted, and said why: the
        // question leaves out the commands that outlive their call.
        let commands = fx.exec.lock().unwrap().commands.clone();
        assert_eq!(commands.len(), 1);
        assert_eq!(
            commands[0].1,
            ["/opt/adam/bin/adam-exec", "active-fg"].map(str::to_owned)
        );
        assert!(steps.0.lock().unwrap().iter().any(|s| {
            s.detail.as_deref().is_some_and(|d| {
                d.contains("making the pod again") && d.contains(&mb.display().to_string())
            })
        }));

        // And the new pod is the pod now: nothing more to change.
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        assert_eq!((fx.requests("POST"), fx.requests("DELETE")), (2, 1));
    }

    #[tokio::test]
    async fn a_slot_made_again_under_the_same_name_has_the_pod_made_again_because_its_mounts_follow_the_old_directory()
     {
        let fx = fixture(settings());
        let ws = fx.ws(RUN);
        let remote = fx.remote("alpha");
        let slot = ws.add_repository(&remote).await.unwrap();
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        fx.exec.lock().unwrap().active = Some(0);
        let pod = pod_name(RUN);
        let mounted = fx
            .annotation(&pod, "adam.vymalo.com/workspace-mounts")
            .unwrap();
        let admin = admin_of(&slot);
        assert!(
            mounted.contains(&format!("rw {} @", admin.display())),
            "the directory is recorded with its generation: {mounted}"
        );
        assert!(
            mounted.contains(&format!("rw {} @", slot.path().display())),
            "and so is the slot's own: {mounted}"
        );
        // Nothing changed: the pod is the pod.
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        assert_eq!((fx.requests("POST"), fx.requests("DELETE")), (1, 0));

        // The slot's directory is lost and the repository is added again: the same slot, the same
        // name of its directory in the mirror, and another directory under it.
        std::fs::remove_dir_all(slot.path()).unwrap();
        let again = ws.add_repository(&remote).await.unwrap();
        assert_eq!(admin_of(&again), admin, "the name is the same");
        assert!(admin.join("HEAD").is_file());

        let steps = Steps::default();
        fx.env.ensure(&ws, &steps).await.unwrap();
        assert_eq!(
            (fx.requests("POST"), fx.requests("DELETE")),
            (2, 1),
            "made again"
        );
        let remounted = fx
            .annotation(&pod, "adam.vymalo.com/workspace-mounts")
            .unwrap();
        assert_ne!(remounted, mounted, "the new pod records the new directory");
        assert!(steps.0.lock().unwrap().iter().any(|s| {
            s.detail.as_deref().is_some_and(|d| {
                d.contains("making the pod again") && d.contains(&admin.display().to_string())
            })
        }));

        // And that pod is the pod now.
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        assert_eq!((fx.requests("POST"), fx.requests("DELETE")), (2, 1));
    }

    fn me() -> adam_workspace::GitIdentity {
        adam_workspace::GitIdentity::new("Adam", "a@example.com")
    }

    #[tokio::test]
    async fn a_scratch_project_added_after_the_pod_exists_is_mounted_by_making_the_pod_again() {
        let fx = fixture(settings());
        let ws = fx.ws(RUN);
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        fx.exec.lock().unwrap().active = Some(0);
        assert_eq!((fx.requests("POST"), fx.requests("DELETE")), (1, 0));

        let notes = ws.add_scratch("notes", &me()).await.unwrap();
        let steps = Steps::default();
        fx.env.ensure(&ws, &steps).await.unwrap();
        assert_eq!(
            (fx.requests("POST"), fx.requests("DELETE")),
            (2, 1),
            "made again"
        );
        assert_eq!(
            fx.work_mounts(&pod_name(RUN)),
            expected(&fx, &[(ws.path(), false), (notes.path(), false)])
        );
        assert!(steps.0.lock().unwrap().iter().any(|s| {
            s.detail.as_deref().is_some_and(|d| {
                d.contains("making the pod again")
                    && d.contains(&notes.path().display().to_string())
            })
        }));
        let recorded = fx
            .annotation(&pod_name(RUN), "adam.vymalo.com/workspace-mounts")
            .unwrap();
        assert!(
            recorded.contains(&format!("rw {} @", notes.path().display())),
            "{recorded}"
        );
        // And that pod is the pod now.
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        assert_eq!((fx.requests("POST"), fx.requests("DELETE")), (2, 1));
    }

    #[tokio::test]
    async fn a_scratch_project_made_again_under_the_same_name_has_the_pod_made_again() {
        let fx = fixture(settings());
        let ws = fx.ws(RUN);
        let notes = ws.add_scratch("notes", &me()).await.unwrap();
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        fx.exec.lock().unwrap().active = Some(0);
        let before = fx
            .annotation(&pod_name(RUN), "adam.vymalo.com/workspace-mounts")
            .unwrap();
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        assert_eq!((fx.requests("POST"), fx.requests("DELETE")), (1, 0));

        // The directory is lost and the project is started again: the same name, another directory,
        // which the pod's mount of the old one does not follow.
        std::fs::remove_dir_all(notes.path()).unwrap();
        ws.add_scratch("notes", &me()).await.unwrap();
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        assert_eq!((fx.requests("POST"), fx.requests("DELETE")), (2, 1));
        let after = fx
            .annotation(&pod_name(RUN), "adam.vymalo.com/workspace-mounts")
            .unwrap();
        assert_ne!(after, before, "the new pod records the new directory");
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        assert_eq!((fx.requests("POST"), fx.requests("DELETE")), (2, 1));
    }

    #[tokio::test]
    async fn a_scratch_project_from_before_generations_keeps_the_pod_made_for_it() {
        let fx = fixture(settings());
        let ws = fx.ws(RUN);
        let notes = ws.add_scratch("notes", &me()).await.unwrap();
        // Metadata written before the generation was recorded.
        let meta = fx
            .workspaces
            .root()
            .join("meta")
            .join(RUN)
            .join("notes.json");
        let mut json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&meta).unwrap()).unwrap();
        json.as_object_mut().unwrap().remove("dir_gen");
        std::fs::write(&meta, serde_json::to_vec(&json).unwrap()).unwrap();

        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        fx.exec.lock().unwrap().active = Some(0);
        let recorded = fx
            .annotation(&pod_name(RUN), "adam.vymalo.com/workspace-mounts")
            .unwrap();
        assert!(
            recorded
                .lines()
                .any(|line| line == format!("rw {}", notes.path().display())),
            "the line it always had: {recorded}"
        );
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        assert_eq!(
            (fx.requests("POST"), fx.requests("DELETE")),
            (1, 0),
            "not made again for nothing"
        );
    }

    #[tokio::test]
    async fn a_pod_made_before_slots_were_mounted_is_made_again_for_them() {
        let fx = fixture(settings());
        let ws = fx.ws(RUN);
        let slot = ws.add_repository(&fx.remote("alpha")).await.unwrap();
        let notes = ws.add_scratch("notes", &me()).await.unwrap();
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        fx.exec.lock().unwrap().active = Some(0);
        // What an earlier version recorded: the run's directory and the git directories, no slot.
        let pod = pod_name(RUN);
        {
            let mut st = lock(&fx.api);
            let annotations = st
                .pods
                .get_mut(&pod)
                .unwrap()
                .metadata
                .annotations
                .as_mut()
                .unwrap();
            let kept: Vec<String> = annotations["adam.vymalo.com/workspace-mounts"]
                .lines()
                .filter(|line| {
                    !line.contains(&slot.path().display().to_string())
                        && !line.contains(&notes.path().display().to_string())
                })
                .map(str::to_owned)
                .collect();
            annotations.insert(
                "adam.vymalo.com/workspace-mounts".to_owned(),
                kept.join("\n"),
            );
        }
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        assert_eq!((fx.requests("POST"), fx.requests("DELETE")), (2, 1));
        assert!(
            fx.work_mounts(&pod)
                .iter()
                .any(|(path, _, _)| Path::new(path) == slot.path())
        );
    }

    #[tokio::test]
    async fn a_pod_that_runs_a_command_is_waited_for_not_deleted() {
        let fx = fixture(settings());
        let ws = fx.ws(RUN);
        ws.add_repository(&fx.remote("alpha")).await.unwrap();
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        ws.add_repository(&fx.remote("beta")).await.unwrap();

        fx.exec.lock().unwrap().active = Some(1);
        let error = fx
            .env
            .ensure(&ws, &Steps::default())
            .await
            .err()
            .expect("a command runs");
        assert!(matches!(error, EnvError::Unavailable(_)), "{error}");
        assert_eq!(error.class(), ErrorClass::Transient, "the caller waits");
        assert!(error.to_string().contains("command is running"), "{error}");
        assert_eq!(fx.requests("DELETE"), 0, "the command is not killed");
        assert_eq!(fx.pod_names().len(), 1);

        // A pod that cannot be asked is waited for too.
        fx.exec.lock().unwrap().active = None;
        let error = fx
            .env
            .ensure(&ws, &Steps::default())
            .await
            .err()
            .expect("cannot ask");
        assert!(matches!(error, EnvError::Unavailable(_)), "{error}");
        assert_eq!(fx.requests("DELETE"), 0);

        // The command ends: the pod is made again.
        fx.exec.lock().unwrap().active = Some(0);
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        assert_eq!((fx.requests("POST"), fx.requests("DELETE")), (2, 1));
    }

    /// A development server a run started to keep running is not a command that holds the pod: a new
    /// slot makes the pod again, and the server ends with it.
    #[tokio::test]
    async fn a_new_mount_deletes_a_pod_with_only_background_commands() {
        let fx = fixture(settings());
        let ws = fx.ws(RUN);
        ws.add_repository(&fx.remote("alpha")).await.unwrap();
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        ws.add_repository(&fx.remote("beta")).await.unwrap();

        // Two live records, both of them `bg-`: nothing the run waits for.
        {
            let mut exec = fx.exec.lock().unwrap();
            exec.active = Some(2);
            exec.background = 2;
        }
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        assert_eq!((fx.requests("POST"), fx.requests("DELETE")), (2, 1));
        let commands = fx.exec.lock().unwrap().commands.clone();
        assert_eq!(
            commands.last().map(|(_, argv)| argv.clone()),
            Some(
                ["/opt/adam/bin/adam-exec", "active-fg"]
                    .map(str::to_owned)
                    .to_vec()
            ),
            "the question for a new slot leaves the background commands out"
        );

        // A command that is not one of them still holds the pod.
        ws.add_repository(&fx.remote("gamma")).await.unwrap();
        {
            let mut exec = fx.exec.lock().unwrap();
            exec.active = Some(2);
            exec.background = 1;
        }
        let error = fx
            .env
            .ensure(&ws, &Steps::default())
            .await
            .err()
            .expect("a foreground command runs");
        assert!(error.to_string().contains("command is running"), "{error}");
        assert_eq!(fx.requests("DELETE"), 1);
    }

    /// The idle sweep asks `active`, which counts every command: a pod with a development server in
    /// it is never idle.
    #[tokio::test]
    async fn the_idle_sweep_spares_a_pod_with_a_background_command() {
        let fx = fixture(idle_settings(Duration::from_millis(20)));
        fx.env.ensure(&fx.ws(RUN), &Steps::default()).await.unwrap();
        {
            let mut exec = fx.exec.lock().unwrap();
            exec.active = Some(1);
            exec.background = 1;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(fx.env.reap_idle().await.unwrap().is_empty());
        assert_eq!(fx.pod_names().len(), 1);
        let commands = fx.exec.lock().unwrap().commands.clone();
        assert_eq!(
            commands.last().map(|(_, argv)| argv.clone()),
            Some(
                ["/opt/adam/bin/adam-exec", "active"]
                    .map(str::to_owned)
                    .to_vec()
            )
        );
    }

    /// A pod made by an older image has an `adam-exec` that does not know `active-fg`: the pod is
    /// asked `active` instead, and behaves as it did before (every command holds the pod).
    #[tokio::test]
    async fn an_adam_exec_without_active_fg_waits_as_before() {
        let fx = fixture(settings());
        let ws = fx.ws(RUN);
        ws.add_repository(&fx.remote("alpha")).await.unwrap();
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        ws.add_repository(&fx.remote("beta")).await.unwrap();
        {
            let mut exec = fx.exec.lock().unwrap();
            exec.active = Some(1);
            exec.without_active_fg = true;
        }
        let error = fx
            .env
            .ensure(&ws, &Steps::default())
            .await
            .err()
            .expect("a command runs");
        assert!(matches!(error, EnvError::Unavailable(_)), "{error}");
        assert_eq!(error.class(), ErrorClass::Transient);
        assert!(error.to_string().contains("command is running"), "{error}");
        assert_eq!(fx.requests("DELETE"), 0, "the pod is not deleted unasked");
        // The same older pod with nothing running is made again, as it was before `active-fg`.
        fx.exec.lock().unwrap().active = Some(0);
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        assert_eq!((fx.requests("POST"), fx.requests("DELETE")), (2, 1));
        let asked: Vec<String> = fx
            .exec
            .lock()
            .unwrap()
            .commands
            .iter()
            .map(|(_, argv)| argv[1].clone())
            .collect();
        assert_eq!(asked, ["active-fg", "active", "active-fg", "active"]);
    }

    #[tokio::test]
    async fn a_pod_made_before_mounts_were_recorded_is_made_again_and_a_spare_mount_is_harmless() {
        let fx = fixture(settings());
        let ws = fx.ws(RUN);
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        fx.exec.lock().unwrap().active = Some(0);
        let pod = pod_name(RUN);

        // A mount the workspace no longer needs (a slot that went away) is no reason to make a new pod.
        lock(&fx.api)
            .pods
            .get_mut(&pod)
            .unwrap()
            .metadata
            .annotations
            .as_mut()
            .unwrap()
            .entry("adam.vymalo.com/workspace-mounts".to_owned())
            .and_modify(|v| v.push_str("\nrw /work/git/gone"));
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        assert_eq!(fx.requests("DELETE"), 0);

        // A pod of an earlier version has no annotation, and mounted the whole volume.
        lock(&fx.api)
            .pods
            .get_mut(&pod)
            .unwrap()
            .metadata
            .annotations
            .as_mut()
            .unwrap()
            .remove("adam.vymalo.com/workspace-mounts");
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        assert_eq!((fx.requests("POST"), fx.requests("DELETE")), (2, 1));
        assert!(
            fx.annotation(&pod, "adam.vymalo.com/workspace-mounts")
                .is_some()
        );
    }

    // ----- seal ---------------------------------------------------------------------------------

    #[tokio::test]
    async fn seal_deletes_a_pod_that_lacks_a_slot_when_no_command_runs_in_it() {
        let fx = fixture(settings());
        let ws = fx.ws(RUN);
        ws.add_repository(&fx.remote("alpha")).await.unwrap();
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        fx.exec.lock().unwrap().active = Some(0);
        let pod = pod_name(RUN);

        // Each kind of slot, right after it was added.
        for slot in ["worktree", "scratch"] {
            if slot == "worktree" {
                ws.add_repository(&fx.remote("beta")).await.unwrap();
            } else {
                ws.add_scratch("notes", &me()).await.unwrap();
            }
            let (posts, deletes) = (fx.requests("POST"), fx.requests("DELETE"));
            fx.env.seal(&ws).await.unwrap();
            assert_eq!(
                fx.requests("DELETE"),
                deletes + 1,
                "{slot}: the stale pod is deleted"
            );
            assert_eq!(fx.requests("POST"), posts, "{slot}: and none is made");
            assert!(fx.pod_names().is_empty(), "{slot}");
            // The next command makes the pod, with the slot mounted.
            fx.env.ensure(&ws, &Steps::default()).await.unwrap();
            assert_eq!(fx.requests("POST"), posts + 1, "{slot}");
            assert!(fx.pod_names().contains(&pod));
        }
        // The pod that was made has every slot: sealing again deletes nothing.
        let deletes = fx.requests("DELETE");
        fx.env.seal(&ws).await.unwrap();
        assert_eq!(fx.requests("DELETE"), deletes);
        assert_eq!(
            fx.work_mounts(&pod)
                .iter()
                .filter(|(path, _, ro)| !ro && path.starts_with(&ws.path().display().to_string()))
                .count(),
            1 + 3,
            "the run's directory and its three slots"
        );
    }

    #[tokio::test]
    async fn seal_leaves_a_pod_alone_while_a_command_runs_in_it_and_says_why() {
        let fx = fixture(settings());
        let ws = fx.ws(RUN);
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        let notes = ws.add_scratch("notes", &me()).await.unwrap();

        fx.exec.lock().unwrap().active = Some(1);
        let error = fx.env.seal(&ws).await.expect_err("a command runs");
        assert!(matches!(error, EnvError::Unavailable(_)), "{error}");
        assert_eq!(
            error.class(),
            ErrorClass::Transient,
            "it ends, and the pod is made again"
        );
        let said = error.to_string();
        assert!(said.contains("command is running"), "{said}");
        assert!(said.contains(&notes.path().display().to_string()), "{said}");
        assert_eq!(fx.requests("DELETE"), 0, "the command is not killed");
        assert_eq!(fx.pod_names().len(), 1);

        // A pod that cannot be asked is left too.
        fx.exec.lock().unwrap().active = None;
        let error = fx.env.seal(&ws).await.expect_err("cannot ask");
        assert!(matches!(error, EnvError::Unavailable(_)), "{error}");
        assert_eq!(fx.requests("DELETE"), 0);

        // The command ends; the next ensure makes the pod again, and sealing finds nothing to do.
        fx.exec.lock().unwrap().active = Some(0);
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        assert_eq!((fx.requests("POST"), fx.requests("DELETE")), (2, 1));
        fx.env.seal(&ws).await.unwrap();
        assert_eq!(fx.requests("DELETE"), 1);
    }

    #[tokio::test]
    async fn seal_with_no_pod_does_nothing_and_never_makes_one() {
        let fx = fixture(settings());
        let ws = fx.ws(RUN);
        ws.add_repository(&fx.remote("alpha")).await.unwrap();
        ws.add_scratch("notes", &me()).await.unwrap();
        fx.env.seal(&ws).await.unwrap();
        assert!(fx.pod_names().is_empty());
        assert_eq!((fx.requests("POST"), fx.requests("DELETE")), (0, 0));
        assert!(
            fx.exec.lock().unwrap().commands.is_empty(),
            "nothing is asked of a pod that is not there"
        );
        // The run's directory was not made for a pod that is not made either.
        let fresh = fx.ws(OTHER);
        fx.env.seal(&fresh).await.unwrap();
        assert!(!fresh.path().exists());
    }

    #[tokio::test]
    async fn seal_leaves_a_pod_that_has_every_mount_and_asks_nothing_of_it() {
        let fx = fixture(settings());
        let ws = fx.ws(RUN);
        ws.add_repository(&fx.remote("alpha")).await.unwrap();
        ws.add_scratch("notes", &me()).await.unwrap();
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        fx.exec.lock().unwrap().active = Some(1);
        fx.env.seal(&ws).await.unwrap();
        assert_eq!((fx.requests("POST"), fx.requests("DELETE")), (1, 0));
        assert!(
            fx.exec.lock().unwrap().commands.is_empty(),
            "a pod that needs nothing is not asked whether it is busy"
        );
        // A mount the pod has and the run no longer needs is no reason either.
        lock(&fx.api)
            .pods
            .get_mut(&pod_name(RUN))
            .unwrap()
            .metadata
            .annotations
            .as_mut()
            .unwrap()
            .entry("adam.vymalo.com/workspace-mounts".to_owned())
            .and_modify(|v| v.push_str("\nrw /work/workspaces/gone"));
        fx.env.seal(&ws).await.unwrap();
        assert_eq!(fx.requests("DELETE"), 0);
    }

    #[tokio::test]
    async fn seal_deletes_a_pod_that_is_not_ready_without_asking_it() {
        let fx = fixture(settings());
        let ws = fx.ws(RUN);
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        // The pod is up and then goes back to waiting (a restart of its container).
        {
            let mut st = lock(&fx.api);
            st.stuck = Some(pull_failure());
            st.pods.get_mut(&pod_name(RUN)).unwrap().status = Some(pull_failure());
        }
        ws.add_scratch("notes", &me()).await.unwrap();
        fx.env.seal(&ws).await.unwrap();
        assert_eq!(fx.requests("DELETE"), 1);
        assert!(fx.exec.lock().unwrap().commands.is_empty());
        assert_eq!(fx.requests("POST"), 1, "and none is made");
    }

    #[tokio::test]
    async fn seal_leaves_the_pod_of_another_run_alone() {
        let fx = fixture(settings());
        let ws = fx.ws(RUN);
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        fx.exec.lock().unwrap().active = Some(0);
        ws.add_scratch("notes", &me()).await.unwrap();
        // The name of the run's pod is another run's (a collision of the names).
        lock(&fx.api)
            .pods
            .get_mut(&pod_name(RUN))
            .unwrap()
            .metadata
            .annotations
            .as_mut()
            .unwrap()
            .insert("adam.vymalo.com/run-id".to_owned(), OTHER.to_owned());
        fx.env.seal(&ws).await.unwrap();
        assert_eq!(fx.requests("DELETE"), 0, "it is not this run's to delete");
    }

    #[tokio::test]
    async fn seal_with_the_cluster_down_is_unavailable_and_transient() {
        let fx = fixture(settings());
        let ws = fx.ws(RUN);
        fx.env.ensure(&ws, &Steps::default()).await.unwrap();
        lock(&fx.api).down = true;
        let error = fx.env.seal(&ws).await.expect_err("down");
        assert!(matches!(error, EnvError::Unavailable(_)), "{error}");
        assert_eq!(error.class(), ErrorClass::Transient);
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
        assert_eq!(
            prepared.cwd,
            Path::new("/"),
            "the client runs from /; the cwd is the pod's"
        );
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
        // No secret is anywhere in the command line: the pod holds no model key.
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
        // A process that outlives its call has `bg-` in front of the same id, and it is what
        // `adam-exec` is told (it names its record after it).
        let kept = session
            .prepare(&ExecSpec::shell("npm run dev", ws.path()).background())
            .unwrap();
        assert!(kept.exec.is_background() && kept.exec.as_str().starts_with("bg-kp-"));
        assert!(!a.exec.is_background());
        assert!(
            kept.args
                .iter()
                .any(|arg| arg.to_string_lossy() == kept.exec.as_str()),
            "{:?}",
            kept.args
        );
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
    async fn the_session_says_the_shape_of_its_pod_which_is_the_templates_requests() {
        let fx = fixture(settings());
        let session = fx.env.ensure(&fx.ws(RUN), &Steps::default()).await.unwrap();
        assert_eq!(
            session.sandbox_shape(),
            Some(adam_workspace::SandboxShape::new(
                Some(250),
                Some(512 * 1024 * 1024)
            )),
            "250m and 512Mi, the requests of the template's run container"
        );
        // The pod that was made is the one that reserves it.
        let pod = fx.api.lock().unwrap().pods.values().next().unwrap().clone();
        let requests = pod.spec.unwrap().containers[0]
            .resources
            .clone()
            .unwrap()
            .requests
            .unwrap();
        assert_eq!(requests["cpu"].0, "250m");
        assert_eq!(requests["memory"].0, "512Mi");
    }

    /// ADR 0038: a run pod carries no agent binary and no model key. The environment gives its
    /// commands neither, and the pod made from a template that names none has none.
    #[tokio::test]
    async fn a_run_pod_spec_has_no_model_key_and_no_opencode() {
        let fx = fixture(settings());
        fx.env.ensure(&fx.ws(RUN), &Steps::default()).await.unwrap();
        let pod = fx.api.lock().unwrap().pods.values().next().unwrap().clone();
        let text = serde_json::to_string(&pod).unwrap();
        assert!(!text.contains("MODEL_API_KEY"), "{text}");
        assert!(!text.contains("opencode"), "{text}");
        assert!(!text.contains("secretKeyRef"), "{text}");
    }
}
