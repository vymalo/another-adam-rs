# Writing docs: the rules and the checks

For agents. The convention is [ADR 0079](../../docs/decisions/0079-docs-for-people-and-docs-for-agents-live-apart.md);
the short version for people is [Writing these docs](../../docs/README.md#writing-these-docs).

## Two audiences, two trees

| Tree | Reader | Holds | Never holds |
|---|---|---|---|
| `README.md`, `docs/` | people: users, operators, embedders, contributors. Published as the site | the answer first; why; how it works, drawn; tables; the status in words with its date; links. File and crate names | a `path:line`, a test-by-test proof, the commit a claim was checked at, a procedure for checking a change |
| `.agents/docs/` | coding agents, and anyone who wants the evidence. Not published | procedures, invariants to verify, the evidence (`path:line`, tests, commits, dates), how to run the gates | opinions or decisions the page for people does not state |
| `CLAUDE.md` (`AGENTS.md` links to it) | agents | the index: what this is, where things are, the rules, the commands, the skills table | anything longer: it links to an agent page |
| `.agents/skills/adam-*` | agents in other repositories | installable instructions | relative links (a skill is copied elsewhere) |
| `docs/decisions/`, `docs/reviews/` | people | dated records: they keep their evidence as written | silent edits: a change is a dated *Amended* note |

* **A pair shares its relative path**: `docs/<p>.md` and `.agents/docs/<p>.md`. The page for people links to its agent
  page where the evidence would have been; the agent page opens with `For people: [...]`.
* **The page for people decides.** The agent page gives the evidence and the procedure, and never contradicts it. When
  the code disagrees with either page, fix both in the same change.
* **Moving text never deletes it.** When a section leaves a page for people, the page says where it went.

## Which page a change updates

A change that alters behaviour updates its page in the same pull request:

| You change | Update |
|---|---|
| an environment variable, default or exit code | `docs/reference/environment.md`, `docs/reference/errors.md`, the crate README |
| a process, lifecycle, port, role or the store schema | `docs/architecture.md` (diagram and tables) |
| the agent file format, validation, `#[tool]` | `docs/reference/agent-files.md`, `docs/guides/write-an-agent.md` |
| the chart or the image | `deploy/coder/README.md`, `docs/guides/deploy-the-coder.md` |
| compose, a mock, a scripted model, an e2e script | `docs/reference/dev-stack.md`, `docs/guides/run-locally.md`, `docs/guides/testing.md` |
| a path a first-party skill cites | the skill (docs-check fails on a missing path) |
| a setting or a feature of any surface (coder, folder agent, engine, the card) | `docs/reference/parity.md`: fill every column, or list the gap |
| a failure interleaving of child runs, sessions or remote tasks | both pages of `reference/child-runs.md`, same row number |
| an error type | [the rules for an error enum](reference/errors.md#rules-for-an-error-enum) |
| a claim about a third party | mark it *verified* (date and source) or *unverified*; the architecture's log is [architecture.md](architecture.md) |

## How to write a page for people

* **Short.** Short sentences, tables, no repeated explanations: link instead of copying, never copy a crate README.
  `README.md` stays about 100 lines; a guide covers one task.
* **Pictures.** A process gets a Mermaid pair, a `sequenceDiagram` for the interaction and a `stateDiagram-v2` for the
  lifecycle; prose says only what they cannot. Every node, edge, state and call must exist in the code; cite files
  (`crates/adam-runtime/src/runtime.rs`), not lines.
* **Comments in code** only where the reason is not obvious: no restating the code, no essays, no history ("added in
  slice N"). What a user needs belongs in a README or `docs/`.
* **No history in a page for people.** What changed and when is in the ADRs and the git log.

## The checks

```sh
npm --prefix tools/docs-check ci          # once per clone
node tools/docs-check/check-docs.mjs
```

`tools/docs-check/check-docs.mjs` (CI's `docs` job):

1. parses every Mermaid block with the pinned Mermaid version;
2. resolves every relative link and `#heading`, in `docs/`, `.agents/docs/` and every other Markdown file outside the
   skills;
3. requires a `README.md` per crate;
4. checks the first-party skills: frontmatter, mirrors in `.claude/`, `.goose/`, `.kiro/`, no relative link, every
   cited path exists;
5. checks the split: each pair links both ways, and no page for people outside `docs/decisions` and `docs/reviews`
   cites a `path:line`. `docs/reference/parity.md` is listed in the checker as mixed until it is split.

Crate READMEs that are a crate's rustdoc (`#![doc = include_str!("../README.md")]`: `adam`, `adam-git-sandbox`,
`adam-service`, `adam-egress`, `adam-agent`) are doc-tested: run `cargo test --workspace --doc --locked` after editing one.
