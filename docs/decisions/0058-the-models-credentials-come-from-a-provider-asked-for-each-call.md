# 0058. The model's credentials come from a provider, asked for each call

Status: **Accepted** (2026-11-19). The owner asked, on 2026-11-19, that an embedded adam mint the token of its model calls
itself, "a kind of trait for providing api-calls", one that serves both the cloud and a person's machine. This ADR is
the shape of that trait; the owner may revisit it.

## Context

`OpenAiCompatible` read its key once: `OpenAiConfig::api_key` became a default `Authorization` header of its
`reqwest::Client` when the client was built (`crates/adam-model-openai/src/lib.rs`, before this change). Three hosts
need something else:

1. **An embedded engine** ([ADR 0047](0047-adam-rs-can-be-embedded-in-an-application.md)) runs inside an app that already holds
   the person's session. An OAuth issuer's access tokens are short-lived (minutes) and renewed with a refresh token
   that rotates on every use; a key read once fails at the first expiry, and only the host can renew.
2. **A token bound to the request.** DPoP (RFC 9449) proves each request with a signature over its method and URL, and a
   server may answer `401` with a `DPoP-Nonce` that the next proof must carry. Nothing that is fixed at build time can
   do that.
3. **An injector in front of the model** (another-agentic-system#334): the agent holds no key; a proxy adds the
   credential, and the agent only names the run. The agent then sends a header that is no secret of its own, and no
   `Authorization` at all.

Every model call of the client leaves through one function (`OpenAiCompatible::send`), for completions and streams
alike. The client's other outbound HTTP (MCP servers, the code host, remote A2A agents) is in other crates and is not
part of this decision.

## Decision

1. **A port in `adam-model`: `ModelCredentials`**, beside `ModelClient`. `headers(CredentialRequest { method, url,
   attempt })` gives the headers of one call; `rejected(call, &Rejection { status, www_authenticate, dpop_nonce })`
   says whether a new credential may help (`AfterRejection::Retry`) or not (`GiveUp`, the default). No HTTP library's
   types in the signatures (rule 2): names are strings, values `SecretString`.
2. **Asked for every call, cached by nobody but the provider.** The client never keeps what it was given and never puts
   it in a journal, a log or an error. A provider that mints must answer from its own cache while its credential is valid.
3. **The key is the default provider.** `StaticBearer` is what `MODEL_API_KEY` gives, so a deployment that changes
   nothing behaves as before; `OpenAiCompatible::with_credentials` replaces it.
4. **One retry, on 401 only.** The provider hears of the first `401` with what the server said; on `Retry` the client
   asks for new headers and sends the same body once more. A second `401` is the call's `Auth` error; a `403` is never
   retried. The client still retries nothing else (the runtime decides from the class).
5. **The provider's errors keep their class.** `Auth` when the person must act (signed out), `Transient` when the
   issuer did not answer, so the runtime retries the second as it retries the model's own failures.
6. **A provider may not frame the request.** `content-type`, `content-length`, `accept`, `host` and
   `transfer-encoding` from a provider are refused with `InvalidRequest`, nothing sent.
7. **A testkit double**, `RenewingCredentials` (`token-1`, `token-2`, ... and a retry on each rejection), for the tests
   of a client.

## Alternatives

* **Wrap the HTTP pool** (a transport trait, or `reqwest-middleware`). It would also let a host bring its own HTTP stack,
  but the trait would carry streaming bodies, timeouts and the redirect policy, and every implementation would have to
  get them right. Minting, renewing, proving and tagging need only the headers and the 401, so the narrower port came
  first; a transport port stays possible, for example for a browser build, and would sit under this one.
* **A key file re-read on change.** Simpler for one host, but it puts a secret on disk, races with a rotating refresh
  token and cannot prove a request.

## Consequences

* `adam-model` gains `ModelCredentials`, `DynCredentials`, `CredentialRequest`, `CredentialHeaders`, `Rejection`,
  `AfterRejection`, `StaticBearer` and `RenewingCredentials`, and depends on `secrecy`. Nothing that existed changed
  signature: no implementer breaks.
* `adam-model-openai` gains `with_credentials` and depends on `bytes` (already in the tree under `reqwest`). The key is no
  longer a default header of the `reqwest::Client`; a bad key still fails at build time (`InvalidApiKey`).
* A provider is called once per model call, twice on a renewed `401`.

## Not built here

* **What a host embeds.** The engine of ADR 0047 takes the host's provider when it is built.
* **The injector's provider.** Its run header is the injector's decision (another-agentic-system#334).
* **A signed-out person mid-run.** Today it is the call's `Auth` error, which ends the run; parking the run until the
  person signs in again is a later decision, beside the parked 402.
* **The other clients** (MCP, the code host, remote A2A agents) keep their own credentials.
* **A refresh token that survives a restart.** The device grant keeps its tokens in memory only, so a restart asks
  for a new code; a store for it would be a port of its own.

## A provider that signs in: the device grant

Built in the same change: [`adam-model-oauth`](../../crates/adam-model-oauth/README.md)'s `DeviceGrant`, the OAuth 2.0
device authorization grant (RFC 8628) for any issuer, and `MODEL_AUTH=device` with `MODEL_OAUTH_ISSUER`,
`MODEL_OAUTH_CLIENT_ID` and `MODEL_OAUTH_SCOPE` in `adam-service`, so both binaries can use it.

* **The issuer is a setting**, with no default: its discovery document names the endpoints, which must be on its
  origin; `https`, or `http` on a loopback address.
* **The person decides when a code is asked for.** One code per process, shown through a `SignInPrompt` (a log line
  in the binaries, never a token). A denied or expired code, or a refused renewal, signs the process out until it
  restarts; it never asks for a new code on its own.
* **Renewal before expiry and after a 401**, one at a time, the rotated refresh token replacing the old one.
* **A call waits for the sign-in** at most 120 s, then fails as transient (retried); the code is never in a run's error.
* **Only `Bearer` tokens.** A DPoP-bound token needs the host's own provider, with its key.

## Where

`crates/adam-model/src/credentials.rs` (the port, `StaticBearer`, `RenewingCredentials`, unit tests),
`crates/adam-model-openai/src/lib.rs` (`with_credentials`, `send`, `credential_headers`, `rejection_of`),
`crates/adam-model-openai/tests/credentials.rs`; `crates/adam-model-oauth` (`src/lib.rs`, `tests/device_grant.rs`); `ModelConfig::parse` and `parse_device_grant` in `crates/adam-service/src/config.rs`.
