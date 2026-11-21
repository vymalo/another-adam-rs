# 0067. A model call through an injector names its run, holds no key, and a refusal parks the run

Status: **Accepted** (2026-11-21). The design of another-agentic-system#334 slice S8 ("adam holds no model key"), which
the owner moved into the first release on 2026-11-19: *"Injector: adam never holds the model credential."* It builds on
[ADR 0058](0058-the-models-credentials-come-from-a-provider-asked-for-each-call.md), whose *Not built here* left the
injector's provider and "a signed-out person mid-run" to this decision. The gateway's side (the broker's lease lookup, the
injector in front of the model endpoint) is another-agentic-system#334 B1 and S9; the points this ADR takes from it are
marked *unverified* until those slices fix them.

## Context

In the deployments of another-agentic-system#334 an agent never holds a model credential. Its model calls go to an internal
gateway (Agent Router, with Authorino in front of its routes), which authenticates the agent's pod, looks up the
credential the orchestrator leased for **this run** of **this person**, adds `Authorization` to the upstream request only,
and strips any `Authorization` the agent sent. The agent's only part is to say which run a call belongs to. The run id is
not a secret: the broker answers only when the caller is the agent the run is bound to.

Three things are missing in adam today:

1. **A call does not know its run.** `ModelCredentials::headers` gets a `CredentialRequest { method, url, attempt }`
   (`crates/adam-model/src/credentials.rs`), and `ModelRequest` (`crates/adam-model/src/types.rs`) carries no run. The
   one run the orchestrator binds at the broker is the A2A task it created, which is adam's **root** run (the A2A task id
   is the root run's id, `crates/adam-a2a-runtime/src/backend.rs`). A local subagent's run and a child session's run are
   other runs of the same task, and their calls must name the root.
2. **A refusal ends the run.** A `401` or `403` is `ModelError::Auth`, a `402` an `InvalidRequest`, and every other model
   error that is not retryable fails the run (`crates/adam-llm-agent/src/agent.rs`, "Failure handling"). In a gateway
   deployment those answers say that the run's credential is not there yet, has expired, or has reached its spending
   limit. Each one is cured by something outside the run (the orchestrator binding the run again, the person signing in
   or raising the limit), never by failing it.
3. **The gateway's certificate.** The internal path's TLS uses a CA from cert-manager, which no public root store holds.

## Decision

1. **`MODEL_AUTH=injector`: no key, the run's id instead.** `adam-model` gains the provider `InjectedRun`, which gives one
   header, the call's run id, under the name `MODEL_INJECTOR_RUN_HEADER` (default `X-Run-Id`, *unverified* against S9),
   and no `Authorization`. With `injector`, `MODEL_API_KEY` must be unset or empty (exit 78 otherwise), as with `device`.
   There is **no fallback**: nothing in the process can send a key on that path.
2. **A call names its root run.** `ModelRequest` gains `run: Option<String>`, the id of the root run of the call, and
   `CredentialRequest` gains `run: Option<&str>`, the same value. The LLM agent fills it from a protected context key,
   `adam.root_run`, which the runtime writes when a root run starts and which every child run inherits (as
   `adam.unattended` is, ADR 0060 slice 6c): a local subagent's calls and a child session's calls name the root. A
   provider may read it; `InjectedRun` requires it, and **a call with no run is refused** before anything is sent
   (`InvalidRequest`, "a model call through the injector names no run"). The run id is not a secret, so it may be logged;
   it is never a credential.
3. **The gateway's CA.** `MODEL_CA_FILE` names a PEM file of certificates trusted for the model endpoint, beside the
   default roots. It applies to every `MODEL_AUTH`. A file that cannot be read or holds no certificate is exit 78 at
   startup.
4. **A refusal parks the run; it never fails it.** In the LLM agent, a model call that ends in
   * `ModelError::Auth` (a `401` or `403`, after ADR 0058's one retry): the run **parks**, its A2A task `auth-required`;
   * a `402`, now `ModelError::LimitReached` (decision 5): the run parks, its task `input-required`;
   * with `MODEL_AUTH=injector` only, a retryable error whose retry policy is spent (the gateway cannot be reached): the
     run parks with a timer (`MODEL_INJECTOR_RETRY_AFTER`, default 300 s) and its task `working`, so the next attempt is
     made without anybody acting, and a message wakes it sooner.

   The park is a new `PendingWait::Model { reason, status }` beside `Run`, `Question`, `Remote` and `Session`
   (`crates/adam-llm-agent/src/conversation.rs`). The conversation is kept as it was before the call: **the same model
   call is made again** when the run wakes. A message to the task wakes it; its text, when it has any, is added as the
   person's turn before the call, as any message to a parked run is. Cancelling the run ends it, as for any wait.
5. **What a park records.** The journal and the task's status message hold the reason (`not authorized by the model
   gateway`, `the model gateway's limit is reached`, `the model gateway cannot be reached`), the status code and, when the
   answer is OpenAI's error object, its `error.code` (`budget_exhausted`, `invalid_token`). Never a header, never the
   body, never a credential. `ModelError::LimitReached` is classed `Rejected` (valid input, but the endpoint's state forbids the
   call: no retry helps), so a host that reads classes and not the park still never retries it.
6. **A child run.** A child run's refusal parks the child the same way. Its parent waits on it as on any child
   (`PendingWait::Run`), so the root's task stays `working` and its status message names the child that waits and why:
   one waiting run in a task is enough for the person to be told. The orchestrator's re-binding or the person's message
   reaches the root; a message to a root whose child is parked on the gateway wakes that child (the root has nothing of
   its own to ask).
7. **Any provider.** Decisions 4 to 6 do not depend on `injector`: a signed-out device grant (ADR 0058) and a key that
   reached a limit park in the same way. Only the timer of an unreachable endpoint is the injector's, since a deployment
   with its own key is told of a dead endpoint by its retries, as before.

## Alternatives

* **Fail the run and let the orchestrator start a new one.** The run's work (tool results, files, child runs) is lost
  for a cause outside the run, and the person pays for it twice. The owner decided the run pauses and resumes where it
  stopped.
* **Send the run id in the body's `metadata`.** The gateway's authorization runs on headers before the body is read,
  and a body field would reach the model provider.
* **Tag with the run of the call, not the root.** The orchestrator binds only the task it created; a subagent's run id
  is adam's own, which the broker cannot know.
* **A transport port under the HTTP client** (ADR 0058's alternative). Still not needed: a header and the status are
  all the injector asks for.

## Consequences

* `adam-model`: `InjectedRun`, `ModelRequest::run`, `CredentialRequest::run`, `ModelError::LimitReached`. Adding a field to
  `CredentialRequest` and `ModelRequest` breaks a struct literal outside the workspace that names every field; both keep
  `Default`, and the release notes say it.
* `adam-model-openai`: the run's header, the CA file, `402` read as `LimitReached`.
* `adam-runtime`: the protected key `adam.root_run`, written for a root run and inherited.
* `adam-llm-agent`: `PendingWait::Model` and the park; the "Failure handling" list changes for `Auth` and `402`.
* `adam-a2a-runtime`: a run parked on the model is `auth-required` or `input-required` with its reason.
* `adam-service`: `MODEL_AUTH=injector`, `MODEL_INJECTOR_RUN_HEADER`, `MODEL_INJECTOR_RETRY_AFTER`, `MODEL_CA_FILE`
  (`docs/reference/environment.md`); `docs/reference/parity.md` gains the rows "model calls name their run", "a model
  refusal parks the run" and "a CA for the model endpoint", for the coder, the folder agent and the engine (whose host
  gives its own provider, ADR 0058, and may set `adam.root_run` itself).
* A deployment that changes nothing keeps its key; its runs park on a `401`, `403` or `402` instead of failing.

## Built in

| Slice | What is true after it | Proof |
|---|---|---|
| S8a | `InjectedRun`, `MODEL_AUTH=injector`, `MODEL_INJECTOR_RUN_HEADER`, `MODEL_CA_FILE`, `adam.root_run` and the two `run` fields: every model call of a run, its subagents and child sessions included, carries the root's id and no `Authorization` | a scripted model behind a test gateway that records each call's headers: the root's id on every call, a subagent's included; no `Authorization`; a call with no run sends nothing; a gateway with a test CA is reached only with `MODEL_CA_FILE` |
| S8b | the park of decisions 4 to 6 | the test gateway answers `401`, `402` and goes away in turn: the run parks each time with the A2A state and reason above, keeps its conversation, and the same call is made again after a message (or the timer); a parked child's reason reaches the root's status; nothing of a header or body in the journal |
| S8c | the coder's chart sets `MODEL_AUTH=injector` and mounts the CA when its values say so | the chart's goldens; S10 (another-agentic-system) puts the agents behind the gateway |

## Open

* **The gateway's answers** (another-agentic-system#334 S9): the header's name, and which status and `error.code` mean
  "not bound", "lease expired" and "limit reached". Decision 4 parks on any `401`, `403` or `402`, so a different split
  changes only the reasons shown.
