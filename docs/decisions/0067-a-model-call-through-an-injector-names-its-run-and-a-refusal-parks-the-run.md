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
   task that opened its session, kept in a new `root` field of the session's header (`SessionHeader`,
   `crates/adam-runtime/src/session.rs`) when it opens, since that is the task the orchestrator bound; a session a person
   opened directly has no opener, and its rounds name their own task, which the orchestrator binds as it binds any task
   it creates.
   *Amended 2026-11-21 (S8a review):* a round names the task that **sent** it, not the one that opened its session:
   `subagent_send` writes the sending task's run into the round's header, since that is the task the orchestrator
   bound for the round (the opener's lease may be over), and a person's later round names what the latest header
   names. Everything under a round names what the round names: its local subagents and the sessions it opens are
   handed that run in their first message (`model_run`, written by in-process code only), so the whole tree under a
   task or a round names one run.
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

   The gateway refuses before a response streams, so a refusal is always an HTTP answer; an error object inside a stream
   is the upstream model's and is read as today. For an answer of status 400 or above, `adam-model-openai` builds a
   `Refusal { status, slug, retry_after }` (the slug from a problem body, when there is one) and asks the provider
   whether it parks: a new provided method of `ModelCredentials`, `parks(&Refusal) -> Park` (`Park::No` by default,
   `Park::OnTimer(d)` or `Park::UntilWoken`).
   * `Park::No`: the error is what it is today (`Auth` for a 401 or 403, `RateLimited`, and so on), so **a deployment
     with its own key fails as before** (a wrong key is a failed task, not a run that waits for ever), as ADR 0058
     decision 3 promised. `StaticBearer` and the device grant answer `No`.
   * Otherwise the error is the new `ModelError::Refused { status, slug, retry_after, park }`, which holds **no body and
     no header**, classed by its status (`401` and `403` `Unauthenticated`, `402` `Rejected`, `429` `RateLimited`, `503`
     `Transient`). `InjectedRun` answers by the table, and reads a `401` or `403` with no slug as `unauthenticated` (an
     edge's own denial may carry no problem body, *unverified*).
   * `model_locked` is `ModelError::ModelLocked`, class `Rejected`, for every provider (decision 7).

   *Amended 2026-11-21 (S8b-1):* what the decision left open, as built. The problem type is read only from a body
   whose `Content-Type` is `application/problem+json`, and has three states (`ProblemType`): **absent** (no body, no
   `type` member, `about:blank`), a **slug** (the `type`'s last path segment, or the member as is with no `/`, when it
   is a word of at most 64 letters, digits, `_`, `-` and `.`: the one part of a body an error keeps), or
   **unreadable** (any other `type`: a `urn:`, a query, a fragment, a trailing `/`, not a string; a body that is not a
   JSON object). A slug of the table decides whatever the status; a slug not in it, and an unreadable type, park
   nothing, on a `401` or `403` too: only a refusal with no problem type reads as `unauthenticated`. A timer park is at
   most `MAX_PARK` (24 h) whatever `Retry-After` says, and a `Retry-After` too large for a `u64` is the longest wait,
   not none. `ModelLocked` names the request's model alias (`ModelLocked { model }`), so the failure of decision 7
   names it. `Refused`'s `retry_after` is its `Classify::retry_after` for a `429` alone. A TLS handshake that fails (a
   `rustls::Error` in the chain) is `Transient`, as before, never `Unreachable`: waiting cures neither a CA not
   trusted nor a name that does not match; a connect timeout is a timeout. `Ctx::retries_left() -> u32` is the
   worker's own decision (`retry::decide`, which both call) for an error of class `Transient` that names no policy,
   which is what the LLM agent returns for a model call.

   **A gateway that cannot be reached.** The client reports a failed connection (not a timeout, not a stream cut off)
   as the new `ModelError::Unreachable`, class `Transient`, so the runtime retries it as today and a short drop costs
   seconds. Under `injector` only, when the step's retries are spent, the LLM agent parks on a timer instead of failing:
   `Ctx` gains `retries_left()`, which the runtime knows from the step's policy. A timeout or a cut stream keeps today's
   retries and failure, so a model slower than the timeout is never called again for a day.
4. **A park keeps the run and makes the same call again.** The LLM agent records a model wait beside its tool waits:
   `Conversation::model_wait: Option<ModelWait { reason, status, slug, park, parked_since }>` (`parked_since` is the
   journaled time of the first park in a row, cleared by the next call that succeeds), not a `PendingWait` variant (a model wait
   is tied to no tool call). The conversation is kept as it was before the call; when the run wakes, the same model call
   is made again (the journal is keyed by step sequence, so the call is a new step). The same holds for the summary call
   of a compaction.
   * **A timer park** waits with a backoff (30 s, doubling, at most 15 min, at least `Retry-After`) and its task is
     `working`, with a status message naming the reason. From `parked_since`, parks on a timer in a row are bounded
     by `MODEL_PARK_LIMIT` (default 24 h, on the journaled clock); past it the run **fails** with the last reason, so a
     gateway that never binds the run cannot keep it for ever. The backoff and the limit are the LLM agent's: the park
     is a `wake_at` to the runtime, which knows nothing of models. Timer parks count against a child run's time limit, as
     any timer does (only a wait with no timer pauses it); a child with a short limit is cancelled during a long outage.
   * **A park until woken** (`over_limit`) waits like a question: its task is `input-required` with the reason as its
     message, it never fails on its own, and the run's deadline is paused as for a question.
   * **Waking.** The A2A backend accepts a message to a task whose run has a `model_wait`, **timer or not** (today a
     message to a run on a timer is refused unless it steers). Its text, when it has any, is the person's turn added
     before the call, as for any message to a parked run. A message whose metadata holds `adam.resume: true` is a
     **resume**, agreed with another-agentic-system#334 B1 on 2026-11-21:
     * it wakes the run and **adds nothing** to the conversation, so the orchestrator can resume a run after binding it,
       or after the person raised the limit, without speaking for them; it may have no text;
     * it continues the same run, so the same root run id and the same binding; it never starts a run;
     * it is accepted only by a run with a `model_wait`, or waiting on a child that has one; any other task, a finished or
       canceled one included, answers `InvalidParams` (`-32602`) with the reason "the task is not parked on the model",
       and nothing is written: a resume never answers a question nor brings back a finished run;
     * it is authenticated as any message, and it cures nothing by itself: the call it repeats goes through the gateway,
       and a lease still refused parks the run again.

     B1's side: the orchestrator binds the run at the broker when it creates the task, before adam's first call, so a call
     only uses a lease; `run_not_bound` covers the moment between the two. A resume is no message of the person's for
     the orchestrator's own rate limits.
   * **Cancelling** ends a parked run as it ends any wait.

   *Amended 2026-11-21 (S8b-2a):* what the decision left open, as built for a run that calls the model itself. A
   refusal of status `401` or `403` makes the task **`auth-required`**, with the reason as its message, timer or not
   (every such park of `InjectedRun` is on a timer); a park until woken is `input-required`, any other timer park
   `working` with the reason. A "row" of parks is parks of one kind in a row: `parked_since` is the first of them, and
   `ModelWait` also keeps `parks` (the backoff's step) and `wake_at`; a park until woken followed by a park on a timer
   starts a new row, so the time a person took to raise a limit is not counted against `MODEL_PARK_LIMIT`. A resume
   names its `taskId` (one with none is `InvalidParams`), is read before the inbound function, and may have **no
   part** at all (the server lets such a message through); its text, if any, is not read. A message to a run parked
   on its compaction's summary call is read once the summary is made: the run takes its inbox when it parks there, the
   words kept for after the summary, so no message keeps a parked run being stepped. `MODEL_PARK_LIMIT` is whole
   seconds, or a number with `s`, `m`, `h` or `d`, from 60 s to 30 d, and a park never waits past it: a wait longer than
   what is left of the limit ends at the limit, where a refusal fails the run.

   *Amended 2026-11-21 (S8b-2a review):*
   * **A resume is a wake-up only.** It is delivered as `adam_runtime::RESUME_KIND`, which a run that works or finishes
     does not count as a message that arrived, so a resume that lands during the woken call never adds a turn. A resume
     is accepted only by a run that is **parked** on the model: one a message or a resume just woke, making its call
     again, is working and answers `NOT_PARKED`.
   * **A local subagent fails on a park until woken.** Nothing wakes a child until S8b-2b forwards a resume (decision
     5), and its deadline is paused, so its whole task would wait for ever: a child run that is no session's round
     fails with the reason, as an unattended run does, and its parent's model gets an error result. A child's timer
     parks stay, bounded by `MODEL_PARK_LIMIT` and its time limit; the root shows no reason for them until S8b-2b.
     *(Superseded by the S8b-2b amendment below: a child parked until woken now waits, and its parent shows it and
     wakes it through a forwarded resume.)*
   * **An unattended run fails on a refusal of status 401 or 403** too, timer or not: nothing binds a firing at the
     gateway yet (decision 6), so waiting cures nothing. Its other timer parks (`rate_limited`, `unavailable`, a gateway
     that cannot be reached) still wake on their own.
   * A model wait this build cannot read (a park kind a later build adds) loads as none, and the rest of the state with
     it.
5. **A child run.** A local subagent's refusal parks the child the same way, and its parent waits on it as on any child.
   * **The parent learns of it** by a notice from the child's park, as it learns of a child's reports
     (`ChildStatus::from_report`, `crates/adam-runtime/src/child.rs`), and keeps the reason with its pending child.
   * **The root's task shows the descendant's wait:** `input-required` with its reason when the child waits until woken,
     and a status message naming it when the child waits on a timer.
   * **Only a resume goes down, never text.** A message or a resume to a run that waits on a child parked on the model
     sends that child a resume; the text of a message stays with the run that got it, as today. Each level forwards to its
     own pending child, so a grandchild is reached through its parent. The resume's message id is derived from the one
     that woke the parent, so a replayed step sends it once.
   * A child session's round is its own task and shows its own state.

   *Amended 2026-11-21 (S8b-2b):* what the decision left open, as built. The notice is the inbound kind
   `adam.child.parked`, whose id is the child's run id and whose payload is not read: the runtime sends it after a
   **stepped** park commit only (never from its own look at a parked child, which parks it again without stepping it),
   and a run that finishes ignores it as it ignores a finished notice. The parent reads the child in one record
   (`Ctx::child_look`) and keeps what it waits on as a copy beside its pending child (`PendingRun::model_wait`, in the
   head, so a front reads it at `/pending_wait/model_wait`); the copy holds the child's own model wait or the copy the
   child keeps of **its** child, so a grandchild's park is shown one level at a time. A child's park until woken makes
   the parent wait with **no timer**, as a question does, so `RunView::waiting` says it and the root's task is
   `input-required` with the reason; a child's timer park keeps the parent's 60 s look and the root's task is `working`
   with the reason. A child's refusal of status 401 or 403 is **not** the root's: the root's task stays `working` (a timer
   park) with the reason, never `auth-required`, since the gateway refused the child's call and not the root's. A message
   or a resume to the root is accepted (a message's text is added at the root, behind the call the child answers) and the
   root forwards **one** resume (`RESUME_KIND`) with an id derived from the waking message's id and the child's run id;
   the copy is cleared when it forwards, and rewritten at the next look. The child drops a resume whose id it has
   read (`read_ids`) and, when that was the whole wake, parks again without a call, so a replayed forward is no second
   call. A local subagent's park until woken no longer
   fails the child (the S8b-2a amendment's arm is gone): it waits, and only its parent's forwarded resume wakes it. The
   coder ends its background commands on an until-woken park of a child it waits on, as on its own (a timer park keeps
   them, bounded by `MODEL_PARK_LIMIT`). A notice that is lost costs the parent its next look at a timer park; for an
   until-woken park, the child cannot finish on its own, so no timer is kept and a lost notice is not cured by one.
6. **Who never parks.** An **unattended** run (a schedule's firing, ADR 0060 slice 6c) has nobody to wake it: a refusal
   that would park until woken fails it with the reason, as the coder already ends a firing that would wait; a timer
   park still wakes on its own. Under `injector`, nothing binds a firing
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

* `adam-model`: `InjectedRun`, `Refusal`, `Park`, `ModelError::Unreachable`, `ModelCredentials::parks` (a provided method: no implementer breaks),
  `ModelRequest::run` (`ModelRequest` keeps `Default`), `CredentialRequest::run` (`CredentialRequest` has no `Default`:
  a struct literal that names its fields outside the workspace breaks, and the release notes say so),
  `ModelError::Refused` and `ModelError::ModelLocked` (`ModelError` is `#[non_exhaustive]`, so new variants break no caller).
* `adam-model-openai`: the run's header, the `Refusal` asked of the provider, `ModelError::Unreachable` for a failed
  connection, `MODEL_CA_FILE`.
* `adam-llm-agent`: `Conversation::model_wait`, the park, its backoff and `MODEL_PARK_LIMIT`, the wake, the summary call,
  the child's notice and the forwarded resume; the "Failure handling" list. `Conversation` denies no unknown field, so a
  build rolled back past this change reads a parked run's state, without its model wait. A parent that is not an
  `LlmAgent` forwards no resume, so a child's park until woken now waits until a cancel (its parent's deadline failing it
  cancels it), where S8b-2a failed it with the reason: a behaviour change for hand-written `adam-embed` agents that wait
  on children.
* `adam-runtime`: `Ctx::retries_left()`; `Ctx::child_look` (the caller's child in one read: its status and the head
  of its state) and `Ctx::resume_child` (a checked wake of the caller's child), `ChildLook` and `CHILD_PARKED_KIND`
  (the notice of a child's park, after its own commit); `SessionHeader::root`; nothing else (a park is a `wake_at`).
* `adam-a2a-runtime` and `docs/reference/a2a-server.md` (a section "A run parked on the model", which names
  `adam.resume`, the contract it is; a second key of its kind would make it an extension of its own): a model park's task state and message (`convert.rs`, also its default prompt and the
  `input-required` rewrite after a delivery, `backend.rs`), a park shown through the parent that waits on it
  (`model_park.rs`: the copy of the child's wait is a model wait for a resume and a reason, and never
  `auth-required`), `auth-required` listed (`list.rs`), a message accepted for a
  run with a `model_wait` on a timer (`backend.rs`), and `adam.resume`, which may have no text (`convert.rs` and
  `vymalo.rs`, which refuse a message with no text today).
* `adam-coder`: a park until woken stops background commands as a question does, its own or a child's it waits on; a
  timer park keeps them, for at most `MODEL_PARK_LIMIT`.
* `adam-service`: `MODEL_AUTH=injector`, `MODEL_PARK_LIMIT`, `MODEL_CA_FILE` (`docs/reference/environment.md`);
  `docs/reference/parity.md` gains "model calls name their run", "a gateway's refusal parks the run" and "a CA for the
  model endpoint" for the coder, the folder agent and the engine (whose host gives its own provider, ADR 0058, and
  decides with `parks` whether its refusals park).
* A deployment that changes nothing keeps its key and its failures.

## Built in

| Slice | What is true after it | Proof |
|---|---|---|
| S8a | `InjectedRun`, `MODEL_AUTH=injector`, `MODEL_CA_FILE` and the two `run` fields: every model call of a run, its subagents' and its sessions' included, carries the root's id and no `Authorization` | two levels. **Requests:** the scripted `MockModel` records every `ModelRequest` of a chain of child runs, a compaction's summary call (of a root and of a child), a session's rounds (opened by a task, sent by a later task, sent by a person) and a subagent under a person's round, and each names the run decision 2 says; a message's metadata cannot change it. **Wire:** a mock HTTP server records each call's headers through `adam-model-openai` and `adam-service`: `X-Run-Id` and no `Authorization` on completions, streams and the retry after a 401; a call with no run sends nothing; a TLS server whose certificate a test CA signed is reached only with `MODEL_CA_FILE` |
| S8b-1 | the model layer of decision 3, and of decisions 7 and 8: `Refusal`, `ProblemType`, `Park`, `ModelCredentials::parks` (no park by default), `InjectedRun`'s table, `ModelError::Refused`, `ModelLocked` and `Unreachable`, `MAX_PARK`, and `Ctx::retries_left()`; nothing parks a run yet | **Unit:** each row of the table and its clamp, an unreadable type, the classes, a `Display` with no body, the worker's verdicts and `retries_left` sharing one decision. **Wire** (`adam-model-openai`, a mock server): each slug, a slugless `401` and `403`, an unreadable `type`, a `Retry-After` far ahead, `model_locked` for every provider, `StaticBearer`'s errors against goldens from before, no body in any error, a refused connection `Unreachable`, a timeout and a TLS handshake refused for its CA not; **runtime:** a policy of three attempts sees 2, 1 and 0, and the last failure fails the run |
| S8b-2a | decisions 4, 6, 7 and 8 for a run that calls the model itself: the park, its backoff, `MODEL_PARK_LIMIT`, the summary call, the states, the message and `adam.resume` over A2A | **Agent loop** (`adam-llm-agent` `tests/model_park.rs`, a scripted model whose refusals are injected): each kind parks with its wake and keeps its conversation, the same call after the timer, a message's text added, a resume adding nothing, the backoff, the limit, an unattended run, an unreachable endpoint, `ModelLocked`, `StaticBearer`'s errors, a cancel, and no body in the journal through `adam-model-openai` against a mock server; the summary call (`tests/compaction.rs`). **A2A** (`adam-a2a-runtime` `tests/model_park.rs`): the three states and their messages, the list filter, a message to a timer park, a textless resume through the backend and over HTTP, the refusals with `-32602` and nothing written, cancel |
| S8b-2b | decision 5 (child runs) and the coder's background commands | **Agent loop** (`adam-llm-agent` `tests/model_park.rs`): a subagent's park until woken shows on its parent, which waits with no timer and says why, and a message or a resume to the parent wakes the child through it (its words staying at the parent, a copy of the waking message waking it once, a forwarded resume clearing the copy, a child on a timer keeping the 60 s look); a grandchild is reached one level at a time; the test gateway's slugs, a slugless `401`, a refused connection and a timeout, `MODEL_PARK_LIMIT`, an unattended run, `ModelLocked`, `StaticBearer` and nothing of a header or body in the journal as S8b-2a built them. **A2A** (`adam-a2a-runtime` `tests/model_park.rs`): the root reads `input-required` or `working` with the descendant's reason, takes a resume and a message, and refuses a resume once the child finished. **Runtime** (`adam-runtime` `tests/runtime.rs`): the park notice is sent once per park and never by a look, and the checked accessors refuse a run that is not the caller's child. **Coder** (`bin/adam-coder`): an until-woken child stops the background commands |
| S8c | the coder's chart sets `MODEL_AUTH=injector` when its values say so | the chart's goldens; another-agentic-system#334 S10 puts the agents behind the gateway |

## Open

* **Firings under the injector:** which run a firing names, and the orchestrator binding it.
* **Authenticating each agent to the gateway** beyond the namespace's trust (decision 1).
