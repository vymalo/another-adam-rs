# 0067. A model call through an injector names its run, holds no key, and a refusal parks the run

Status: **Accepted** (2026-11-21). The design of another-agentic-system#334 slice S8 ("adam holds no model key"), which
the owner moved into the first release on 2026-11-19: *"Injector: adam never holds the model credential."* It builds on
[ADR 0058](0058-the-models-credentials-come-from-a-provider-asked-for-each-call.md), whose *Not built here* left the
injector's provider to this decision. The gateway's side is another-agentic-system#334 B1 (the broker's lease lookup) and
S9 (the injector in front of the model endpoint); its interface below was agreed with the session building them on
2026-11-21, and S9's spike confirms the points marked *(spike)*.

## Context

In the deployments of another-agentic-system#334 an agent never holds a model credential. Its model calls go to an
internal gateway: Agent Router with Authorino in front of its routes, which authenticates the caller, looks up the
credential the orchestrator leased for **this run** of **this person** from the broker, adds `Authorization` to the
upstream request only, and strips any `Authorization` the agent sent (those products' features *verified 2026-11-19* in
another-agentic-system#334, not re-checked here). The agent's only part is to say which run a call belongs to.

Three things are missing in adam today:

1. **A call does not know its run.** `ModelCredentials::headers` gets a `CredentialRequest { method, url, attempt }`
   (`crates/adam-model/src/credentials.rs`), and `ModelRequest` (`crates/adam-model/src/types.rs`) carries no run. The
   one run the orchestrator binds at the broker is the A2A task it created, which is adam's **root** run (the task id is
   the root run's id, `crates/adam-a2a-runtime/src/ids.rs` `task_id_for`). A local subagent's run is another run of the
   same task; the LLM agent already knows its root (`Conversation::root_run`, `crates/adam-llm-agent/src/conversation.rs`,
   written only by in-process code and used as `root_run.unwrap_or(ctx.run_id())` in `agent.rs`).
2. **A refusal ends the run.** A `401` or `403` is `ModelError::Auth` and a `402` an `InvalidRequest`
   (`crates/adam-model-openai/src/errors.rs`), and both fail the run (`crates/adam-llm-agent/src/agent.rs`, "Failure
   handling"). Behind the gateway those answers say that the run is not bound yet, that its lease expired, or that the
   person's usage limit is reached: each is cured outside the run, by the orchestrator or the person, never by failing
   it. The owner decided on 2026-11-17 that such a run pauses and resumes where it stopped.
3. **The gateway's certificate.** A TLS path inside the cluster would use a CA no public root store holds.

## Decision

1. **`MODEL_AUTH=injector`: no key, the run's id instead.** `adam-model` gains the provider `InjectedRun`, which gives
   one header, `X-Run-Id`, whose value is the call's root run id, and no `Authorization`. With `injector`,
   `MODEL_API_KEY` must be unset or empty (exit 78 otherwise), as with `device`. There is **no fallback**: nothing in the
   process can send a key on that path.
   * The run id is an **opaque capability, not a credential**: the broker answers only for the (run, agent, person) it
     bound, the agent and the person come from the lease and never from a header, and the edge strips an inbound
     `X-Run-Id`. Any pod the network policy admits to the gateway could send another run's id; that is the platform's
     namespace trust domain, and the lease bounds the loss. Authenticating each agent to the gateway is a later decision.
   * The run id may be logged; it is in no error as a secret, since it is none.
2. **A call names its root run, from in-process state.** `ModelRequest` gains `run: Option<String>` and
   `CredentialRequest` gains `run: Option<&str>`. The LLM agent fills it with `Conversation::root_run`, else the run's
   own id: never from a message's context, which a person can set. A child session's round names the root run of the
   task that opened its session (kept on the session when it opens), since that is the task the orchestrator bound.
   `InjectedRun` requires the run: **a call with no run is refused** before anything is sent (`InvalidRequest`, "a model
   call through the injector names no run").
3. **A refusal is read by its problem type, and a provider says whether it parks.** The gateway answers a refusal as
   `application/problem+json` (RFC 9457), and adam matches its `type` slug, never the status alone:

   | Slug *(spike)* | Status | Meaning | adam |
   |---|---|---|---|
   | `unauthenticated` | 401 | the caller is not authenticated | park, timer |
   | `run_not_bound` | 403 | no lease for this run yet | park, timer |
   | `lease_expired` | 403 | the lease ended | park, timer |
   | `over_limit` | 402 | the person's usage limit is reached | park, until woken |
   | `rate_limited` | 429 + `Retry-After` | a per-minute cap | park, timer of at least `Retry-After` |
   | `unavailable` | 503 + `Retry-After` | the broker cannot decide | park, timer of at least `Retry-After` |
   | `model_locked` | 403 | this model is not allowed for the person | not a park: `ModelError::ModelLocked` (decision 7) |

   `adam-model-openai` reads the slug into a new `ModelError::Refused { status, slug, retry_after }`, which holds **no
   body and no header**. Whether a refusal parks is the **provider's** answer, a new provided method of
   `ModelCredentials`, `parks(&Refusal) -> Park` (`Park::No` by default, `Park::OnTimer(d)` or `Park::UntilWoken`):
   `InjectedRun` answers by the table; `StaticBearer` and the device grant answer `No`, so **a deployment with its own
   key fails as before** (a wrong key is a failed task, not a run that waits for ever), as ADR 0058 decision 3 promised.
   A connection to the gateway that fails, under `injector`, parks on a timer too: it is the gateway that cannot be
   reached, and the runtime's retries of a model error are replaced by the park's timer for those calls.
4. **A park keeps the run and makes the same call again.** The LLM agent records a model wait beside its tool waits:
   `Conversation::model_wait: Option<ModelWait { reason, status, slug, parks }>`, not a `PendingWait` variant (a model wait
   is tied to no tool call). The conversation is kept as it was before the call; when the run wakes, the same model call
   is made again (the journal is keyed by step sequence, so the call is a new step). The same holds for the summary call
   of a compaction.
   * **A timer park** waits with a backoff (30 s, doubling, at most 15 min, at least `Retry-After`) and its task is
     `working`, with a status message naming the reason. Time parked on a timer counts towards `MODEL_PARK_LIMIT`
     (default 24 h, measured on the journaled clock); past it the run **fails** with the last reason, so a wrong base URL
     or a gateway that never binds the run cannot keep it for ever.
   * **A park until woken** (`over_limit`) waits like a question: its task is `input-required` with the reason as its
     message, it never fails on its own, and the run's deadline is paused as for a question.
   * **Waking.** A message to the task wakes a parked run; its text, when it has any, is the person's turn added before
     the call, as for any message to a parked run. A message whose metadata holds `adam.resume: true` wakes the run and
     **adds nothing** to the conversation, so the orchestrator can resume a run after binding it or after the person
     raised the limit without speaking for them; such a message may have no text.
   * **Cancelling** ends a parked run as it ends any wait.
5. **A child run.** A local subagent's refusal parks the child the same way, and its parent waits on it as on any child.
   The root's task shows the descendant's wait: `input-required` with its reason when the child waits until woken, and a
   status message naming it when the child waits on a timer. A message or a resume to the root is forwarded to its
   descendants parked on the model. A child session's round is its own task and shows its own state.
6. **Who never parks.** An **unattended** run (a schedule's firing, ADR 0060 slice 6c) has nobody to wake it: a refusal
   fails it with the reason, as the coder already ends a firing that would wait. Under `injector`, nothing binds a firing
   at the broker yet, so a firing's model calls fail as unauthenticated until the orchestrator binds firings (open). A
   remote adam subagent called over A2A runs in its own deployment, with its own gateway and its own bindings: its
   `auth-required` stays a failed call for the caller (`crates/adam-assembly/src/remote.rs`), unchanged.
7. **`model_locked` is not a park.** The person may not use this model at all; waiting cures nothing. It is
   `ModelError::ModelLocked`, class `Rejected`: the switch to the profile's fallback model is the model profiles' slice
   ([ADR 0064](0064-model-profiles-reasoning-effort-and-output-cap-follow-the-model.md)); until that switch is built the
   run fails naming the model.
8. **What a park records.** The journal, the step's error and the task's message hold the reason, the status and the
   slug. Never a header, never the body, never a credential. A refusal's report is built from those three fields, not
   from the body the client read.
9. **The gateway's CA, for later.** The first cut is plain HTTP on the cluster's service network, the gateway admitted to
   the agents' pods by a network policy (another-agentic-system#334 S9 and S10): no CA to mount. `MODEL_CA_FILE` names a
   PEM file of certificates trusted for the **model endpoint** (not the device grant's issuer), beside the default roots,
   for the day the path crosses a boundary that needs TLS; a file that cannot be read or holds no certificate is exit 78.
   Mounting it into an agent's pod is the platform's work.

## Alternatives

* **Name the run with a protected context key that children inherit** (as `adam.unattended`). A message can set or
  delete such a key, and a continuing task copies it from the task it continues, so a follow-up would name the previous
  task at the broker; the in-process root is what the runtime knows for certain.
* **Park on every provider's refusal.** A wrong key or a forbidden model would leave every task waiting for an answer
  that never comes, where today it fails and someone acts. Only a provider whose refusals are cured outside the run
  parks.
* **Fail the run and let the orchestrator start a new one.** The run's work (tool results, files, child runs) is lost
  for a cause outside the run, and is done twice.
* **Send the run id in the body's `metadata`.** The gateway's authorization runs on headers before the body is read,
  and a body field would reach the model provider.
* **A `PendingWait::Model` variant.** `PendingWait` is public, matched outside the workspace and tied to a tool call
  (`call_id()`); a field of its own breaks neither.

## Consequences

* `adam-model`: `InjectedRun`, `Refusal`, `Park`, `ModelCredentials::parks` (a provided method: no implementer breaks),
  `ModelRequest::run` (`ModelRequest` keeps `Default`), `CredentialRequest::run` (`CredentialRequest` has no `Default`:
  a struct literal that names its fields outside the workspace breaks, and the release notes say so),
  `ModelError::Refused` and `ModelError::ModelLocked` (`ModelError` is `#[non_exhaustive]`, so new variants break no caller).
* `adam-model-openai`: the run's header, the problem's slug, `MODEL_CA_FILE`.
* `adam-llm-agent`: `Conversation::model_wait`, the park and the wake, the summary call, the forwarding to a parked child;
  the "Failure handling" list. A build rolled back past this change reads a parked run's state without its model wait.
* `adam-runtime`: the backoff timer and `MODEL_PARK_LIMIT`; the deadline paused during a park until woken.
* `adam-a2a-runtime`: a model park's task state and message (`convert.rs`, also its default prompt and the
  `input-required` rewrite after a delivery), `auth-required` listed (`list.rs`), and `adam.resume`.
* `adam-coder`: a park until woken stops background commands as a question does; a timer park keeps them.
* `adam-service`: `MODEL_AUTH=injector`, `MODEL_PARK_LIMIT`, `MODEL_CA_FILE` (`docs/reference/environment.md`);
  `docs/reference/parity.md` gains "model calls name their run", "a gateway's refusal parks the run" and "a CA for the
  model endpoint" for the coder, the folder agent and the engine (whose host gives its own provider, ADR 0058, and
  decides with `parks` whether its refusals park).
* A deployment that changes nothing keeps its key and its failures.

## Built in

| Slice | What is true after it | Proof |
|---|---|---|
| S8a | `InjectedRun`, `MODEL_AUTH=injector`, `MODEL_CA_FILE` and the two `run` fields: every model call of a run, its subagents' and its sessions' included, carries the root's id and no `Authorization` | a scripted model behind a test gateway that records each call's headers: the root's id on every call, a subagent's and a round's included; no `Authorization`; a call with no run sends nothing; with a test CA the endpoint is reached only with `MODEL_CA_FILE` |
| S8b | decisions 3 to 8 | the test gateway answers each slug of the table in turn: the run parks with the state and reason above, keeps its conversation, and the same call is made again after the timer, a message or `adam.resume` (which adds no turn); `model_locked` and an unattended run fail; a parked child shows on the root and is woken through it; `MODEL_PARK_LIMIT` fails a run that never binds; `StaticBearer` fails as before; nothing of a header or body in the journal |
| S8c | the coder's chart sets `MODEL_AUTH=injector` when its values say so | the chart's goldens; another-agentic-system#334 S10 puts the agents behind the gateway |

## Open

* **Firings under the injector:** which run a firing names, and the orchestrator binding it.
* **Authenticating each agent to the gateway** beyond the namespace's trust (decision 1).
