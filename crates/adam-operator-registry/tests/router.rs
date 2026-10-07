//! The router: authentication, the cache headers, `ETag` and `304`, `HEAD`, the refusals, and the flag a
//! full registry raises.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use adam_operator_ports::memory::MemoryDirectory;
use adam_operator_registry::document::{MAX_ITEMS, PROFILE};
use adam_operator_registry::{MAX_AGE_SECS, PATH, Registry, Token, build};
use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, Response, StatusCode, header};
use support::{consumer, entry, titled};
use tower::ServiceExt;

const TOKEN: &str = "registry-token-0123456789";

struct Fixture {
    directory: MemoryDirectory,
    full: Arc<AtomicBool>,
    router: Router,
}

fn fixture() -> Fixture {
    let directory = MemoryDirectory::new();
    directory.put(titled("ns", "coder", "Coder", &["coding", "git"]));
    directory.put(titled("ns", "researcher", "Researcher", &[]));
    let full = Arc::new(AtomicBool::new(false));
    let registry = Registry::new(directory.clone(), Token::new(TOKEN).unwrap())
        .with_anchor("http://operator.ns.svc:8080/registry/v1/agents")
        .with_full_flag(full.clone());
    Fixture {
        directory,
        full,
        router: Arc::new(registry).router(),
    }
}

async fn call(router: &Router, method: Method, headers: &[(&str, &str)]) -> Response<Body> {
    let mut req = Request::builder().method(method).uri(PATH);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    router
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

fn bearer() -> Vec<(&'static str, String)> {
    vec![("authorization", format!("Bearer {TOKEN}"))]
}

async fn get(router: &Router, extra: &[(&str, &str)]) -> Response<Body> {
    let auth = bearer();
    let mut headers: Vec<(&str, &str)> = auth.iter().map(|(k, v)| (*k, v.as_str())).collect();
    headers.extend_from_slice(extra);
    call(router, Method::GET, &headers).await
}

async fn body(response: Response<Body>) -> Vec<u8> {
    axum::body::to_bytes(response.into_body(), 4 << 20)
        .await
        .unwrap()
        .to_vec()
}

fn header_of(response: &Response<Body>, name: header::HeaderName) -> String {
    response
        .headers()
        .get(name)
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default()
}

// ---------------------------------------------------------------- authentication

#[tokio::test]
async fn a_missing_or_wrong_token_is_401_with_no_body_and_no_document() {
    let f = fixture();
    let wrong_scheme = format!("Basic {TOKEN}");
    let almost = format!("Bearer {TOKEN}x");
    let short = format!("Bearer {}", &TOKEN[..TOKEN.len() - 1]);
    let cases: Vec<Vec<(&str, &str)>> = vec![
        vec![],
        vec![("authorization", "")],
        vec![("authorization", "Bearer")],
        vec![("authorization", "Bearer ")],
        vec![("authorization", TOKEN)],
        vec![("authorization", wrong_scheme.as_str())],
        vec![("authorization", almost.as_str())],
        vec![("authorization", short.as_str())],
        vec![("authorization", "Bearer wrong")],
        // The token in the right place is not enough when there are two credentials.
        vec![
            ("authorization", format!("Bearer {TOKEN}").leak() as &str),
            ("authorization", "Bearer other"),
        ],
    ];
    for headers in cases {
        for method in [Method::GET, Method::HEAD] {
            let response = call(&f.router, method.clone(), &headers).await;
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{method} {headers:?}"
            );
            assert_eq!(header_of(&response, header::WWW_AUTHENTICATE), "Bearer");
            assert_eq!(header_of(&response, header::VARY), "Authorization");
            assert!(response.headers().get(header::ETAG).is_none());
            assert!(body(response).await.is_empty(), "no body: {headers:?}");
        }
    }
}

#[tokio::test]
async fn the_scheme_is_case_insensitive_and_the_token_is_not() {
    let f = fixture();
    for scheme in ["Bearer", "bearer", "BEARER"] {
        let value = format!("{scheme} {TOKEN}");
        let response = call(&f.router, Method::GET, &[("authorization", &value)]).await;
        assert_eq!(response.status(), StatusCode::OK, "{scheme}");
    }
    let upper = format!("Bearer {}", TOKEN.to_uppercase());
    let response = call(&f.router, Method::GET, &[("authorization", &upper)]).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn an_unauthenticated_caller_learns_nothing_not_even_a_304() {
    let f = fixture();
    let etag = header_of(&get(&f.router, &[]).await, header::ETAG);
    let response = call(&f.router, Method::GET, &[("if-none-match", &etag)]).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------- the document and its headers

#[tokio::test]
async fn it_serves_the_linkset_with_the_contracts_headers() {
    let f = fixture();
    let response = get(&f.router, &[("accept", "application/linkset+json")]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header_of(&response, header::CONTENT_TYPE),
        "application/linkset+json; profile=\"https://www.rfc-editor.org/info/rfc9727\""
    );
    let cache = header_of(&response, header::CACHE_CONTROL);
    assert_eq!(cache, "private, max-age=30");
    let max_age: u32 = cache.rsplit('=').next().unwrap().parse().unwrap();
    assert!(
        max_age <= 60 && max_age == MAX_AGE_SECS,
        "the contract asks for 60 or less"
    );
    assert_eq!(header_of(&response, header::VARY), "Authorization");
    let etag = header_of(&response, header::ETAG);
    assert!(
        etag.starts_with('"') && !etag.starts_with("W/"),
        "a strong validator: {etag}"
    );

    let bytes = body(response).await;
    let expected = build(
        &[
            titled("ns", "coder", "Coder", &["coding", "git"]),
            titled("ns", "researcher", "Researcher", &[]),
        ],
        Some("http://operator.ns.svc:8080/registry/v1/agents"),
    )
    .unwrap();
    assert_eq!(bytes, expected.body);
    assert_eq!(etag, expected.etag);
    let read = consumer::parse(&bytes).unwrap();
    assert_eq!(read.items.len(), 2);
    assert!(read.skipped.is_empty());
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.contains(PROFILE));
    assert!(!text.contains(TOKEN), "the token is never in the document");
}

#[tokio::test]
async fn head_has_the_headers_and_no_body() {
    let f = fixture();
    let headers = bearer();
    let pairs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let response = call(&f.router, Method::HEAD, &pairs).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header_of(&response, header::CACHE_CONTROL),
        "private, max-age=30"
    );
    assert!(!header_of(&response, header::ETAG).is_empty());
    assert!(body(response).await.is_empty());
}

#[tokio::test]
async fn a_matching_etag_is_304_with_the_validator_and_no_body() {
    let f = fixture();
    let first = get(&f.router, &[]).await;
    let etag = header_of(&first, header::ETAG);
    for presented in [
        etag.clone(),
        format!("W/{etag}"),
        format!("\"other\", {etag}"),
        format!("{etag} , \"x\""),
        "*".to_owned(),
    ] {
        let response = get(&f.router, &[("if-none-match", &presented)]).await;
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED, "{presented}");
        assert_eq!(header_of(&response, header::ETAG), etag);
        assert_eq!(
            header_of(&response, header::CACHE_CONTROL),
            "private, max-age=30"
        );
        assert_eq!(header_of(&response, header::VARY), "Authorization");
        assert!(body(response).await.is_empty());
    }
    for presented in ["\"other\"", "", "W/\"other\"", &etag[1..etag.len() - 1]] {
        let response = get(&f.router, &[("if-none-match", presented)]).await;
        assert_eq!(response.status(), StatusCode::OK, "{presented:?}");
    }
}

#[tokio::test]
async fn a_change_of_the_fleet_is_a_new_etag_and_the_old_one_is_no_longer_current() {
    let f = fixture();
    let old = header_of(&get(&f.router, &[]).await, header::ETAG);
    f.directory.put(titled("ns", "chat", "Chat", &["chat"]));
    let response = get(&f.router, &[("if-none-match", &old)]).await;
    assert_eq!(response.status(), StatusCode::OK);
    let new = header_of(&response, header::ETAG);
    assert_ne!(old, new);
    let read = consumer::parse(&body(response).await).unwrap();
    let ids: Vec<&str> = read.items.iter().map(|i| i.service.as_str()).collect();
    assert_eq!(ids, ["chat", "coder", "researcher"]);
    // And a service that is blocked, or whose A2A is off, leaves the list.
    let mut blocked = titled("ns", "chat", "Chat", &["chat"]);
    blocked.blocked = true;
    f.directory.put(blocked);
    let read = consumer::parse(&body(get(&f.router, &[]).await).await).unwrap();
    assert_eq!(read.items.len(), 2);
}

#[tokio::test]
async fn an_empty_fleet_is_an_empty_list_that_a_reader_accepts() {
    let f = fixture();
    f.directory.remove("ns", "coder");
    f.directory.remove("ns", "researcher");
    let response = get(&f.router, &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    let read = consumer::parse(&body(response).await).unwrap();
    assert!(read.items.is_empty());
}

// ---------------------------------------------------------------- when there is no document

#[tokio::test]
async fn a_directory_that_has_not_synced_is_503_never_an_empty_list() {
    let f = fixture();
    f.directory.set_ready(false);
    let response = get(&f.router, &[]).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(header_of(&response, header::CACHE_CONTROL), "no-store");
    assert!(body(response).await.is_empty());
    f.directory.set_ready(true);
    assert_eq!(get(&f.router, &[]).await.status(), StatusCode::OK);
}

#[tokio::test]
async fn past_a_limit_it_refuses_with_503_and_raises_the_flag_until_it_fits_again() {
    let f = fixture();
    assert!(!f.full.load(Ordering::Acquire));
    for i in 0..=MAX_ITEMS {
        f.directory.put(entry("big", &format!("a{i}")));
    }
    let response = get(&f.router, &[]).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        body(response).await.is_empty(),
        "a refusal is not a truncated list"
    );
    assert!(
        f.full.load(Ordering::Acquire),
        "every service learns it is not listed"
    );
    // An ETag from before does not turn a refusal into a 304.
    let response = get(&f.router, &[("if-none-match", "*")]).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

    f.directory.remove("big", "a0");
    f.directory.remove("big", "a1");
    for i in 2..=MAX_ITEMS {
        f.directory.remove("big", &format!("a{i}"));
    }
    assert_eq!(get(&f.router, &[]).await.status(), StatusCode::OK);
    assert!(!f.full.load(Ordering::Acquire));
}

#[tokio::test]
async fn the_document_call_sets_the_flag_without_a_request() {
    // The operator calls `document()` on a timer so the flag is true when nobody asks.
    let directory = MemoryDirectory::new();
    for i in 0..=MAX_ITEMS {
        directory.put(entry("big", &format!("a{i}")));
    }
    let full = Arc::new(AtomicBool::new(false));
    let registry =
        Registry::new(directory, Token::new(TOKEN).unwrap()).with_full_flag(full.clone());
    assert!(registry.document().await.is_err());
    assert!(full.load(Ordering::Acquire));
}

// ---------------------------------------------------------------- what else is routed

#[tokio::test]
async fn only_the_registry_path_is_routed_and_only_for_reading() {
    let f = fixture();
    let auth = bearer();
    let headers: Vec<(&str, &str)> = auth.iter().map(|(k, v)| (*k, v.as_str())).collect();
    for method in [Method::POST, Method::PUT, Method::DELETE, Method::PATCH] {
        let response = call(&f.router, method.clone(), &headers).await;
        assert_eq!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{method}"
        );
    }
    for path in [
        "/",
        "/registry",
        "/registry/v1",
        "/registry/v1/agents/coder",
        "/registry/v2/agents",
    ] {
        let response = f
            .router
            .clone()
            .oneshot(
                Request::get(path)
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
}
