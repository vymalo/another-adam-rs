# 0085. An agent keeps the network tier a message names: what each tier means on a run pod and on a laptop

Status: **Accepted** (2026-11-27). The contract is the orchestration layer's: sys ADR 0062
(*the person picks a network tier for each message, the agent keeps to it*, accepted 2026-11-26, amended the same day
so a card lists only the tiers its agent enforces). This ADR decides **adam's side** of it: which tiers an adam agent
lists, what each one means where a run actually executes, and how the tier is said back. It follows
[ADR 0042](0042-run-pods-reach-the-internet-through-an-egress-proxy.md) (a run pod's egress is deny-by-default through
`adam-egress`) and [ADR 0036](0036-the-coder-keeps-to-the-access-tier-the-person-picks.md) (the access tier, whose
shape this copies).

## Decision

1. **The extension is `network-tier/v1`**, URI `https://agents.vymalo.com/a2a/extensions/network-tier/v1`, listed by
   `adam-coder` and `adam-agent` with `params: {tiers, default}` — the tiers **this deployment enforces**, and the one a
   message that names none works under. The card lists a tier only when the deployment can keep it (sys ADR 0062
   decision 9 as amended): a compose stack with no run pods does not offer `allow-list` unless it can enforce a list.
   **`adam-engine` has no card**, so a host embedding it sets its own network and the extension is n/a there.

2. **Three tiers, the system's closed list**: `offline` < `allow-list` < `open`. A tier **only narrows**: it never grants
   a host, a credential or a tool that the deployment's own policy withheld, and `open` is today's behaviour rather than
   a widening.

3. **On a run pod** (ADR 0019, ADR 0042), the tier is the egress proxy's policy for **that run**:
   - `offline`: the run pod's connection to the proxy is refused, so its only routes are the ones the NetworkPolicy
     already allows besides the proxy — DNS, and `runPods.networkPolicy.extraEgress` (the model gateway). Nothing on the
     public internet is reachable, and `adam-egress` never sees the run.
   - `allow-list`: the proxy accepts the deployment's allow-list (`runPods.egressProxy.allow`, ADR 0042 decision 3) —
     and only that. A per-run list, so one run's tier cannot widen another's, is slice 3.
   - `open`: the proxy accepts any public host the deployment allows (private ranges and the metadata address stay
     refused at every tier, as `adam-egress` already does).

4. **Without run pods** — the system's compose stack, an `adam-agent` folder on a laptop, a CI run — the tier is kept by
   the **process**, because there is no pod to police:
   - `offline`: the loop's network tools are withheld and their calls refused (`web_search`, `web_fetch`, the MCP
     servers that reach the internet), **and** a command runs with no network namespace where the OS gives us one
     (`unshare -n`, Linux) or is refused with the reason where it does not. A tool that fetches for the agent is the
     same rule as a fetch: refused.
   - `allow-list`: the same withheld tools except the hosts on the deployment's list, checked at the tool, since a
     process cannot enforce a list at the socket.
   - `open`: nothing changes.
   The honest statement matters more than the mechanism: a deployment that cannot keep a tier **does not list it**, and
   a run whose command cannot be confined says so in its answer instead of pretending.

5. **The tier rides on the message and is said back on the task**, exactly as the access tier does
   (`crates/adam-a2a-runtime/src/tier.rs`): an activated message must carry `metadata[<URI>] = {"tier": "…"}`; a tier the
   card does not list is `InvalidParams` before anything is written; the tier becomes a run context key the newest
   message wins; and a caller that activated the extension reads `{"tier": …}` in the task's `metadata` — the one the run
   worked under, or the deployment's default. The two extensions are independent: `edits` + `offline` is a person's
   choice, and neither implies the other (sys ADR 0062 decision 10).

6. **Nothing is passed down.** A child session, an `ask_agent` child and any task an agent starts for another agent run
   under **their own** deployment's tier; adam never forwards one. What a child runs counts as the agent's own, which is
   why a child inherits the parent's **process** (and so its tier) without a message carrying it.

7. **The default is `open`** — what run pods allow today — and a deployment may set another. **Lowering the default is
   the owner's call**, not the implementer's: it changes what every existing deployment's agents may reach.

## Slices (ANR-907, one PR per row)

| Slice | What | Test |
|---|---|---|
| N1 | the card entry and the reading: `ExtensionConfig::network_tier`, `NetworkTiers`, `metadata[<URI>]` read and refused when not listed | a card lists only what the deployment enforces; a tier it does not list is `InvalidParams`, nothing written |
| N2 | the echo on the task, and the context key the run reads (the `tier.rs` shape) | the task says the tier the run worked under; a message that names none leaves the run's context |
| N3 | run pods: the proxy's policy per run, `offline` refused at the pod, `allow-list` served from the deployment's list | a run pod under `offline` cannot reach the proxy; under `allow-list` it reaches a listed host and not another |
| N4 | without run pods: the withheld tools and the confined command (`unshare -n`), and the words when neither is possible | `offline` refuses `web_fetch` and a `curl`; `allow-list` lets a listed host through and refuses another |
| N5 | the compose scenario the system runs (ANR-911) against the published image | on the stack, `offline` makes a fetch fail and the run says so; `open` lets it through |

## What the system runs (ANR-911)

The orchestrator sends `offline` and then `open` to the same agent on the system's compose stack: the first run's fetch
fails with the tier named in its answer, the second reaches the host. That is the acceptance this ADR is written for; it
needs N1, N2 and N4, not the run pods.

## Consequences

* **Good:** the person's choice is enforced where the run is, not asked of the model, and a deployment cannot advertise
  a tier it cannot keep.
* **Cost:** two enforcement paths (a pod's proxy policy, a process's tools and namespaces) with different guarantees;
  the second is weaker on a kernel without user namespaces, which is why it says so rather than failing silently.
* **Risk:** a deployment that lists `open` while its NetworkPolicy is stricter than the proxy is telling the person
  something untrue — decision 1 forbids listing what is not enforced, and the parity table is where that is checked.

## Open questions

| # | Question | Recommendation |
|---|---|---|
| 1 | Is `open` the right default for `adam-agent` on a laptop? | Yes, for now: it is what an agent without a pod already has, and lowering it is a separate owner decision (decision 7). |
| 2 | Does `allow-list` on a process need a list at all, or is `offline` the only honest tier there? | A list checked at the tool, in slice N4; if the check cannot be made sound, the deployment lists `offline` and `open` only. |
| 3 | Should a run's tier be recorded in its state, as the access tier is in the notes? | Yes in slice N2, for the echo and for a replay to say the same tier. |
