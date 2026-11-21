# 0066. A first request is lean, and tools load in groups

Status: **Accepted** (2026-11-21)

## Context

When a person says "Hi", the coder sent the model about **22,300 tokens** before the word: 7,600 of instructions
and 14,657 of tool schemas, of which `ask_user` alone is 2,594. A plain folder agent sent **4,299**. The owner
ruled on 2026-11-21 that a greeting must not cost that, without touching the coder's quality:

- the coder keeps its full playbook prompt and all its tools — *"I also don't wanna sacrifice the quality of our
  current system prompt"* — but its tools get a budget;
- every other base agent (chat, a plain folder, the engine) starts **lean** and loads what it needs;
- a person who adds their own MCP server chooses how it loads;
- no router: a lean request is enough.

The measurements and the full design are the plan `2026-11-21-lean-first-request.md` and its tool table
`2026-11-21-tool-loading-table.md`, both in `skyengpro/another-context`. The numbers below are `cl100k` tokens
counted on the real request (`origin/main` at `1dee409`); a number with `~` is an estimate.

## Decision

### 1. Two kinds of base agent

| | Lean (chat, plain folder, engine) | Eager (the coder, the browser, the researcher, the painter) |
|---|---|---|
| First request | at most **5,000 tokens**, ratcheting down | its measured size, and a no-growth limit |
| Tools | `ask_user` on, the rest behind loaders | every tool on, as today |

A folder says `tools_loading: eager` to opt out; the deployment's default is `lazy`, and an eager agent keeps
today's behaviour exactly. `browser`, `researcher` and `painter` stay eager because their tools are their job.

### 2. A group, and a loader per group

A **group** is a named set of tools and MCP servers. Every group the deployment, the access tier and the
person allow is announced by **one generated tool**, `load__<kind>__<name>`:

- `load__group__<name>` loads the whole group; a **searchable** group's loader takes `query, offset, limit,
  list_only` and loads the matches of a page;
- `load__tool__<name>` loads one heavy tool that belongs with no group;
- the word **`load` is reserved, fail closed**: a server named `load`, or a tool whose name starts with
  `load__`, is refused at start (a listed one is left out with a warning), because an MCP tool is
  `<server>__<tool>` and the names would collide. `load_skill` keeps its one underscore and its name.

A loader exists only if its group has a member the deployment allows, the person attached and the tier shows.
**Lazy never widens access**: a forbidden tool is in no loader, no page and no result, so its existence is not
revealed, and a call to a tool that is not loaded is refused as a withheld tool is today.

`find_tools` and `load_tools` are **dropped**: the loaders are the index, and a searchable group's loader is the
search.

### 3. The coder's groups

| Group | Loader | Searchable | Contents |
|---|---|---|---|
| always on | none | n/a | a compact `ask_user`, `load_skill`, `read_skill_file` |
| `bash` | `load__group__bash` | no | commands, files and search/edit, language servers |
| `repo` | `load__group__repo` | no | workspace and scratch, checks, git, pull requests |
| `screen` | `load__group__screen` | no | `share_file`, `show`, `ui_catalog`, `plan_*` |
| `helpers` | `load__group__helpers` | no | the subagents and `ask_agent` |
| `github` | `load__group__github` | no | the deployment's GitHub server |
| `mine` | `load__group__mine` | **yes** | the person's own servers and the deployment's attached ones |
| `community` | `load__group__community` | **yes** | the community catalogue, once the platform has one |

Its prompt's "Tools" section **moves now** into the notes of the groups that hold those tools; the one rule that
only it carried ("never to look around" — looking around is `run_command`, never a check) moves to "How to work"
in the always-on core. Its first request becomes about **7,500** (5,466 of prompt, ~970 of always-on tools, ~750
of loaders, ~120 of the ordering property), of which tools are about **1,850, under the 3,000 budget**.

A coding task loads `bash` and `repo` **before the first call** when the task holds a keyword (a repository
address, "pull request", "fix", "bug", "test", "build", a file name); the model may also call both loaders in one
reply, which applies in order and costs one step. A continuing conversation keeps what it loaded.

### 4. The budgets, and what enforces them

- a lean agent: **5,000 tokens** in its first request;
- the coder: **tools at most 3,000 tokens**, and its **prompt may not grow**;
- both ratchet down only. S0 measures each with a scripted model, prints a CI table, and pins the generated part
  of the tool table as a golden file, so a new tool or a grown prompt fails a test.

### 5. Loaded state is durable and replayable

What a run has loaded is written in the same commit as the loader's result. A resumed run builds the same
request, byte for byte; a searchable load records the **names** it loaded, not the query, so a catalogue change
does not change a replay. A load **appends** its tools after the others and never inserts one, so the request
before stays a prefix of the request after and prompt caching keeps its prefix (ADR 0044, decision 1).

### 6. Compaction must not forget what is loaded

The compaction marker gains a block written by **code**, from the saved loaded set — never from the model's
summary — with one line per loaded group or tool, one line per loader still available, and a journaled removal
for a tool that disappeared, so a replay is exact. It is bounded to about 2 KiB and 50 lines; beyond that, names
only.

### 7. The person chooses how their own server loads

| Setting | Values | Default | Meaning |
|---|---|---|---|
| `loading` | `eager`, `lazy`, `searchable` | `lazy` | in every request, one loader, or a paging loader |
| `keywords` | up to 16 words, 64 characters each | none | load before the first call when the task holds one |
| `group` | lowercase, digits, single underscores, up to 40 | the server's name | servers of one group load together |

A setting may lower what the model sees, never raise it above the deployment: a forbidden tool stays forbidden;
a person's group cannot take a built-in or deployment group's name; and a person may choose `eager` for their own
servers, which the deployment may refuse with `allowEager: false`.

### 8. Every surface, or a gap

The loaders, the budgets and the settings reach the coder, `adam-agent` (a folder's `mcp.json`), the engine
(builder and `Limits`), the orchestrator's chat and the platform's CRD, form, API and `aapctl`. The parity page
gains the nine rows the plan lists (tool groups, a searchable group, the loader names, a server's group and
searchable flag, skills, a skill that brings tools, the first-request budget, the person's load settings, the
compaction recap), each filled for every column or listed as a gap.

## Consequences

- ADR 0044, decisions 1 and 8, are **amended**: a request is still the previous one plus a suffix, but a suffix
  may now be a **tool load** as well as a step, and progressive disclosure extends from skills to tools.
- `use_server` and `when` fold into the groups; an MCP server's `when` becomes its keywords.
- The slices are S0 (budget tests and the golden), S1 (groups, loaders, the recap), S2 (searchable groups), S3
  (the coder), S3b (the paid A/B, 30 USD, after S3 and not blocking it), S4 (chat and folder agents), S5 (MCP
  servers as groups), S6 (parity), S7 (the dashboard settings).
- **Unverified:** the GitHub server's 3,000–6,000 tokens (its schemas could not be read offline); the sizes of
  the loaders and the compact `ask_user` (estimates until S1/S3 measure them); the engine's first request (not
  measured).
