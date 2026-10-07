# 0031. Swagger UI and the A2A REST binding

Status: **Accepted** (2026-10-07), decided by the owner on 2026-10-07 (the four decisions below); the owner may revisit.
Builds on [ADR 0030](0030-a2a-push-notifications-list-tasks-extended-card-signatures.md) (the methods both bindings serve).

## Context

Every adam agent spoke A2A 1.0 over JSON-RPC only (`POST /`), and nothing described the API to a person: trying a call
meant writing a JSON-RPC envelope with `curl`. A2A 1.0 defines three bindings, and the SDK the server is built on already
ships the HTTP+JSON one (`a2a-server-lf` 0.4.4, `src/rest.rs`).

Facts the design rests on, each *verified 2026-10-07* unless it says otherwise:

* **The REST binding** (A2A 1.0 §11, <https://a2a-protocol.org/latest/specification/>): `POST /message:send`,
  `POST /message:stream` (SSE), `GET /tasks/{id}`, `GET /tasks`, `POST /tasks/{id}:cancel`, `POST /tasks/{id}:subscribe`
  (SSE), `/tasks/{id}/pushNotificationConfigs[/{configId}]` (`POST`, `GET`, `DELETE`), `GET /extendedAgentCard`; errors are
  a `google.rpc.Status` whose `details` carry an `ErrorInfo` with the A2A reason (§11.6); `application/a2a+json` SHOULD be the
  content type (§11.1).
* **Where it is**: an `AgentInterface`'s `url` is "the URL or address where this interface is available" (§4.4.6); the REST
  paths are relative to it (§11.3, and the SDK's client appends them to the interface URL: `a2a-client-lf` 0.2.5
  `src/rest.rs`). `supportedInterfaces` is an "Ordered list of supported interfaces. The first entry is preferred." (§4.4.1, §8.3.1).
* **The SDK's `rest_router`** (`a2a-server-lf` 0.4.4 `src/rest.rs`, read whole): it is generic over the same
  `RequestHandler` trait as `jsonrpc_router`, passes every request header to the handler as `ServiceParams`, limits bodies to
  the same 10 MiB, maps an `A2AError` to the HTTP status and envelope of §11.6, and also answers the paths of earlier drafts
  (`/message/send`, `/tasks/{id}/cancel`, `/tasks/{id}/push-configs`, `/agent-card/extended`, `GET /tasks/{id}:subscribe`,
  ...). What it does not do: its `Json` and `Query` extractors refuse a malformed request with plain text (400, 413, 415,
  422), its error builder is private, and, like the JSON-RPC route, it reads a send as proto3 JSON and drops
  `configuration.pushNotificationConfig` (the name of an earlier draft) silently.
* **The assets**: `utoipa-swagger-ui` 10.0.1 with the `vendored` feature takes Swagger UI 5.32.6 from the zip in the
  `utoipa-swagger-ui-vendored` 0.2.0 crate (its `build.rs` reads `utoipa_swagger_ui_vendored::SWAGGER_UI_VENDORED` when
  `CARGO_FEATURE_VENDORED` is set, and downloads only otherwise; the zip is in the crate's `res/`): nothing is fetched at
  build time. Both crates are MIT OR Apache-2.0 and need Rust 1.88 (the workspace's MSRV is 1.94).
* **Swagger UI** resolves a relative server URL against the URL it read the document from (`oas3BaseUrl` and
  `buildOas3UrlWithContext` in the bundled `swagger-ui-bundle.js`, with the spec URL made absolute against the page by
  `url-parse`), and cannot show a stream as it arrives: it waits for the whole response. *Verified 2026-10-07* in headless
  Chromium (Playwright) against `adam-agent` on PostgreSQL 16: the page renders under the policy below with no console error,
  no CSP violation and no request to another origin; **Authorize** then **Try it out** on `POST /message:send` and on
  `POST /` (an example from the dropdown) reach the server with the bearer token.

## Decision

1. **Both bindings, one handler.** The SDK's `rest_router` is mounted beside `jsonrpc_router` over the **same**
   `BackendHandler`, so caller identity, extension activation, push configs, `ListTasks`, the extended card and the mapping
   of a `BackendError` are one code path (`crates/adam-a2a/src/server.rs`). It is wrapped, not reimplemented: the same
   `echo_extensions` layer (reading `message.extensions` of a REST send), and `rest::rejections`, which answers the
   extractors' plain-text refusals and a send that names `pushNotificationConfig` in the binding's own envelope
   (`crates/adam-a2a/src/rest.rs`; the envelope is rebuilt there because the SDK's builder is private). A 401 on a REST path
   is a `google.rpc.Status` `UNAUTHENTICATED`; at `POST /` it stays the JSON-RPC error `-32000`. The SDK's aliases of
   earlier drafts stay mounted, and are documented as deprecated.
2. **The card lists `JSONRPC` first and `HTTP+JSON` second, at the same URL** (`PUBLIC_URL`): a client that takes the first
   interface, like the orchestration layer, is unchanged.
3. **One OpenAPI 3.1 document of both**, at `GET /openapi.json`, with Swagger UI at `GET /docs`. JSON-RPC is `POST /` with a
   request body that is a `oneOf` of one schema per method and a named example per method (a dropdown in Swagger UI). The
   streaming methods are documented as `text/event-stream`, and their descriptions say Swagger UI cannot show a stream and
   give the `curl`. The document is **written by hand** from the SDK's types (`crates/adam-a2a/src/openapi.rs`): `a2a-lf`
   derives neither `utoipa::ToSchema` nor `schemars::JsonSchema`, and the tests hold it to the code instead.
4. **The page is public; the calls need the token.** `/docs`, the files the page loads and `/openapi.json` join the public
   routes (`Authenticator::with_public_docs`, as the key set joins them when the card is signed); nothing else does. The
   document declares the bearer scheme exactly when the server requires one, so **Authorize** sends
   `Authorization: Bearer …`. It is built once from the public card and the switches the card already shows, so it is the
   same for every caller and names no token and nothing of the extended card.
5. **On by default, for every adam agent.** It lives in `adam-a2a` (`ServerOptions::docs`, `with_docs`) and `adam-service`
   reads `A2A_DOCS` (`false` turns it off), so `adam-coder` and `adam-agent` have it with no code of their own.
6. **Assets in the binary.** `utoipa-swagger-ui` with `vendored` (and `minified`: no source maps), its `serve` function and
   our own routes: only the eight files `index.html` loads are served, under
   `Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; …`
   (inline `style` attributes are Swagger UI's React components; `data:` images are its stylesheet's icons). Its
   configuration points at `../openapi.json`, so the page works under a nested prefix, turns the validator badge (a call to
   `validator.swagger.io`) and `?url=` off, and does not persist the token.
7. **Tested, not trusted** (`crates/adam-a2a/tests/docs.rs`, `tests/rest.rs`): the document validates against the official
   OpenAPI 3.1 JSON Schema (vendored in `tests/fixtures`) and every `$ref` resolves; every JSON-RPC method the handler
   dispatches is in the `oneOf` and the reverse, and each example reaches its method; every REST route is documented and
   every documented one is routed, and no other method is served on those paths; the examples, and real responses, stream
   events and errors from both bindings, validate against the document's schemas; the official client round-trips over
   HTTP+JSON. axum cannot list a router's routes, so the tables are pinned to `a2a-server-lf` 0.4.4 by a test that fails when
   `Cargo.lock` moves it.
8. **The netcup deployment stays cluster-internal.** No Ingress and no chart route: the docs are reached with
   `kubectl port-forward` (`deploy/coder/README.md`).

The interaction is the sequence in [Architecture](../architecture.md#swagger-ui-and-the-rest-binding). There is no
lifecycle to draw: the document and the assets are built once when the router is, and never change while it runs.

## Consequences

* The agent card has a second `supportedInterfaces` entry; a client that prefers `HTTP+JSON` now gets it.
* New public routes: `GET /docs`, `/docs/` and its files, `GET /openapi.json` (unless `A2A_DOCS=false`). The REST paths are
  behind the token like every other route.
* `ServerOptions` gains `docs` (`Default` is now hand-written: on); `adam_service::A2aSettings` gains `docs`, **so a literal
  built by hand needs it**.
* New dependencies: `utoipa-swagger-ui` and, through it, `utoipa`, `utoipa-gen`, `utoipa-swagger-ui-vendored`, `zip`,
  `mime_guess`, `flate2` and their own (MIT, Apache-2.0, Zlib); `jsonschema` (already in the tree) for the tests. The
  binaries carry Swagger UI's `dist` (about 4 MB, the unused ES bundles included: the crate embeds the whole folder).
* Not done: no Ingress or public route to the docs; the streaming calls cannot be tried in Swagger UI (use the `curl` the
  page gives); the multi-tenant path prefix of A2A 1.0 (`/{tenant}/…`) is not served (neither binding has tenants here);
  responses are `application/json`, not `application/a2a+json` (the SDK's choice).

## Alternatives rejected

* **A REST layer of our own** over `TaskBackend`: a second path where the SDK already has one; it would drift from JSON-RPC.
* **Generating the document with `utoipa` derives**: the SDK's types derive nothing, and wrapper types would document a
  copy, not the wire.
* **Assets from a CDN, or downloaded by `build.rs`**: a page that depends on a third party, and an image build that needs
  GitHub.
* **The docs behind the token**: Swagger UI cannot send a token before it has loaded the page and the document.
