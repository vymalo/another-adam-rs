# Skills: what this repository provides and how they are kept

For agents. The table of which skill to use when is in [`CLAUDE.md`](../../CLAUDE.md#skills); the first-party skills
themselves are in `.agents/skills/adam-*`.

## Skills this repo provides

Other repositories install the six public ones (`adam-agent-folder`, `adam-embed`, `adam-store-adapter`,
`adam-a2a-extensions`, `adam-coder-deploy`, `adam-upgrade`) with the CLI:

```sh
npx skills add vymalo/another-adam-rs --list
npx skills add vymalo/another-adam-rs --skill adam-agent-folder --skill adam-upgrade -a claude-code -y
npx skills update -p -y                   # later, in the consumer
```

The consumer's `skills-lock.json` pins them and each skill says to read adam-rs files at the pinned revision.
`update-vendored-skills` is `metadata.internal: true` and not listed; the CLI hides internal skills and those in
this repository's own `skills-lock.json` (*verified 2026-10-03* in `skills@1.7.0`, `dist/cli.mjs`
`discoverSkills`), so never list a first-party skill there.

## Writing a first-party skill

* Write it for a reader outside this repository: no relative links, adam-rs paths as code spans plus an absolute
  GitHub URL.
* A skill may cite a page of `docs/` (for people) or of `.agents/docs/` (for agents); cite the one whose reader the
  skill is sending there.
* When a doc moves, update every skill that cites it: docs-check fails on a cited path that does not exist, and
  checks the frontmatter and the mirrors ([the checks](writing-docs.md#the-checks)).

## Vendored skills

The vendored ones (`addyosmani/agent-skills`, `actionbook/rust-skills`, `leonardomso/rust-skills`, `docker/skills`)
are pinned in `skills-lock.json`: update them with the skills CLI through `update-vendored-skills`, never by hand
(licences: [`third-party-notices.md`](../../third-party-notices.md)).
