# 0081. An agent keeps a self-improvement memory near it, and what holds becomes the deployment's memory

Status: **Accepted** (2026-11-27), the owner's decision; the points under [Still open](#still-open) wait for an answer.
Nothing was built when this was accepted; M1, M2a, the M4 memory tools (partly), and the MongoDB
and Redis adapters are built since (see the amendments below). The evidence behind this page (the paper, the
code it was checked against, the code index, the proposals for a memory protocol) is in [the agent page](../../.agents/docs/decisions/0081-an-agent-keeps-a-self-improvement-memory-near-it-and-what-holds-becomes-the-deployments-memory.md).

**Amended 2026-11-28 (ANR-1292).** The PostgreSQL L2 adapter is built: `PgMemory`, the `memory`
feature of `adam-store-postgres`, one configured scope per adapter over one `memory_lesson` table
(schema version 10 of the run store's database), proved by `memory_conformance!` with two scopes on
one database. The MongoDB adapter (ANR-1293) is built too; Redis (ANR-1294) remains unbuilt.

The owner said (2026-11-27):

> *"We'll also add this [arXiv 2611.18856] to the adam-rs, in the form of a new crate on all coding agents. Also, we
> don't need it for chat by default. Adam-agents and adam-coder has them enabled by default, configurable. That implies
> a memory. That memory is not the same as the global memory, this one is not far from the agent. We'll keep it
> hexagonal tho (with port and adapters). We'll have a pass between global and local memory so that if something
> happens consistently, we just have to upgrade the global memory. => I'll say we have SIMemory (L1) and Memory(L2)"*

And, answering the questions put to them the same day:

| Question | The owner, 2026-11-27 |
|---|---|
| What to build | "Both, lessons first": task-time lessons first, then the paper's own setting, a harness optimizer over agent folders, on the same ports |
| Who promotes L1 to L2 | "Review using another configurable agent/model with its config": another agent, with its own model and configuration, reviews each candidate; not a person, not automatic |
| How an agent reaches it | "Tools first": recall and record are tools in a lazy tool group, journaled; memory in the prompt is a later option |
| Where it is stored | "I'm seeing a port & adapter here for mongo, redis, postgres and in-folder at first": ports with adapters for MongoDB, Redis, PostgreSQL and an in-folder store, the in-folder one first |

Then, later the same day, on the shape of the two tiers:

> *"So, this is how I see it now. I'll copy the CPU: L1: all local memories <=> SIM, LCI. L2: all remote memories,
> shared across sessions"* (the owner, 2026-11-27)

> *"About if L2 is per deployment or shared across deployments, the adapter will decide. The way I see it, it can be a
> single or a per adam-coder."* (the owner, 2026-11-27)

Builds on [ADR 0050](0050-hooks-around-the-agent-loop-compiled-first-and-programs-second.md) (plugins and observers),
[ADR 0066](0066-a-first-request-is-lean-and-tools-load-in-groups.md) (tool groups),
[ADR 0044](0044-paged-tools-a-stable-prompt-prefix-a-plan-skills-commands-and-project-context.md) (the stable prefix),
[ADR 0060](0060-schedules-are-parked-runs.md) (schedules) and [ADR 0070](0070-a-secret-is-replaced-where-it-is-a-value.md)
(the redaction marker).

## Context

*Self-Improvement Memory* (arXiv:2611.18856, *verified 2026-11-27*) is the record a harness optimizer keeps of the edits
it tried: grouped by root cause, each attempt with its rationale, what it was **predicted** to fix, what was **observed**,
and where it came from. Carried to a new benchmark or model it beat carrying the harness alone in all ten settings. The
paper's memory serves the optimizer, not the agent that solves the task.

adam-rs has no agent memory today. The orchestration layer keeps a person's own notes, owned by the person; that is not
this memory, and it leaves an agent's own memory to adam.

### Is there a protocol for memory between agents?

Not a widely adopted one (*verified 2026-11-27*, web search): MCP standardises tools and A2A the messages between agents,
and both leave memory out of scope. Proposals exist, such as the Mesh Memory Protocol (a preprint of April 2026) and
Akashik (an open specification), and are early; their adoption is *unverified*. Because L2 is a port, a protocol that
matures becomes one more L2 adapter, and nothing here waits on one.

## Decision

1. **Two tiers, as in a CPU's caches.** **L1** is every memory that lives next to the agent, on its own machine or
   volume. Two today: *SIMemory*, the self-improvement memory (the problems the agent met and what it tried), and a code
   index such as lci-mcp, reached through its own tools. **L2** is every memory reached over the network, shared across
   sessions: *Memory*, the lessons that held across L1 scopes. SIMemory and Memory are each a trait of a new crate
   `adam-memory` with a conformance testkit, so an adapter proves itself the way a `Store` does. No driver type is in a
   signature; errors carry a class. The code index keeps its own interface: it is an L1 memory beside SIMemory, not
   SIMemory's store.
2. **Adapters, and what L1 keeps.** L1's adapter is the in-folder store (plain files in a directory the deployment gives
   the agent), and it is L1's only one: L1 is local by definition. L2's adapters are PostgreSQL, MongoDB and Redis.
   A database reached over the network is L2, even when one agent alone uses it. Each adapter is a feature of a crate of
   its own, and its port's suite runs on each. Like a cache, L1 holds only what the agent's local storage keeps: a
   folder that persists across runs (a volume) keeps L1 across that agent's sessions, one that does not keeps it for
   the session only, and the deployment decides by the volume it gives. Two workers with two folders have two L1s, as
   two cores have. What must survive the machine, or be seen by other sessions, has to be promoted to L2.
3. **The paper's shape, at task time.** An L1 entry is a **problem** (a short root cause) with its **attempts**: the
   remedy, the rationale, a prediction written before the outcome, the observation stamped after it, and provenance. A
   failed attempt is kept. The model decides which problem an attempt belongs to. An attempt may carry code anchors as
   text so a lesson can be judged stale.
4. **Scope.** L1 is keyed by deployment, caller, agent and kind (`agent`, or one repository); no L1 entry crosses a
   caller. **L2 is keyed by the scope its adapter is configured with**, the owner's choice: a single store, which
   several deployments may share, or one per adam-coder. Within a scope, a lesson is keyed by its audience, and two
   scopes on one database never see each other's lessons. Memory is not per person, because adam cannot tell people
   apart yet; a repository-scoped entry is served only to a run that has that repository prepared.
5. **Bounds and secrets.** Every list is bounded and old attempts are swept. Every text is scrubbed before the write, and
   a text that still holds the redaction marker is refused, not stored. An agent-scoped entry or a lesson may not name a
   repository, a path, a URL, a host or an e-mail address. The loop never copies a person's message or a tool's output
   into memory.
6. **Tools, not the prompt.** A tool group `memory` offers `si_recall` (the run's L1 problems and the active L2 lessons
   for its audience, with provenance) and `si_record` (open or extend a problem). Both are ordinary journaled tool steps:
   a replay returns the record and asks no store, and a write lands once across a retry. The first request gains one
   loader line. A preload into the prompt is a later option and waits for a journal epoch. The code index is not read
   through these tools.
7. **Outcomes are observed, not written by the model.** An observer plugin stamps an attempt's observation from the
   tool and run events (the coder's checks, the run's end). It runs outside the journal and never changes a run.
8. **Promotion is a rule, then a review.** A promotion pass runs where L1 is, in the agent's own process: on a schedule
   it reads that L1 and sends each problem's evidence, scrubbed and keyed by its root cause, to L2, once. Nothing sent is
   served. A **pure** rule, judged over the evidence of every L1 scope in L2, makes a problem a candidate when, within a
   window, it has enough successful attempts from enough runs and conversations in at least two L1 scopes, a high
   enough success rate and no recent failure. The thresholds are deployment settings.
9. **Another agent reviews each candidate.** A reviewer agent, with its own model and configuration (a folder, a model
   alias and its limits, set by the deployment), reads a candidate with its evidence and approves or rejects it. Only an
   approved lesson is served. The review is journaled and keeps its reason.
10. **A lesson never widens.** Its audience is the intersection of its sources'. A text that fails decision 5 stays in
    L1. A lesson whose evidence turns is retired: kept, and no longer served. States are closed: proposed, active, retired.
11. **On for the two binaries, off for a chat by its folder.** `adam-coder` and `adam-agent` enable it by default,
    configurable; `adam-engine` does not. A folder's `si_memory:` key turns it off (the chat folders say `off`) or tunes
    it. The key is not `memory`, which is Claude Code's ignored key. L2 and promotion need a configured L2 store; with
    none there is no L2, and L1 alone works.
12. **Fail closed.** An unknown option, or memory on over a store with no adapter, exits at startup. At run time an
    unreachable memory is a transient tool error and the run goes on without it; a failed scrub or bound refuses the
    write. Memory never grants a tool, a tier or a repository.

```mermaid
flowchart LR
    subgraph local["L1: next to the agent (its machine or volume)"]
        S["SIMemory: problems and attempts (new)"]
        C["a code index, such as lci-mcp"]
    end
    subgraph remote["L2: over the network, shared across sessions"]
        G["Memory: lessons (new)"]
    end
    A["agent"] -->|si_recall, si_record| S
    A -->|its own tools| C
    A -->|si_recall| G
    S -->|"promotion pass, then a review"| G
```

```mermaid
sequenceDiagram
    autonumber
    participant M as model
    participant L as LlmAgent loop
    participant S as SiMemory L1 (new)
    participant O as outcome observer (new)
    participant P as promotion pass (new)
    participant R as reviewer agent (new)
    participant G as Memory L2 (new)
    M->>L: load the memory group, call si_recall
    L->>S: problems of the run's scopes (a journaled step)
    L->>G: active lessons of its audience
    M->>L: si_record with a prediction
    L->>S: record the attempt, scrubbed, once
    L->>O: tool and run events, after the steps
    O->>S: observe the attempt's outcome
    P->>S: this L1's evidence since the watermark, on a schedule
    P->>G: the evidence, keyed by root cause, once
    R->>G: candidates the rule makes from every scope's evidence
    R->>G: approve: upsert the lesson, once
```

```mermaid
stateDiagram-v2
    [*] --> Open: si_record names a new root cause
    Open --> Evidenced: attempts observed
    Evidenced --> Evidenced: more attempts, failures kept
    Evidenced --> Candidate: the rule holds over the scopes' evidence in L2
    Candidate --> Active: the reviewer approves
    Candidate --> Evidenced: the reviewer rejects
    Active --> Retired: later evidence falls below the rule
    Evidenced --> [*]: swept past retention, or the L1 folder is gone
```

## Slices

Cards of the board epic; each is one pull request, its proof seen failing first.

| Slice | What | Proof |
|---|---|---|
| M0 | this ADR and its pages | docs-check |
| M1 | the L1 port, its testkit and the in-folder adapter, L1's only one | a double that answers another scope or writes twice for one key fails the suite |
| M2a | the L2 port, its testkit and the in-process reference | `memory_conformance!`; two configured scopes on one store never see each other's lessons |
| M2b | the PostgreSQL, MongoDB and Redis adapters (ANR-1292, ANR-1293 and ANR-1294, built) | the suite on each; two scopes on one database never see each other; fails with a URL unset under the CI switch |
| M3 + M4 | `si_memory:` and the deployment's settings; the `si_recall` and `si_record` tools as a lazy group, over L1 | a folder `off` is off; a replay asks the store nothing; a secret in a field is refused |
| M5 + M6 | the outcome observer; the defaults in `adam-coder` and `adam-agent`, and the chart | a passing check stamps a success; the first request differs by one loader line; the chart renders unchanged by default |
| M7 + M8 | the promotion pass and rule; the reviewer agent | the same evidence twice is one lesson; one L1 scope alone is never a candidate; a rejected candidate is never served; one scope never reaches every audience |
| M9 + M10 | L2 in `si_recall` and retirement; a paired study in `bench/` ([ADR 0048](0048-a-benchmark-harness-for-adam-coder-and-opencode.md)) with and without memory | a retired lesson is not served; the study is a report, not a gate |
| M11 | the harness optimizer over agent folders, on the same ports | its own ADR first |

The M2 port-and-kit half (M2a) is built (*2026-11-27*): `adam-memory`'s `Memory` trait, its types,
its in-process reference and `adam-memory-testkit`'s `memory_conformance!`, with the two-scope case.
M2b's three adapters are built (*2026-11-28*): the PostgreSQL one as `adam-store-postgres`'s
`memory` feature (ANR-1292), the MongoDB one as `adam-store-mongodb`'s (ANR-1293) and the Redis one
as `adam-memory-redis` (ANR-1294). M2a claimed no adapter.

## Still open

| # | Question | Recommendation |
|---|---|---|
| 1 | How a chat is told apart | the chat folders say `si_memory: off` |
| 2 | The promotion thresholds | the draft's: 5 successes, 5 runs, 3 conversations, 2 scopes, a lower bound of 0.6 over 30 days |
| 3 | Per-person isolation | key repository entries by person once the person's identity exists |
| 4 | Whether subagents read the root's memory | read the root's L1, write under their own name |
| 5 | The platform's `AgentConfig` field for the key | a field of the same name, in step |
| 6 | Where the promotion pass runs when an agent has several workers | in each process that holds an L1 folder; the evidence adds up in L2 by scope |

## Consequences

* Two ports with a testkit each, one L1 adapter and three L2 adapters. `Store` does not change, so no implementer breaks.
* Like a cache, L1 is only as durable as the folder the deployment gives. An agent with no volume forgets at the end of
  a session; what it learned survives only if it was promoted to L2 by then.
* Every agent built with the crate gains one loader line in its first request; a model call is added only when the model
  loads the group.
* A wrong lesson can spread to the agents that share an L2 scope; provenance, review and retirement limit it and do not
  remove it. A single L2 shared by several deployments widens that reach, and is the adapter's configuration to choose.
  A repository's text can try to plant a lesson; repository scope and the reviewer limit that.
* The reviewer is a model: it adds a cost per candidate and can be wrong. It is configured, journaled and replaceable.
* The gain of task-time memory is unmeasured; the paper measured an optimizer's. M10 is the evidence.
* The platform's `AgentConfig` and the orchestration layer's chat folders change in step.

## Alternatives rejected

* **`memory:` as the key.** It is Claude Code's, read and ignored today: reusing it turns behaviour on in imported files.
* **Memory as `Store` methods.** A new required method breaks every implementer, and the store is per run.
* **The orchestration layer's person memory as L2.** It is the person's, off until they turn it on, reached by tools.
* **The whole record in every prompt, as the paper does.** Thousands of runs need a bound, and a changing prefix breaks
  the provider's cache.
* **The code index as SIMemory's store.** The code index is an L1 memory of its own, holding code rebuilt per revision;
  it does not hold SIMemory's outcomes, which cannot be rebuilt from code. An attempt links to code only through its
  anchors.
* **SIMemory on a remote database.** A store reached over the network is L2 by the owner's line; an L1 on one would blur
  the tiers. An agent whose memory must outlive its machine gets a volume, or promotes to L2.
* **L2's scope fixed by adam.** The owner left it to the adapter: one store for many deployments, or one per adam-coder.
* **Promotion by a person, or with no review.** The owner chose another agent as reviewer.
* **Inferring a chat** from its tools or tags. Implicit, and a tag is presentation.

## Amended 2026-11-28: the Redis adapter

The Redis adapter is built (`adam-memory-redis`, ANR-1294), and it settles nothing this decision
left open: Redis is **L2, a store and not an L1 cache** — decision 2 already makes any database
reached over the network L2 even when one agent alone uses it — so the adapter implements `Memory`
and never `SiMemory`.

Its durability is its deployment's to configure, and the adapter documents it rather than enforcing
it (it cannot reach a server's settings): AOF with `appendfsync everysec` (or `always`, at the cost
of latency) so a restart does not lose the lessons since the last snapshot, and
`maxmemory-policy noeviction` so the server refuses a write instead of evicting one. A deployment
that chooses an eviction policy breaks decision 10's "kept, not deleted" **silently**: lessons go
with no error the caller ever sees.

The suite runs under `ADAM_TEST_REDIS_URL` with `ADAM_TEST_REQUIRE_DB=1` in CI, and with ANR-1293's
MongoDB adapter two of the Slices table's M2b three are built.
