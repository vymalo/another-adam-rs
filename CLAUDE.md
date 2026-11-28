# Agent guide — another-adam-rs

`AGENTS.md` is a symlink to this file. Edit `CLAUDE.md` only. This is the index for coding agents: people start at
`README.md` and `docs/`; the procedures, invariants and evidence for agents are in `.agents/docs/`
([ADR 0079](docs/decisions/0079-docs-for-people-and-docs-for-agents-live-apart.md)).

## What this is

A Rust framework for **durable AI agents** (`adam-rs`): a run store behind a trait (Postgres, MongoDB), a
durable agent loop with a journal and leases, A2A 1.0 servers over it, agents authored as Markdown folders
plus `#[tool]` functions, and two shipped agents, `adam-coder` (a coding task to a verified pull request)
and `adam-agent` (serves any agent folder). Both ship in one image, `ghcr.io/vymalo/another-adam-rs/coder`,
deployed by the chart in `deploy/coder`. Everything infrastructural is a trait with a conformance testkit.
Start at `README.md`, then `docs/README.md`. This repository also **provides skills** to the repositories
that integrate adam-rs.

## Layout

| Path | What |
|---|---|
| `README.md`, `docs/` | the human docs: `docs/README.md` (index), `architecture.md`, `guides/`, `reference/`, `roadmap.md` (the library's own; the product roadmap is `ROADMAP.md` in another-agentic-system, https://github.com/vymalo/another-agentic-system/blob/main/ROADMAP.md), `decisions/NNNN-*.md` (ADRs) |
| `.agents/docs/` | the docs for agents, not published: [its index](.agents/docs/README.md), [writing docs](.agents/docs/writing-docs.md) (the split, which page a change updates, the checks), [skills](.agents/docs/skills.md), and the agent page of a `docs/` page at the same relative path |
| `CONTRIBUTING.md`, `SECURITY.md` | how to contribute (claim an issue, build, test, open a pull request) and how to report a vulnerability privately; the front door for outside contributors |
| `crates/`, `bin/adam-coder`, `bin/adam-agent` | libraries, one per concern, and the two agents (library and binary each); each has a `README.md` |
| `deploy/coder/`, `docker/coder/` | the Helm chart (`bump-tag.sh`, `tests/`: renders, goldens, schemas) and the image with its smoke tests. **`deploy/coder` is read by another repository**: vymalo/another-agentic-platform renders its parity goldens from this chart at a pinned revision (its `tools/adam-parity/regen.sh`), so what the chart renders for a given value is a contract; say so in the pull request when it changes |
| `dev/`, `compose.yaml` | local stack: databases, WireMock mocks, a git server, example agent folders `dev/agents/*/agent`, e2e scripts `dev/*-e2e.sh` |
| `tools/docs-check/` | diagram, link, anchor, crate-README and skill checker (also in CI) |
| `bench/` | the benchmark harness for adam-coder and OpenCode (ADR 0048): Node ESM, no dependencies, outside the Rust workspace; its configuration (endpoint, model, prices, key) is the owner's and **never in the repository**; `bench/README.md`; the agents are pinned in `bench/pins.json`, the stack is `bench/compose.yaml`, `bench/dry-run.sh` is the docker dry run; CI `bench.yml` (`node --test "bench/test/*.test.mjs"`), the dry run `bench-dry-run.yml` (`workflow_dispatch` only) |
| `.agents/skills/` | first-party (`adam-*`, `update-vendored-skills`) and vendored skills (`skills-lock.json`); `.claude/skills`, `.goose/skills`, `.kiro/skills` are symlinks |
| `.github/workflows/` | `ci.yml` (Rust, docs, compose), `coder.yml` (image, chart, e2e, tag bump), `nightly.yml` (live services), `cancel-stale-runs.yml` (cancels a branch's queued and running runs when its pull request closes or the branch is deleted; a newer push is each workflow's own `concurrency`) |

## Rules — check every change against these

1. **Every crate has a `README.md`** next to its `Cargo.toml` (and `readme = "README.md"`). Update it in the
   same change as any change to the crate's public API, environment variables or tests. CI fails on a
   missing one; review checks the rest.
2. **Infrastructure is a trait with a testkit** (`Store` and `adam-store-testkit`, `Notifier` and
   `adam-notify-testkit`, `ModelClient`, `CodeHost`, ...). No driver type in a trait signature. A new
   required trait method breaks implementers: say so in the PR and in the crate README.
3. **Closed enums stay closed on purpose** (`Role`, `Placement`, `ClaimScope`): a new variant must fail to
   compile in every host and store.
4. **Fail closed**: no A2A bearer token, no server; an MCP server kind is allowed by the deployment, not by
   the agent's files; secrets are never in files or logs.
5. **A notification is never the truth**: signals are a latency optimisation, polling and the store decide.
6. **Errors carry a class** (`adam_error::Classify`): callers decide from the class, never from the variant.
   `unwrap` and `expect` warn in the workspace (`[workspace.lints.clippy]`); tests may.
7. **Tests that need a database skip when their variable is unset**, and fail instead under
   `ADAM_TEST_REQUIRE_DB=1` (CI sets it).
8. **Decisions are ADRs** in `docs/decisions/NNNN-kebab-title.md`, next number, headed `# NNNN. Title` and
   `Status: **Accepted** (date)`, with a row in `docs/decisions/README.md`. Change a past decision with a dated
   *Amended* note, never silently.
9. **Mark facts** about third parties *verified* (date and source) or *unverified*.
10. **Documentation**: `docs/` is for people, `.agents/docs/` for agents, and each pair links both ways (ADR 0079).
    Keep the human docs short and current, and comments rare (below).

## Documentation

* `README.md` and `docs/` are for **people**: the answer first, short sentences, tables, Mermaid pairs, file names
  but no `path:line`. They are published as the site.
* `.agents/docs/` is for **agents**: procedures, invariants to check, evidence (`path:line`, tests, commits), the gates.
  A page there sits at the same relative path as its page for people, and the two link to each other.
* **A change that alters behaviour updates its page in the same PR.** Which page, how to write it, and what
  `docs-check` enforces: [writing docs](.agents/docs/writing-docs.md).
* `node tools/docs-check/check-docs.mjs` parses every diagram, resolves every relative link and `#heading`
  (`.agents/docs` included), requires a README per crate, checks the first-party skills, and holds the split.

## Commands

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --workspace --all-targets --locked --features adam-llm-agent/schema,adam-assembly/dev,adam/dev,adam-assembly/mcp,adam/mcp -- -D warnings
cargo test --workspace --doc --locked
cargo nextest run --workspace --locked --features adam-llm-agent/schema,adam-assembly/dev,adam/dev,adam-assembly/mcp,adam/mcp   # CI adds --profile ci
cargo test -p adam --test ui              # `#[tool]` compile tests; the rustc-worded half needs ADAM_TRYBUILD=1 on the CI toolchain
docker compose up -d --wait               # the databases and mocks the tests use (docs/guides/testing.md)
npm --prefix tools/docs-check ci          # once per clone
node tools/docs-check/check-docs.mjs
git config core.hooksPath .githooks       # once per clone: the local Conventional Commits hook
```

MSRV is `rust-version` in `Cargo.toml` (CI job `msrv`). Database suites need `ADAM_TEST_POSTGRES_URL` and
`ADAM_TEST_MONGODB_URI`. The CI jobs are the reference: `.github/workflows/ci.yml` and `coder.yml`.

## Commits and pull requests

Conventional Commits (`feat(scope): ...`, `fix`, `docs`, `test`; the workflow writes `chore(deploy): bump
coder to sha-<7>`), enforced by `tools/commit-lint.sh` in the local hook (`git config core.hooksPath .githooks`, once
per clone) and in CI (`commit-lint.yml`, the commits and the pull request's title): release-please reads the subjects to
choose the next version (ADR 0084). There is no pull request template (*verified 2026-11-27*). Work on a branch, open a pull request against `main`; never edit `deploy/coder/values.yaml` `image.tag` by
hand (the coder workflow bumps it).

## Skills

Skills live in `.agents/skills/`. The vendored ones (`addyosmani/agent-skills`, `actionbook/rust-skills`,
`leonardomso/rust-skills`, `docker/skills`) are pinned in `skills-lock.json`: update them with the skills CLI
through `update-vendored-skills`, never by hand (licences: `third-party-notices.md`). **Precedence:** this
file's *Rules* → first-party skills (`adam-*`) → vendored skills. Start with `using-agent-skills` if unsure.

| When you are… | Use |
|---|---|
| Writing or changing an agent that is only a folder | **`adam-agent-folder`** |
| Hosting adam agents in a Rust program, adding a `#[tool]` | **`adam-embed`** |
| Implementing or changing a `Store` or `Notifier` | **`adam-store-adapter`** |
| Touching A2A extensions (the card, activation) | **`adam-a2a-extensions`** |
| Changing the image or the chart, releasing | **`adam-coder-deploy`** |
| Moving a consumer between adam-rs revisions | **`adam-upgrade`** |
| Updating the vendored skills | **`update-vendored-skills`** (internal) |
| A decision, a vague request, a claim to check | `documentation-and-adrs` (format: Rule 8), `idea-refine`, `spec-driven-development`, `planning-and-task-breakdown`, `source-driven-development` |
| Designing a trait, port or public API | `api-and-interface-design`, `m04-zero-cost`, `m05-type-driven` |
| Implementing; something broke | `incremental-implementation`, `test-driven-development`; `debugging-and-error-recovery` |
| Rust | `rust-router` (dispatches to `m01`…`m15`), `rust-skills`, `coding-guidelines`; `m06-error-handling`, `m07-concurrency`, `m11-ecosystem` |
| HTTP, A2A, MCP; containers; CI; logs | `domain-web`, `security-and-hardening`; `domain-cloud-native`, `docker-*`; `ci-cd-and-automation`; `observability-and-instrumentation` |
| Before a pull request; editing this file | `code-review-and-quality`, `code-simplification`, `git-workflow-and-versioning`; `context-engineering` |

Off-domain here: `domain-cli`, `domain-embedded`, `domain-fintech`, `domain-iot`, `domain-ml`. Not for direct
use: the `core-*` helpers, `meta-cognition-parallel`, `rust-skill-creator`, `rust-daily`, `m14-mental-model`.

**Skills this repo provides.** Other repositories install the six public ones (`adam-agent-folder`, `adam-embed`,
`adam-store-adapter`, `adam-a2a-extensions`, `adam-coder-deploy`, `adam-upgrade`) with `npx skills add
vymalo/another-adam-rs`. How they are installed, pinned and written: [skills](.agents/docs/skills.md).
