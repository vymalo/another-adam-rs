# 0079. Docs for people and docs for agents live apart

Status: **Accepted** (2026-11-26). The owner asked on 2026-11-26: *"revamp the doc in -adam-rs and related, so that
easy to read. Re-organize, so that agent's and human doc are separated"*, and accepted the layout below the same day.

## Context

Two kinds of reader open this repository's Markdown:

- **People**: someone trying the agents, an operator deploying the coder, a developer embedding adam, a contributor.
  They want the answer first, in short sentences, with tables and diagrams.
- **Coding agents**: they change the code and the docs, and must check their change. They want the procedures, the
  invariants to verify, the exact evidence (`path:line`, the test that proves each case, the commit a claim was
  checked at) and the commands that run the gates.

`CLAUDE.md` already said that people read `README.md` and `docs/`, and that agent-only context lives in `CLAUDE.md`
and the skills. In practice the two had mixed:

- pages of `docs/` carried agent evidence: the test of every row of the child-run interleavings, the
  verified-and-unverified log of the architecture, the 79 `path:line` citations of the parity matrix;
- `CLAUDE.md` carried long rules that people also need, and had grown past an index;
- `docs/README.md` held a 71-row decisions table, so the index for people was mostly history.

The docs are about to become a site, built from `docs/` (VitePress, one site per repository plus a hub, with the
groups Guides, Reference and Architecture). Whatever is in `docs/` is published, so the split has to hold there.

## Decision

1. **`docs/` is for people, and only for people.** Its pages answer first, in short sentences, tables and Mermaid
   pairs. They may name a file or a crate (where the code is); they never carry a `path:line`, a test-by-test proof,
   a commit a claim was checked at, or a procedure for checking a change. Two kinds of page are dated records and
   keep their evidence as written: ADRs (`docs/decisions/`) and reviews (`docs/reviews/`).
2. **`.agents/docs/` is for agents.** It holds the procedures, the invariants to check, the evidence and how to run
   the gates. It is not published: the site builds from `docs/` only, and the skills CLI reads only
   `.agents/skills/*/SKILL.md`, so a page here is neither a page of the site nor an installable skill.
3. **A pair shares its relative path.** The agent page of `docs/reference/errors.md` is
   `.agents/docs/reference/errors.md`. The page for people links to it, and the agent page opens with a link back.
   An agent page with no counterpart (the docs rules, the skills this repository provides) sits at the top of
   `.agents/docs/`. The same-folder `<name>.agent.md` form is not used, because `*.agent.md` already means a
   subagent file in an agent folder ([agent files](../reference/agent-files.md)).
4. **`CLAUDE.md` is the agents' short index** (`AGENTS.md` stays a link to it): what this is, where things are, the
   rules every change is checked against, the commands, and the table of skills. Anything longer is an agent page it
   links to.
5. **Skills stay what they are**: instructions that other repositories install. They cite the pages of either tree
   by an absolute GitHub URL or a repository path, as before.
6. **`tools/docs-check` holds the split**: it checks the pages of `.agents/docs` like any other (diagrams, links,
   anchors), fails when a pair does not link both ways, and fails on a `path:line` in a page for people outside the
   dated records. A page that still mixes the two is listed in the checker by name until it is split.

## Consequences

- Moving text, never deleting it: what left a page for people is on its agent page, and the page for people says
  where.
- The paths of the pages for people do not change, so the skills, the crate READMEs and the site's sidebar keep
  their links.
- `docs/README.md` is an index by reader, in the site's groups. The decisions table moved to
  [`docs/decisions/README.md`](README.md).
- Still mixed after the change that brings this ADR: `docs/reference/parity.md` (its citations move once the open
  change to that page has merged), and the crate READMEs, which are written for people who change or embed a crate
  and keep their file citations. The large reference pages (`agent-files.md`, `a2a-server.md`, `coder-agent.md`,
  `child-runs.md`) are for people, but long; making them shorter is later work, one page at a time.
- A new agent page is added with its pair link; `docs-check` refuses the page otherwise.
