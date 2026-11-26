# The shared structure of the documentation sites

another-agentic-system, another-agentic-platform and another-adam-rs publish one VitePress site each, built from the
repository's own `docs/` with `@vaam-apps/vitepress-md3`, so a pull request updates its docs in the same change. All use
the same seven sections, in this order, so a reader can move from one site to another. The other repositories copy this
file and never diverge.

| # | Section | Holds |
|---|---|---|
| 1 | Overview | What the product is, where it stands, a picture (vision, MVP, requirements, open questions) |
| 2 | Get started | Run it locally; the first success |
| 3 | Guides | One task per page (plans and how-tos) |
| 4 | Reference | The API (the OpenAPI document rendered), config, environment, errors, extension contracts |
| 5 | Architecture | Components and lifecycles, as Mermaid pairs |
| 6 | Decisions | The ADR index and every ADR, listed and never rewritten |
| 7 | Contributing | How to build, test and open a pull request; security; roadmap |

Rules: the sidebar is built in `.vitepress/config.ts` (a folder lists its pages by first heading); existing pages are
mapped into sections and not moved; a page moves only when the structure needs it, and then every link is fixed;
Mermaid renders and local search is on; the build fails on a dead link, and only links to source files outside `docs/`
(which `tools/docs-check` checks) are let through by pattern; the base path is `/<repository>/` (`DOCS_BASE` overrides it).
