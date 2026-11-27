# Writing docs

For agents: [writing-docs.md](../.agents/docs/writing-docs.md), with the full table of which page a change updates and
what the checks enforce.

`docs/` is for people and `.agents/docs/` is for agents ([ADR 0079](decisions/0079-docs-for-people-and-docs-for-agents-live-apart.md)).
A page for people gives the answer first and leaves evidence and procedures to the agent page at the same path.

## The rules

* **Short pages, the answer first.** Tables and diagrams over prose. Link instead of copying; never copy a crate README.
* **Mermaid pairs.** A process gets a `sequenceDiagram` for the interaction and a `stateDiagram-v2` for the lifecycle,
  then prose for what they cannot say. Every node, edge and state must exist in the code.
* **Mark facts.** A claim about a third party, or about what has been tried, is *verified* (with the date and the
  source) or *unverified*.
* **Same pull request.** A change that alters behaviour updates its page in the same pull request. Which page depends
  on what changed: [the table](../.agents/docs/writing-docs.md#which-page-a-change-updates).
* **A pair links both ways.** `docs/<p>.md` and `.agents/docs/<p>.md` point at each other, and the page for people
  decides when they disagree.

## Check and preview

```sh
npm --prefix tools/docs-check ci          # once per clone
node tools/docs-check/check-docs.mjs      # diagrams, links, anchors, crate READMEs, skills, the split

npm --prefix docs ci                      # once per clone
npm --prefix docs run dev                 # the site with live reload
npm --prefix docs run build               # what CI builds; it fails on a dead link
```

The build lets through only links to source files outside `docs/`, which `check-docs.mjs` checks instead. A file
outside `docs/` cannot be included in a page, because its relative links would resolve from the wrong place: link it.

## The site's structure

The sidebar is built in `docs/.vitepress/config.ts`, and a folder lists its pages by their first heading. The seven
sections and their order are in `docs/.vitepress/STRUCTURE.md`, shared with the other repositories' sites. A new guide
must be listed in a group of the sidebar, or the build fails.
