//! A fake API server for the Cluster of CloudNativePG: a `tower` service behind a real `kube::Client`,
//! so the provisioner's requests are the real ones. It answers the discovery of
//! `postgresql.cnpg.io/v1` (or `404` when CloudNativePG is "not installed"), `GET`, server-side apply,
//! merge patches and `DELETE` of one Cluster, and records every call. It is not an API server: there
//! is no CloudNativePG controller (no status ever appears by itself), no validation and no garbage
//! collection. What a cluster does is `tests/cluster.rs`'s.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use http::{Request, Response};
use kube::Client;
use kube::client::Body;
use serde_json::{Value, json};
use tower::service_fn;

/// One request the provisioner made.
#[derive(Clone, Debug)]
pub struct Call {
    pub method: String,
    pub path: String,
    pub query: String,
    pub content_type: String,
    pub body: Option<Value>,
}

impl Call {
    pub fn writes(&self) -> bool {
        matches!(self.method.as_str(), "PATCH" | "DELETE" | "PUT" | "POST")
    }
}

struct State {
    installed: bool,
    serves_clusters: bool,
    objects: BTreeMap<String, Value>,
    calls: Vec<Call>,
    fail_with: Option<u16>,
    next_version: u64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            installed: true,
            serves_clusters: true,
            objects: BTreeMap::new(),
            calls: Vec::new(),
            fail_with: None,
            next_version: 0,
        }
    }
}

/// The fake server. Clones share one state.
#[derive(Clone, Default)]
pub struct Fake {
    state: Arc<Mutex<State>>,
}

fn lock(s: &Mutex<State>) -> MutexGuard<'_, State> {
    s.lock().unwrap_or_else(PoisonError::into_inner)
}

const PREFIX: &str = "/apis/postgresql.cnpg.io/v1";

fn path(ns: &str, name: &str) -> String {
    format!("{PREFIX}/namespaces/{ns}/clusters/{name}")
}

impl Fake {
    pub fn new() -> Self {
        Self::default()
    }

    /// A client whose requests go to this fake.
    pub fn client(&self) -> Client {
        let fake = self.clone();
        let service = service_fn(move |req: Request<Body>| {
            let fake = fake.clone();
            async move { Ok::<_, Infallible>(fake.handle(req).await) }
        });
        Client::new(service, "default")
    }

    /// Whether the API group exists (CloudNativePG is installed).
    pub fn set_installed(&self, installed: bool) {
        lock(&self.state).installed = installed;
    }

    /// Whether the API group lists `clusters` among its resources (a group can exist without them).
    pub fn set_serves_clusters(&self, serves: bool) {
        lock(&self.state).serves_clusters = serves;
    }

    /// Put a Cluster there, as if someone had made it.
    pub fn put(&self, ns: &str, name: &str, mut object: Value) {
        object["metadata"]["name"] = json!(name);
        object["metadata"]["namespace"] = json!(ns);
        let mut s = lock(&self.state);
        s.next_version += 1;
        object["metadata"]["resourceVersion"] = json!(s.next_version.to_string());
        s.objects.insert(path(ns, name), object);
    }

    pub fn get(&self, ns: &str, name: &str) -> Option<Value> {
        lock(&self.state).objects.get(&path(ns, name)).cloned()
    }

    /// Set the status of a Cluster, as CloudNativePG would.
    pub fn set_status(&self, ns: &str, name: &str, status: Value) {
        if let Some(o) = lock(&self.state).objects.get_mut(&path(ns, name)) {
            o["status"] = status;
        }
    }

    /// Answer every request with this HTTP status, or go back to answering.
    pub fn fail_with(&self, code: Option<u16>) {
        lock(&self.state).fail_with = code;
    }

    pub fn calls(&self) -> Vec<Call> {
        lock(&self.state).calls.clone()
    }

    pub fn clear_calls(&self) {
        lock(&self.state).calls.clear();
    }

    pub fn writes(&self) -> Vec<Call> {
        self.calls().into_iter().filter(Call::writes).collect()
    }

    async fn handle(&self, req: Request<Body>) -> Response<Body> {
        let (parts, body) = req.into_parts();
        let bytes = body.collect_bytes().await.unwrap_or_default();
        let call = Call {
            method: parts.method.as_str().to_owned(),
            path: parts.uri.path().to_owned(),
            query: parts.uri.query().unwrap_or_default().to_owned(),
            content_type: parts
                .headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_owned(),
            body: serde_json::from_slice(&bytes).ok(),
        };
        let mut s = lock(&self.state);
        s.calls.push(call.clone());
        if let Some(code) = s.fail_with {
            return status(code, "the fake is failing on purpose", "InternalError");
        }
        if !s.installed {
            return status(
                404,
                "the server could not find the requested resource",
                "NotFound",
            );
        }
        if call.method == "GET" && call.path == PREFIX {
            let mut resources = vec![
                json!({"name": "poolers", "singularName": "pooler", "namespaced": true,
                                            "kind": "Pooler", "verbs": ["get"]}),
            ];
            if s.serves_clusters {
                resources.push(
                    json!({"name": "clusters", "singularName": "cluster", "namespaced": true,
                                      "kind": "Cluster", "verbs": ["get", "patch", "delete"]}),
                );
            }
            return json_response(
                200,
                &json!({"kind": "APIResourceList", "apiVersion": "v1",
                        "groupVersion": "postgresql.cnpg.io/v1", "resources": resources}),
            );
        }
        match call.method.as_str() {
            "GET" => match s.objects.get(&call.path) {
                Some(o) => json_response(200, o),
                None => status(404, "not found", "NotFound"),
            },
            "PATCH" => {
                let Some(patch) = call.body.clone() else {
                    return status(400, "no body", "BadRequest");
                };
                s.next_version += 1;
                let version = s.next_version.to_string();
                let existing = s.objects.get(&call.path).cloned();
                let mut merged = if call.content_type.contains("apply-patch") {
                    let mut object = patch;
                    if let Some(old) = &existing {
                        // What the server keeps across an apply: its status and bookkeeping.
                        if let Some(v) = old.get("status") {
                            object["status"] = v.clone();
                        }
                        if let Some(v) = old["metadata"].get("ownerReferences") {
                            object["metadata"]["ownerReferences"] = v.clone();
                        }
                    }
                    object
                } else {
                    let Some(mut old) = existing else {
                        return status(404, "not found", "NotFound");
                    };
                    merge(&mut old, &patch);
                    old
                };
                merged["metadata"]["resourceVersion"] = json!(version);
                s.objects.insert(call.path.clone(), merged.clone());
                json_response(200, &merged)
            }
            "DELETE" => {
                if s.objects.remove(&call.path).is_some() {
                    json_response(
                        200,
                        &json!({"apiVersion": "v1", "kind": "Status", "status": "Success", "code": 200}),
                    )
                } else {
                    status(404, "not found", "NotFound")
                }
            }
            _ => status(405, "not supported by the fake", "MethodNotAllowed"),
        }
    }
}

fn json_response(code: u16, body: &Value) -> Response<Body> {
    Response::builder()
        .status(code)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(body).unwrap_or_default()))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

fn status(code: u16, message: &str, reason: &str) -> Response<Body> {
    json_response(
        code,
        &json!({"apiVersion": "v1", "kind": "Status", "status": "Failure",
                "message": message, "reason": reason, "code": code}),
    )
}

/// JSON merge patch (RFC 7386).
fn merge(target: &mut Value, patch: &Value) {
    match patch {
        Value::Object(p) => {
            if !target.is_object() {
                *target = json!({});
            }
            for (k, v) in p {
                if v.is_null() {
                    target.as_object_mut().map(|m| m.remove(k));
                } else {
                    merge(&mut target[k.as_str()], v);
                }
            }
        }
        other => *target = other.clone(),
    }
}
