# adam-engine

adam-rs embedded in an application ([ADR 0047](../../docs/decisions/0047-adam-rs-can-be-embedded-in-an-application.md)):
the runtime on the host's tokio runtime, with a store and a model client the host gives, and no server, no protocol and
nothing on the network of its own.

## Where it sits

**Slice 1 of ADR 0047: the embedded set, and nothing more yet.** The crate's dependencies are the crates an embedded
agent needs, each with its server parts turned off, and it re-exports them:

| Crate | Built without |
|---|---|
| [`adam-a2a-runtime`](../adam-a2a-runtime/README.md) | `push` (and through it [`adam-a2a`](../adam-a2a/README.md)'s `server`) |
| [`adam-ui`](../adam-ui/README.md) | `thread-tools`: use `Ui::embedded` and `Ui::screen` |
| [`adam-runtime`](../adam-runtime/README.md), [`adam-llm-agent`](../adam-llm-agent/README.md), [`adam-core`](../adam-core/README.md), [`adam-agent-fs`](../adam-agent-fs/README.md), [`adam-model`](../adam-model/README.md), [`adam-error`](../adam-error/README.md) | nothing: they have no server parts |

The API (start, send, steer, answer, cancel, list and a stream of events, over `RuntimeTaskBackend`) is slice 4. The
model's credentials come from the host through `adam_model::ModelCredentials`
([ADR 0058](../../docs/decisions/0058-the-models-credentials-come-from-a-provider-asked-for-each-call.md)): the engine
reads no key.

## The dependency check

`tools/embedded-deps/check.sh` (CI job `embedded`, part of the required `lint`):

* builds `adam-a2a`, `adam-a2a-runtime` and `adam-ui` with their defaults off and with each feature alone, tests
  included, and this crate, under `clippy --all-targets -D warnings`;
* fails when this crate's tree (`cargo tree -e normal --target all`, so a Windows- or macOS-only dependency is seen
  from Linux too) holds any of: `axum`, `hyper`, `h2`, `reqwest`, `ureq`, `curl`, `curl-sys`, `tower-http`, `warp`,
  `actix-web`, `tonic`, `tokio-tungstenite`, `a2a-server-lf`, `a2a-client-lf`, `utoipa-swagger-ui`, `sqlx`,
  `sqlx-core`, `tokio-postgres`, `postgres`, `mongodb`, `rusqlite`, `libsqlite3-sys`, `diesel`, `redis`, `kube`,
  `k8s-openapi`, `rustls`, `aws-lc-rs`, `aws-lc-sys`, `ring`, `openssl`, `openssl-sys`, `native-tls`, `rmcp`. Not
  `mio`: tokio's process support needs it, and the tools (slice 6) run commands;
* runs the split crates' unit and integration tests with their defaults off, and their docs under `-D warnings`;
* `self-test` proves it: it plants `axum`, then `rustls`, in this crate's manifest (in the working tree, put back after),
  and turns `adam-ui/thread-tools` and `adam-a2a-runtime/push` back on; each must fail the check and be named. The
  files are put back on any exit, a failure or an interrupt included.

## Features and environment

No Cargo features, no environment variables.

## Tests

None yet beyond the check above: the API arrives with slice 4.

## See also

[ADR 0047](../../docs/decisions/0047-adam-rs-can-be-embedded-in-an-application.md),
[ADR 0058](../../docs/decisions/0058-the-models-credentials-come-from-a-provider-asked-for-each-call.md),
[`docs/guides/embed-adam.md`](../../docs/guides/embed-adam.md).
