# 0072. The coder verifies who the person is and asks GitHub before it touches a repository

Status: **Proposed** (2026-11-24). This is adam-rs's own record of its half of the orchestrator's
[ADR 0064](https://github.com/vymalo/another-agentic-system/blob/main/docs/decisions/0064-the-person-reaches-the-agent-and-links-their-github.md)
(accepted 2026-11-24, in `vymalo/another-agentic-system`), written before any code (corntchap ANR-288, issue #121). Nothing
here is built. Every name below follows ADR 0064; the **extension is `identity/v1`** (it was `person/v1` in the proposal). The
decisions that are adam-rs's own, and the ones the owner must confirm, are at the end. Builds on
[ADR 0041](0041-a-run-pod-sees-only-its-own-run.md) (a run pod holds one Secret key), [ADR 0019](0019-a-runs-processes-in-a-pod-of-their-own.md)
and the thread-tools grant the coder already keeps (`vymalo.threadTools`).

## Context

The coder acts through one GitHub App for everyone and checks nobody's access: any signed-in person could ask it to read or change
any repository the App is installed on. ADR 0064 makes the orchestrator send, with each message, a short-lived signed **assertion**
of who the person is and which GitHub account they linked, and lets the coder redeem a **lease** of that person's GitHub token. This
ADR says what the coder does with both. The orchestrator's side (the assertion, the JWKS, the lease route, the link store) is not
decided here.

## Decision

1. **The card lists `identity/v1` unless the mode is `off`.** The entry carries `claims: ["github"]`, and `required: true` when the
   deployment sets `PERSON_REQUIRED`. Modes: `off` (not listed, nothing verified: today's behaviour), `optional` (listed; a message
   without an assertion is served as before but the coder acts as no person and refuses every repository check, see 7), `required`
   (a message without a valid assertion is refused). The default is `off`, so nothing changes until a deployment turns it on.
2. **Settings** (environment, then the chart; `docs/reference/environment.md` and the chart README change with the code):
   `PERSON_MODE` (`off`, `optional`, `required`), `PERSON_REQUIRED` (the card flag; `required` implies it), `PERSON_ISSUER`,
   `PERSON_AUDIENCE`, `PERSON_JWKS_URL`, `PERSON_GITHUB_CHECK` (on or off; on needs the lease route of the thread-tools grant),
   `PERSON_CHECK_TTL_SECS` (default 300, at most 900). A malformed value, or a mode other than `off` without issuer, audience and JWKS
   URL, is exit 78 naming the setting. *Whether `PERSON_*` stays the prefix now that the extension is `identity/v1` is decision 2 of
   "For the owner".*
3. **Verify before anything is written.** A message that activated the extension and carries a bad assertion is refused as
   unauthenticated, as a bad access tier is refused as invalid, before the run, the journal or the workspace is touched. The checks, all
   of them: the signature against an EdDSA key of the JWKS (the set is fetched, cached and refetched at most once per unknown `kid`;
   an unreachable set fails closed); `typ`; `iss` and `aud` exact; `exp` and `iat` within 30 seconds of the clock; `jti` equal to the
   message id; `thr` equal to the thread of the thread-tools grant when the message carries one; `links` well formed. A task's `sub`
   never changes: a later message with another `sub` is refused. HMAC and `none` are never accepted whatever key they name.
4. **Keep the facts, not the assertion.** The verified `sub` and `links` go into the run's context under a key of their own (not
   `vymalo.person`, which holds the person's words). The newest message wins. A message with no assertion, to an agent that does not
   require one, **clears** the facts (a run must not go on acting as the person before), as `vymalo.mentions` is cleared. With
   `required`, that message is refused (decision 3).
5. **The leased token is held in process memory only.** The thread-tools grant is kept in the run's durable context until it expires;
   the leased GitHub token must **not** go there, nor in the journal, a note, a log line, a step's input or output, or the model's
   context. A crash costs a new lease. A run pod gets no new Secret: the admission policy keeps its one key, `MODEL_API_KEY` (ADR 0041,
   decision 5). The token's type is wiped on drop and its `Debug` prints `<redacted>`.
6. **The check is made in the one place that hands out an installation token for a repository.** Before the coder adds, clones,
   fetches or pushes a repository, it redeems a lease (the lease is for one thread, one job and one agent) and calls
   `GET /repos/{owner}/{repo}` with the person's token. It reads `permissions.pull`, and `permissions.push` when the work needs a push.
   The answer is cached per `(sub, repository, need)` for `PERSON_CHECK_TTL_SECS` and never past the lease. There is no other path to
   an installation token: a code path that mints one without the check is a bug the tests below must catch.
7. **Every failure fails closed, and nothing falls back to the App.** The refusals, in the words a person reads:

   | Situation | The coder |
   |---|---|
   | The card lists the extension, mode `required`, no assertion | refuses the message |
   | The assertion is bad or expired | refuses the message before anything is written |
   | The person never linked | refuses every repository, with the step to link |
   | The link is `needs_relink` | refuses, with the step to link again |
   | The lease is `404`, `409` or `403` | the same refusals, in words |
   | The lease is `503`, GitHub is down, a `429` or a `5xx` | "I cannot verify your access right now": refuses, retryable; **no fallback to the App** |
   | GitHub says `404` or `403` for the repository | "not found, or you have no access to it"; clones nothing |
   | GitHub says `200`, `push` is false, and the work needs a push | the read is allowed, the push is refused |
   | GitHub says `401` to a leased token | one new lease, one new try, then refuses |

8. **The tests run on a WireMock GitHub** (the `dev/` stack already has mocks of this kind) and a mock lease route; no live GitHub.
   Each of the eight criteria of issue #121 has a named test, below.

## The tests, one for each criterion of issue #121

The names are the ones the build slices (ANR-290, ANR-295, ANR-297) must use or replace with a note here.

| # | Criterion | Test (planned name) | Slice |
|---|---|---|---|
| 1 | The repository is allowed (`pull` and `push`): the work proceeds | `identity_check::allowed_repository_proceeds` | ANR-295 |
| 2 | `pull` only, the work needs a push: read allowed, push refused | `identity_check::pull_only_allows_the_read_and_refuses_the_push` | ANR-295 |
| 3 | `404` or `403` for the repository: refused, nothing cloned | `identity_check::no_access_refuses_and_clones_nothing` | ANR-295 |
| 4 | Unlinked person: refused with the step to link | `identity_check::unlinked_person_is_told_how_to_link` | ANR-295 |
| 5 | GitHub down, `429` or `5xx`: retryable refusal, no App fallback | `identity_check::github_down_refuses_retryably_and_never_uses_the_app` | ANR-295 |
| 6 | Bad or expired assertion: refused before anything is written | `identity_verify::a_bad_or_expired_assertion_writes_nothing` (one case each for signature, `iss`, `aud`, `exp`, `iat`, `jti`, `thr`, `sub` change, `none` and HMAC) | ANR-290 |
| 7 | A sentinel token planted in the mock is in no log, journal, note, context, run pod or output | `identity_sentinel::the_leased_token_is_nowhere` | ANR-297 |
| 8 | Docs and READMEs updated, existing behaviour unbroken | `docs-check`, the existing suites with mode `off`, and `docs/reference/parity.md` | all |

A further test, `identity_verify::mode_off_changes_nothing`, holds the default: with `PERSON_MODE=off` the card, the journal and the
answers are byte for byte today's.

## Alternatives rejected

* **Check access with the App's own token.** It shows what the App sees, not what the person may do: the hole this ADR closes.
* **Put the person's token in the run pod or the run's durable context.** It would reach every command of the run and survive a crash;
  ADR 0041 keeps one Secret key in a run pod on purpose.
* **Trust a header or a field of the message for who the person is.** Any sender with the agent's shared bearer could claim anyone.
* **Fall back to the App when GitHub cannot be reached.** It turns an outage into a bypass of the check.

## Consequences

* A deployment that turns the mode on needs the orchestrator's half (ADR 0064, slices B1b) running; until then the mode stays `off`.
* One more request to GitHub per repository and need, bounded by the cache; GitHub's rate limit is the person's own token's.
* A leaked thread token of the coder can redeem the owner's GitHub token while its job is open, in leases of 10 minutes (ADR 0064's
  accepted residual risk); the open-job rule and the list of agents bound it.
* **Unverified:** that GitHub's `GET /repos/{owner}/{repo}` carries `permissions.pull` and `permissions.push` for a user-to-server
  token of a GitHub App in every case (private, organisation and fork repositories), and its exact rate-limit behaviour. Both are checked,
  with date and source, in the slice that builds the check (ANR-295); until then the table above is the intent, not a verified fact.

## For the owner to confirm

1. **Default mode `off`**, so nothing changes until a deployment chooses.
2. **The prefix of the settings:** keep `PERSON_*` as the sketch has them, or rename to `IDENTITY_*` to match `identity/v1`.
3. **`optional` mode:** a message without an assertion is served, but every repository check refuses. The alternative is to have only
   `off` and `required`.
4. **Cache time:** 300 seconds by default, 900 at most (as the sketch has it).
