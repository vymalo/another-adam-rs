//! A fake API server: a `tower` service behind a real `kube::Client`, so the provider's requests are
//! the real ones and what it does with the answers is the real code. It keeps objects by path,
//! answers `GET`, lists with a label selector, server-side apply and merge patches, and `DELETE`,
//! and records every call. It is not an API server: there are no controllers (no pod ever appears
//! by itself), no validation and no garbage collection. What a cluster does is the cluster test's.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use http::{Request, Response};
use kube::Client;
use kube::client::Body;
use serde_json::{Value, json};
use tower::service_fn;

/// The kinds the provider uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum K {
    StatefulSet,
    Deployment,
    Service,
    ConfigMap,
    Claim,
    Pod,
    NetworkPolicy,
    Budget,
}

impl K {
    /// The path of the collection of this kind in a namespace.
    pub fn collection(self, ns: &str) -> String {
        let (prefix, plural) = match self {
            Self::StatefulSet => ("/apis/apps/v1", "statefulsets"),
            Self::Deployment => ("/apis/apps/v1", "deployments"),
            Self::Service => ("/api/v1", "services"),
            Self::ConfigMap => ("/api/v1", "configmaps"),
            Self::Claim => ("/api/v1", "persistentvolumeclaims"),
            Self::Pod => ("/api/v1", "pods"),
            Self::NetworkPolicy => ("/apis/networking.k8s.io/v1", "networkpolicies"),
            Self::Budget => ("/apis/policy/v1", "poddisruptionbudgets"),
        };
        format!("{prefix}/namespaces/{ns}/{plural}")
    }

    /// The kind a collection path is of.
    pub fn of_path(path: &str) -> Option<Self> {
        [
            Self::StatefulSet,
            Self::Deployment,
            Self::Service,
            Self::ConfigMap,
            Self::Claim,
            Self::Pod,
            Self::NetworkPolicy,
            Self::Budget,
        ]
        .into_iter()
        .find(|k| {
            let c = k.collection("x");
            let plural = c.rsplit('/').next().unwrap_or_default();
            path.split('/').any(|s| s == plural)
        })
    }
}

/// One request the provider made.
#[derive(Clone, Debug)]
pub struct Call {
    pub method: String,
    pub path: String,
    pub query: String,
    pub content_type: String,
    pub body: Option<Value>,
}

impl Call {
    pub fn kind(&self) -> K {
        K::of_path(&self.path).unwrap_or_else(|| panic!("unknown path {}", self.path))
    }

    /// The last path segment: the object's name, for a call on one object.
    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or_default()
    }

    pub fn writes(&self) -> bool {
        matches!(self.method.as_str(), "PATCH" | "DELETE" | "PUT" | "POST")
    }
}

#[derive(Default)]
struct State {
    objects: BTreeMap<String, Value>,
    calls: Vec<Call>,
    fail_with: Option<u16>,
    next_version: u64,
}

/// The fake server. Clones share one state.
#[derive(Clone, Default)]
pub struct Fake {
    state: Arc<Mutex<State>>,
}

fn lock(s: &Mutex<State>) -> MutexGuard<'_, State> {
    s.lock().unwrap_or_else(PoisonError::into_inner)
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

    /// Put an object there, as if it had been made by someone.
    pub fn put(&self, kind: K, ns: &str, name: &str, mut object: Value) {
        object["metadata"]["name"] = json!(name);
        object["metadata"]["namespace"] = json!(ns);
        let mut s = lock(&self.state);
        s.next_version += 1;
        let version = s.next_version;
        object["metadata"]["resourceVersion"] = json!(version.to_string());
        object["metadata"]["generation"] = json!(1);
        s.objects
            .insert(format!("{}/{name}", kind.collection(ns)), object);
    }

    pub fn get(&self, kind: K, ns: &str, name: &str) -> Option<Value> {
        lock(&self.state)
            .objects
            .get(&format!("{}/{name}", kind.collection(ns)))
            .cloned()
    }

    /// Every object of a kind in a namespace, by name.
    pub fn all(&self, kind: K, ns: &str) -> Vec<(String, Value)> {
        let prefix = format!("{}/", kind.collection(ns));
        lock(&self.state)
            .objects
            .iter()
            .filter_map(|(p, o)| Some((p.strip_prefix(&prefix)?.to_owned(), o.clone())))
            .collect()
    }

    /// Set the status of an object, as its controller would.
    pub fn set_status(&self, kind: K, ns: &str, name: &str, status: Value) {
        let mut s = lock(&self.state);
        let key = format!("{}/{name}", kind.collection(ns));
        if let Some(o) = s.objects.get_mut(&key) {
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

    /// The calls that change something.
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
        let segments: Vec<&str> = call.path.split('/').collect();
        let Some(ns_at) = segments.iter().position(|s| *s == "namespaces") else {
            return status(404, "not a path of the fake", "NotFound");
        };
        let rest = &segments[ns_at + 2..];
        match (call.method.as_str(), rest) {
            ("GET", [_plural]) => {
                let prefix = format!("{}/", call.path);
                let selector = selector(&call.query);
                let items: Vec<&Value> = s
                    .objects
                    .iter()
                    .filter(|(p, o)| p.starts_with(&prefix) && matches(o, &selector))
                    .map(|(_, o)| o)
                    .collect();
                json_response(
                    200,
                    &json!({"apiVersion": "v1", "kind": "List", "metadata": {"resourceVersion": "1"}, "items": items}),
                )
            }
            ("GET", [_plural, _name]) => match s.objects.get(&call.path) {
                Some(o) => json_response(200, o),
                None => status(404, "not found", "NotFound"),
            },
            ("PATCH", [_plural, _name]) => {
                let Some(patch) = call.body.clone() else {
                    return status(400, "no body", "BadRequest");
                };
                s.next_version += 1;
                let version = s.next_version.to_string();
                let existing = s.objects.get(&call.path).cloned();
                let merged = if call.content_type.contains("apply-patch") {
                    let mut object = patch;
                    if let Some(old) = &existing {
                        // What the server keeps across an apply: its status and bookkeeping.
                        if let Some(v) = old.get("status") {
                            object["status"] = v.clone();
                        }
                        object["metadata"]["generation"] = old["metadata"]["generation"].clone();
                        object["metadata"]["uid"] = old["metadata"]["uid"].clone();
                    } else {
                        object["metadata"]["generation"] = json!(1);
                        object["metadata"]["uid"] = json!(format!("uid-{version}"));
                    }
                    object
                } else {
                    let Some(mut old) = existing else {
                        return status(404, "not found", "NotFound");
                    };
                    merge(&mut old, &patch);
                    old
                };
                let mut merged = merged;
                merged["metadata"]["resourceVersion"] = json!(version);
                s.objects.insert(call.path.clone(), merged.clone());
                json_response(200, &merged)
            }
            ("DELETE", [_plural, _name]) => {
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

/// The `labelSelector` of a query: `a=b,c=d`, percent-encoded.
fn selector(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .find_map(|p| p.strip_prefix("labelSelector="))
        .map(percent_decode)
        .map(|v| {
            v.split(',')
                .filter_map(|kv| kv.split_once('='))
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect()
        })
        .unwrap_or_default()
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| char::from(b).to_digit(16);
        match (
            bytes[i],
            bytes.get(i + 1).copied().and_then(hex),
            bytes.get(i + 2).copied().and_then(hex),
        ) {
            (b'%', Some(hi), Some(lo)) => {
                out.push(u8::try_from(hi * 16 + lo).unwrap_or(b'?'));
                i += 3;
            }
            (b'+', ..) => {
                out.push(b' ');
                i += 1;
            }
            (b, ..) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn matches(object: &Value, selector: &[(String, String)]) -> bool {
    selector
        .iter()
        .all(|(k, v)| object["metadata"]["labels"][k] == json!(v))
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
