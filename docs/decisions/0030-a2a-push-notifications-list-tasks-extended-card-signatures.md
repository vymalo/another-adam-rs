# 0030. The A2A server is complete: push notifications, ListTasks, the extended card and card signatures

Status: **Accepted** (2026-10-07), decided on the owner's request of 2026-10-07 (build, in this order: push notifications,
`ListTasks`, `GetExtendedAgentCard`, agent card signatures); the owner may revisit.
Builds on [ADR 0001](0001-library-first-host-roles.md) (the control plane and the workers meet in the store) and
[ADR 0006](0006-a2ui-and-the-vymalo-extensions-in-adam-rs.md) (the card and its extensions).

## Context

The server answered `PushNotificationNotSupported` for the four push-configuration methods, "ListTasks is not supported" for
`ListTasks`, `ExtendedAgentCardNotConfigured` for `GetExtendedAgentCard`, and served a card with `pushNotifications: false`,
`extendedAgentCard: false` and no `signatures` (`crates/adam-a2a/src/handler.rs`, `card.rs`). The orchestration layer and other
clients cannot be told that a long task finished without holding a stream open, cannot list their own tasks, and cannot trust
the card they fetched.

Facts the design rests on. Each is *verified 2026-10-07* from the A2A 1.0 specification,
<https://a2a-protocol.org/latest/specification/> (section numbers are its own), or from the SDK source in the Cargo registry
(`a2a-lf` 0.3.1 and `a2a-server-lf` 0.4.4, the versions `Cargo.lock` pins), unless it says *unverified*:

* **Push (§3.1.7 to §3.1.10, §4.3, §13.2).** `TaskPushNotificationConfig` has `id`, `taskId`, `url` (required), `token` and
  `authentication` (`scheme`, `credentials`). Create returns the config with an assigned id and `TaskNotFoundError` for a task
  that is not accessible; the configuration "MUST persist until task completion or explicit deletion"; delete "MUST be
  idempotent"; list "MAY support pagination". With `capabilities.pushNotifications` false or absent the four methods "MUST
  return `PushNotificationNotSupportedError`" (§3.3.4). `SendMessageConfiguration.taskPushNotificationConfig` (the SDK also
  accepts the alias `pushNotificationConfig`, `a2a-lf` `src/types.rs`) registers one with the message; its task id "should be
  empty". A notification is `POST {url}` with `Content-Type: application/a2a+json`, `Authorization: {scheme} {credentials}` and
  a `StreamResponse` body (exactly one of `task`, `message`, `statusUpdate`, `artifactUpdate`). The agent "MUST attempt
  delivery at least once", "MAY implement retry logic with exponential backoff", "SHOULD include a reasonable timeout
  (recommended: 10-30 seconds)" and "MAY stop attempting delivery after a configured number of consecutive failures"; it SHOULD
  validate webhook URLs against SSRF (reject private ranges, localhost, link-local, "implement URL allowlists where
  appropriate") and SHOULD store configs and credentials securely.
* **The specification does not ask the server to sign notifications.** There is no JWT or JWKS for them: "JWKS" appears only
  for the agent card's `jku`, and "JWT" only in a `bearerFormat` example. The credentials in `authentication` are all it
  requires. It does not name a header for `token`; the official Rust SDK sends `A2A-Notification-Token`
  (`a2a-server-lf` `src/push/sender.rs`), sends `Content-Type: application/json` (the specification says
  `application/a2a+json`), checks the URL by its text only (no DNS), and builds its HTTP client with only a timeout, so
  reqwest's default redirect policy applies. That sender is not used here.
* **ListTasks (§3.1.4, §13.1).** Filters `contextId`, `status`, `statusTimestampAfter` (inclusive), `pageSize` (default 50,
  minimum 1, maximum 100), `pageToken`, `historyLength`, `includeArtifacts` (artifacts "MUST be omitted entirely" when false).
  The response always carries `nextPageToken` (empty on the last page), `pageSize` and `totalSize` ("before pagination"). It
  "MUST return only tasks visible to the authenticated client", "MUST use cursor-based pagination", ordered by status
  timestamp, newest first; scoping applies "even when contextId or other filter parameters are not specified".
* **GetExtendedAgentCard (§3.1.11, §3.3.4, §13.3).** Available only when `capabilities.extendedAgentCard` is true; without it
  the answer is `UnsupportedOperationError` (this server used to answer `ExtendedAgentCardNotConfigured`, which is for "declared
  but not configured"). It "MUST require authentication"; the extended card "MAY include additional skills" and SHOULD NOT hold
  anything that hurts if it leaks.
* **Card signatures (§4.4.7, §8.4).** `AgentCardSignature` is `protected` (base64url JSON), `signature` (base64url) and an
  optional `header`. The card is canonicalized with RFC 8785 (JCS) after removing `signatures` and "properties with default
  values", with the rules for optional and REQUIRED fields of §8.4.1. The protected header MUST have `alg`, `typ` (SHOULD be
  `JOSE`) and `kid`, and MAY have `jku`; the signing input is `BASE64URL(protected) || "." || BASE64URL(payload)`; a verifier
  takes the key by `kid` and `jku` or from a trusted store and "SHOULD verify at least one signature".
* **The SDK serves the card in two JSON forms.** The agent card route writes serde's JSON; the JSON-RPC route
  (`GetExtendedAgentCard`) writes proto3 JSON, which drops fields that hold their default (`required: false` of an extension,
  an empty `tags`) and spells `securityRequirements` as `{"schemes": {...}}` (*observed 2026-10-07* against this server in
  `crates/adam-a2a/tests/card.rs`). The client's `AgentCard` parses both to one value.
* *Unverified:* that another SDK's canonical payload of the same card is byte for byte the one here. The specification gives
  one worked example of the default rule (a fragment, §8.4.1) and no test vector with a key and a signature.

## Decision

### 1. Push notifications

1. **Off unless the deployment turns them on and names the webhooks.** `A2A_PUSH_ALLOWED_URLS` (comma-separated URL prefixes or
   hosts) is the whole policy; an empty list is off. The card says `pushNotifications: true` only when it is not empty; off,
   the four methods and an inline config answer `PushNotificationNotSupported`, and an inline config creates no task. A config
   whose URL the policy refuses is `InvalidParams` at create time: not on the list, not `https` (`http` only to a loopback host
   and only with `A2A_PUSH_ALLOW_PRIVATE`, a development switch), credentials in the URL, a literal private, loopback,
   link-local, carrier-grade, reserved or unique-local address, `localhost`. At delivery time the policy is judged again and
   the DNS answer is filtered by the client's own resolver, so a name that resolves to a private address, now or later, is
   never connected to, and there is no gap between the check and the connect; redirects are never followed and
   `HTTP(S)_PROXY` is ignored (the check is the connection). `crates/adam-a2a/src/push/policy.rs`.
2. **A config belongs to its task and caller.** Every method first reads the task as the caller (`TaskBackend::get`): another
   caller's task, or none, is `TaskNotFound`, as for `GetTask`. The store keeps the owner. At most 16 configs per task; an id
   is at most 128 characters of `[A-Za-z0-9_.:-]`; `token` and `credentials` must be valid header values of at most 4096
   characters (refused at create, so a delivery never fails for ever on a bad header). A config in `SendMessage` is checked
   **before** the task is created and registered **after**, with the task as the baseline; with no id it gets one derived
   from the URL, and a repeated request leaves the first config alone.
3. **`token` and `credentials` are write-only.** They are stored as the client gave them and never returned (create, get and
   list answer with the scheme and without them), logged or put in `last_error`. **The store holds them in clear**, in the
   run database (`config` column or field); protect the database like the runs. Encryption at rest is not done here.
4. **Durable, at-least-once, in the run store.** The configs and each one's delivery progress live next to the run, behind
   five new **required** `Store` methods (below), so a restart or another replica loses nothing and a purged run takes its
   configs with it. The deliverer (`crates/adam-a2a/src/push/deliverer.rs`) claims due configs (a lease; the version
   compare-and-swap of `push_commit` is what keeps a cursor from going backwards), reads the task as its owner, sends the
   first event the webhook has not heard, and commits. A success makes the config due at once (the next event does not wait
   for a poll); a failure keeps **the event** pending in the cursor and retries it as it was with capped exponential backoff
   (1 s, doubling, capped at 5 min, up to a fifth of jitter); after `A2A_PUSH_GIVE_UP_AFTER_SECS` (default 1 hour) of failures
   the config is `GaveUp` with the reason recorded (never the URL or a credential), and a `410 Gone` or an address the policy
   refuses gives up at once. A config that has heard everything is polled every 2 s and is `Done` once the task is terminal.
   **A notification is a hint** (Rule 5): the deliverer polls the store; the broadcast of status events (and a created config)
   only makes a round start sooner.
5. **What is delivered.** A `statusUpdate` when the task's status changed (its state or its message, not its timestamp) and an
   `artifactUpdate` for each artifact the webhook has not heard (artifacts first, as a stream gives them), each with
   `Content-Type: application/a2a+json`, `Authorization: {scheme} {credentials}` and `A2A-Notification-Token: {token}`. The
   state a task passes through between two reads, or while every replica is down, is **not** delivered: the next event is the
   state the task is in (the store keeps the task, not a log of its statuses, and this ADR does not add one). Events seen are
   sent in order, a failed event is sent again before any later one, and a webhook may hear one twice. Live step reports and
   text chunks are streams' business, not notifications'. A config created for a task starts from what the task says now, so
   only what changes afterwards is news; one created for a task that is already terminal gets nothing.
6. **Where it runs.** In the control plane (`all`, `control-plane`) beside the server that accepts the configs, as the host
   component `push-delivery`, one loop per replica; it needs the store and the backend, not the agent. Not signing the
   notifications is a decision, not an omission: the specification asks for no signature, and a webhook verifies the
   credentials it chose.

### 2. ListTasks

1. **A store method per read.** `Store::list_runs(&RunQuery)` and `Store::count_runs`: always scoped (`ConversationScope::Prefix`
   of the caller's `<subject>:` namespace, or `Exact` for a `contextId`), by keyset (`updated_at` descending, then id), one
   indexed read per page (Postgres: `(agent, conversation_id COLLATE "C", updated_at DESC, id DESC)`; MongoDB:
   `(agent, conversation_id, updated_at, _id)`).
2. **The A2A state is read, not stored.** `failed` and `canceled` are one run status, `submitted` and `working` another, so
   the store narrows by the run statuses a state can come from and `RuntimeTaskBackend` keeps the tasks whose state is the one
   asked for, reading at most `MAX_SCAN` (500) runs per page; a page that cannot be filled in that is returned short with a
   token that continues where the scan stopped. **`totalSize` is exact for no status filter and for `completed`**, and an
   upper bound for the states that share a run status (counting them exactly would read every run).
3. **The token is a cursor, never an offset** (`crates/adam-a2a/src/page.rs`): base64url JSON of the last task's position,
   bound by a SHA-256 digest to the caller and the filters (not the page size). Another caller's token, other filters, and
   anything that does not decode are the same `InvalidParams` ("invalid page token"). It is not signed: forging one moves the
   position inside the forger's own tasks, because the query is scoped before the position is looked at.
4. `TaskBackend::list` is a **default method** answering `UnsupportedOperation`, so a backend written before it keeps
   compiling; `InMemoryBackend` and `RuntimeTaskBackend` implement it.

### 3. The extended card

The extended card is the public card plus what the deployment adds for authenticated callers: the agent folder's
`card.extended` (a `description` that replaces the public one and `skills` that are added, a skill with a public id replaces
it) or `AgentCardConfig::with_extended_card` (which can also add extensions). It is served only when configured **and** the
server authenticates (`AuthConfig::BearerTokens`): with `AllowAnonymous` it is off, the card says `extendedAgentCard: false`,
and a warning is logged. An extension only the extended card declares can be activated by an authenticated caller. Without an
extended card the method answers `UnsupportedOperation`, as §3.3.4 says.

### 4. Card signatures

`A2A_CARD_SIGNING_KEY_FILE` names a PKCS#8 PEM private key, ECDSA P-256 (`ES256`) or Ed25519 (`EdDSA`) by the key's own type,
mounted from a Secret and checked at startup (exit 78 on a key that cannot be used). The public and the extended card are
signed when the router is built (`typ: JOSE`, `kid` from `A2A_CARD_SIGNING_KEY_ID` or the RFC 7638 thumbprint, `jku` only from
`A2A_CARD_SIGNING_JKU`); no key, no signature. When signing is on the server also publishes the key set at
`GET /.well-known/jwks.json` (public, and absent otherwise), so a `jku` that points at the server works. The payload is the
card as the server holds it, canonicalized with RFC 8785 (`ryu-js` for ECMAScript number formatting, members sorted by UTF-16
code units), after removing `signatures`, `null`s, and empty arrays and objects other than the REQUIRED ones, and an
extension's `required: false` (a proto3 default); `securityRequirements` and extension `params` are signed as they are. Signer
and verifier share one function (`canonical_payload`), and `VerifyingKey::verify_card` is the helper the tests and clients use.
Why not the specification's text alone: the one card reaches a client in two JSON forms (above), so the payload is made from
the parsed card, not from either wire form.

## Consequences

* **`Store` gains seven required methods** (`push_put`, `push_list`, `push_delete`, `push_claim_due`, `push_commit`,
  `list_runs`, `count_runs`): breaking for every implementer, as Rule 2 says to state. `MemoryStore`, `PgStore` (schema version
  3: the `push` table and the `runs_list` index; the migration creates the push objects before it locks `runs`, so it takes its
  locks in the order a `push_put` does) and `MongoStore` (schema version 3: the `push` collection, the `adam_list` index)
  implement them, `purge_finished` also deletes a purged run's configs, the testkit has 15 new cases (`push_*`, `list_runs_*`,
  `count_runs_*`) and `FaultyStore` can fail each (`Method` has 7 more variants: it is not `#[non_exhaustive]`).
* **New port** `adam_a2a::push::PushStore` (the configs and progress, by task id), with `InMemoryPushStore` (feature
  `test-util`) and `adam_a2a_runtime::StorePushStore` over the `Store`. `ServerOptions` gains `push` and `card_signer`;
  `AgentCardConfig` gains `extended`; `adam-service` gains `A2aSettings` (read by the roles that serve A2A) and
  `ServeError::Push` (exit 78).
* **Environment:** `A2A_PUSH_ALLOWED_URLS`, `A2A_PUSH_ALLOW_PRIVATE`, `A2A_PUSH_GIVE_UP_AFTER_SECS`,
  `A2A_PUSH_REQUEST_TIMEOUT_SECS`, `A2A_CARD_SIGNING_KEY_FILE`, `A2A_CARD_SIGNING_KEY_ID`, `A2A_CARD_SIGNING_JKU`. Nothing is on
  by default and the default chart render is byte for byte what it was. The chart's `a2a.push.*` and `a2a.cardSigning.*`
  values render them for the pods that serve A2A (the front with `topology: split`), and mount the key's Secret read-only.
* New dependencies of `adam-a2a`: `reqwest` (the delivery client), `aws-lc-rs` and `rustls-pki-types` (the signature; both
  already in the tree), `ryu-js` (small, no dependencies), `base64` and `uuid`.
* Proved against real databases on 2026-10-07: the conformance suite (44 cases with the owner-field one) on PostgreSQL 16 and on a
  standalone `mongod` 7.0.14 (`fastdl.mongodb.org`), and the restart scenario of the push loop over PostgreSQL
  (`notifications_survive_a_restart_over_postgres_too`). The Mongo run found one bug, fixed: `limit(0)` means "no limit" to
  MongoDB.
* Not done: encryption of stored webhook credentials; signing the notifications; a log of every status a task passed through
  (so a notification can skip a state, above); per-caller extended cards (one extended card for all authenticated callers).
* A send whose configuration names the config the way earlier drafts did (`pushNotificationConfig`) is refused with
  `InvalidParams` and starts nothing: the SDK reads requests as proto3 JSON and would drop the member silently, so the client
  would wait for notifications nobody registered. The 1.0 name is `taskPushNotificationConfig`.

## Alternatives rejected

* **Deliver from the live event stream** (`TaskBackend::subscribe` per config). It ends at the first interruption and does not
  survive a restart; the durable cursor does.
* **A table of every status change** so each is delivered. It makes `GetTask` and the notifications two truths, and a second
  write on every commit of every run for a feature most deployments leave off.
* **The SDK's `HttpPushSender`.** No DNS check, follows redirects, wrong content type, no status to retry on.
* **Offset page tokens**, or a token with an HMAC key per process (it would break paging across replicas); a binding digest
  with owner-scoped queries gives the safety that matters.
* **Signing the card from the specification's text alone** (the serialized card as it happens to be written): the two wire
  forms above would verify differently.
