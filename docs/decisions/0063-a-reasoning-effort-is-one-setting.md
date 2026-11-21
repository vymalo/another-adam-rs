# 0063. A reasoning effort is one setting, configurable end to end

Status: **Accepted** (2026-11-21)

## Context

The model gateway `api.ai.camer.digital` reasons **not at all** when a request carries no
`reasoning_effort`, and adam sends none today: `MODEL_EXTRA_BODY` is the only way to set it, and the
chart documents that escape hatch (`deploy/coder/values.yaml:71-79`). The owner asked for one setting,
reasoning effort, configurable in adam and all its clients.

The gateway's own values were **verified** on 2026-11-20 by direct calls from the benchmark harness
(`bench/README.md` on `bench/variants-reasoning`, not merged; model `deepseek-v4.1-flash-1017`):
`low`, `medium`, `high` and `max` are accepted; omitting the member reasons not at all; `none` and
`minimal` were never sent and are **unverified**. At the reviewer's output budget, `medium`, `high` and
`max` spent more than 32k reasoning tokens and produced no answer, so the effort and the output budget
interact (the loop's handling of a reasoning-only length stop is its own change, S-S5bS's).

## Decision

1. **The values are a closed enum.** `ReasoningEffort` in `adam-model`: `Low`, `Medium`, `High`,
   `Max`, spelled `low`, `medium`, `high`, `max` on the wire and in every setting. `none` and
   `minimal` are refused as unknown, not passed through; they are added only when the gateway
   verifies them, with a dated note here. "Off" is **the absence of the setting**, never a value.

2. **The levels, narrowest wins:**
   request → thread → agent folder → deployment → `MODEL_EXTRA_BODY` → absent.
   - **request**: the `reasoning/v1` extension on one A2A message (decision 4).
   - **thread**: the run's current effort, set by the last activated message; a child inherits it, and
     `subagent_start`'s config may set it (ADR 0054 decision 8 and slice 12 — this ADR implements that
     seat and does not contradict it).
   - **agent folder**: `reasoning_effort` at the top level of the frontmatter.
   - **deployment**: `MODEL_REASONING_EFFORT` and the chart's `config.modelReasoningEffort`.
   - **`MODEL_EXTRA_BODY`**: its `reasoning_effort` member keeps working as the raw deployment escape
     hatch, but **loses to any narrower level**: a set effort replaces that one member (ADR 0054
     decision 8's rule). This is the one exception to "the extra body wins over what the runtime
     wrote", and it is stated in `adam-model-openai`'s docs, the chart's and
     `docs/reference/environment.md`.
   - Every level is optional; absent everywhere is today's behaviour.

3. **The folder key is `reasoning_effort`, not `effort`.** `effort` is already an ignored Claude
   Code/Copilot key (`crates/adam-agent-fs/src/load/agent.rs:27`); promoting it would change what an
   existing folder means. It stays ignored.

4. **The A2A extension is `https://agents.vymalo.com/a2a/extensions/reasoning/v1`.** Its card entry is
   `required: false` with `params: {efforts: [lowest first], default?: <one of them>}`. It is
   activated per message exactly like `access-tier/v1` (the `A2A-Extensions` header and/or
   `message.extensions`); an activated message **must** carry
   `metadata[URI] = {"effort": "<one the agent honours>"}`, and anything else — missing, non-object,
   missing or non-string `effort`, an unknown word, a word the agent does not honour — is
   `InvalidParams` (`-32602`) **before anything is written**. A message that did not activate it
   carries no effort. The value is merged into the run's protected context as
   `vymalo.reasoningEffort` (`{"effort": ..}`); the newest message wins from the agent's next step;
   it is not inherited across tasks. A caller that activated it is echoed the effort on the
   `SendMessage` answer, `GetTask`, the first stream frame, and when it changes — the same shape as the
   access tier's echo.

5. **The wire.** `ModelRequest` gains `reasoning_effort: Option<ReasoningEffort>`;
   `adam-model-openai` sends it as `reasoning_effort`. The merge order is: the extra body, then the
   request's own member, so a narrower level wins. The compaction/summary call carries **no** effort
   (a fixed output budget; reasoning there spends it).

6. **Usage.** Each model call's `UsageEvent` carries `effort: Option<ReasoningEffort>`, reported per
   call by `usage/v1` (the struct is `#[non_exhaustive]`, so additive). The totals stay per
   provider+model; the effort is per call. The bench reads the per-call value.

7. **The engine.** `adam_engine::Limits` gains `reasoning_effort: Option<ReasoningEffort>`
   (`#[non_exhaustive]`, additive). It is the deployment level for the agents the engine hosts; a
   folder's setting and a request's extension still win.

8. **The deployment.** `MODEL_REASONING_EFFORT` (both binaries, through `ModelConfig`) and the chart's
   `config.modelReasoningEffort` (validated against the closed list, refused beside
   `config.extraEnv.MODEL_REASONING_EFFORT`, and it wins over `config.modelExtraBody`'s member). Unset
   renders nothing, so the chart's goldens stay byte-identical. `deploy/coder` is read by
   `vymalo/another-agentic-platform`'s parity goldens; the PR says so.

9. **The default.** The code ships with **no** default: nothing changes until a deployment sets one.
   The recommended deployment value for the coder is **`low`** — the only level the bench verified to
   answer quickly (10–20 s), while `medium`, `high` and `max` ran past 32k reasoning tokens with no
   answer at the reviewer's output budget. `medium` is the intended next step once the length-stop fix
   (S-S5bS) and a larger output budget are proven. The owner decides whether the chart ships a default.
   The extension's card `default` is the deployment's value when it sets one, and is absent otherwise,
   so "no member sent" remains expressible.

## Consequences

- A deployment that sets nothing is byte-identical to today: no member, no reasoning, no new env.
- `ModelConfig` (`adam-service`) and `ModelRequest` (`adam-model`) gain a public field: a consumer that
  builds either as a struct literal must add it. `adam-service/README.md`, `adam-model`'s README and
  the `adam-upgrade` skill say so. `adam-engine::Limits` is `#[non_exhaustive]`, so it is additive.
- The card gains one extension: `bin/adam-agent/tests/agent.rs`'s exact URI list and the coder's card
  test change on purpose.
- The chart renders `MODEL_REASONING_EFFORT` only when set; the four goldens are unchanged by default.
- The bench is the instrument that verified the values; its `sent.effort`/`askedEffort` already exist on
  `bench/variants-reasoning`, and the full run can read the effort a call ran at.
- The length-stop fix is complementary: a high effort can spend the whole output budget reasoning;
  the loop must not treat that as a question (S-S5bS's change).

## Alternatives considered

- Accept `none`/`minimal`: unverified against the gateway; fail closed instead.
- Reserve `reasoning_effort` in `MODEL_EXTRA_BODY`: breaks every deployment and the chart's own
  documented example (`{reasoning_effort: medium}`); the narrower-wins exception keeps them working.
- Promote `effort`: silently changes an ignored key's meaning.
- A hardcoded default: the owner said nothing changes until it is configured.
- A separate extension per level: the effort is one scalar; one URI with `efforts`/`default` is enough.

## Open questions

- The owner's default (recommendation: `low` for the coder, the only level the bench verified to answer quickly; `medium` once the length-stop fix and a larger budget are proven; off in code).
- The system and web half (read `reasoning/v1` from the live card, per message and per thread, the
  picker) and the platform field (`AgentConfig.model.reasoningEffort`, a deployment ceiling, `aapctl`)
  are separate ADRs in their repositories; they follow this contract.
- Whether `medium`/`high`/`max` need a larger output budget to answer at all is a bench question, not
  a setting this ADR adds.
