# Agent guide — another-adam-rs

`AGENTS.md` is a symlink to this file. Edit `CLAUDE.md` only.

## What this is

A Rust framework for **durable AI agents** (`adam-rs`): a run store behind a trait
(Postgres and MongoDB adapters), a durable agent loop with a journal and leases, A2A 1.0 servers
over it, agents authored as Markdown folders plus `#[tool]` functions, and two shipped agents:
`adam-coder` (a coding task to a verified pull request) and `adam-agent` (serves any agent folder).
Both ship in one image, `ghcr.io/vymalo/another-adam-rs/coder`, deployed by the chart in
`deploy/coder`. Everything infrastructural is a trait with a conformance testkit. Start at
`README.md`, then `docs/README.md` and `docs/architecture.md`.

This repository also **provides skills** to the repositories that integrate adam-rs: see
*Skills this repo provides*.

## Layout

| Path | What |
|---|---|
| `crates/` | libraries, one per concern, each with a `README.md` (the table in `README.md` links them all) |
| `bin/adam-coder`, `bin/adam-agent` | the two agents you can run (library and binary each), over `crates/adam-service` |
| `docs/` | `architecture.md` (crate map, ports, the path of a task, lifecycle, errors), `authoring.md` (agents as files, `#[tool]`), `decisions/NNNN-*.md` (ADRs) |
| `deploy/coder/` | Helm chart of the coder, `bump-tag.sh`, `tests/` (render checks, golden render, schemas) |
| `docker/coder/` | the image (`Dockerfile`) and its smoke tests (`test/`) |
| `dev/`, `compose.yaml` | local stack: Postgres, MongoDB, WireMock mocks of the model and GitHub, a git server, the example agent folders `dev/agents/*/agent`, end-to-end scripts `dev/*-e2e.sh` |
| `tools/docs-check/` | diagram, link and skill checker (also run in CI) |
| `.agents/skills/` | skills: first-party (`adam-*`, `update-vendored-skills`) and vendored (pinned in `skills-lock.json`); `.claude/skills`, `.goose/skills`, `.kiro/skills` are symlinks to them |
| `.github/workflows/` | `ci.yml` (Rust checks, docs, compose), `coder.yml` (image, chart, e2e, tag bump), `nightly.yml` (live services) |

## Rules — check every change against these

1. **Every crate has a `README.md`** next to its `Cargo.toml` (and `readme = "README.md"`).
   Update it in the same change as any change to the crate's public API, environment variables
   or tests. CI fails on a missing one; review checks the rest.
2. **Infrastructure is a trait with a testkit** (`Store` and `adam-store-testkit`, `Notifier` and
   `adam-notify-testkit`, `ModelClient`, `CodeHost`, ...). No driver type in a trait signature.
   A new required trait method breaks implementers: say so in the PR and in the crate README.
3. **Closed enums stay closed on purpose** (`Role`, `Placement`, `ClaimScope`): a new variant
   must fail to compile in every host and store.
4. **Fail closed**: no A2A bearer token, no server; an MCP server kind is allowed by the
   deployment, not by the agent's files; secrets are never in files or logs.
5. **A notification is never the truth**: signals are a latency optimisation, polling and the
   store decide.
6. **Errors carry a class** (`adam_error::Classify`): callers decide from the class, never from
   the variant. `unwrap` and `expect` warn in the workspace (`[workspace.lints.clippy]`); tests may.
7. **Tests that need a database skip when their variable is unset**, and fail instead under
   `ADAM_TEST_REQUIRE_DB=1` (CI sets it).
8. **Decisions are ADRs** in `docs/decisions/NNNN-kebab-title.md`, next number, headed
   `# NNNN. Title` and `Status: **Accepted** (date)`. Change a past decision with a dated
   *Amended* note, never silently.
9. **Mark facts** about third parties *verified* (date and source) or *unverified*.

## Writing docs

* A process gets a Mermaid pair: a `sequenceDiagram` for the interaction and a `stateDiagram-v2`
  for the lifecycle; prose says what they cannot. Every node and call must exist in the code.
* Cite files (`crates/adam-runtime/src/runtime.rs`); do not copy a crate README, link to it.
* `node tools/docs-check/check-docs.mjs` parses every diagram, resolves every relative link and
  `#heading`, requires a README per crate, and checks the first-party skills.

## Commands

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --workspace --all-targets --locked --features adam-llm-agent/schema,adam-assembly/dev,adam/dev,adam-assembly/mcp,adam/mcp -- -D warnings
cargo test --workspace --doc --locked
cargo nextest run --workspace --locked --features adam-llm-agent/schema,adam-assembly/dev,adam/dev,adam-assembly/mcp,adam/mcp   # CI runs it with --profile ci
cargo test -p adam --test ui              # `#[tool]` compile tests; the rustc-worded half needs ADAM_TRYBUILD=1 on the CI toolchain
docker compose up -d --wait               # the databases and mocks the tests use (README "Testing")
npm --prefix tools/docs-check ci          # once per clone
node tools/docs-check/check-docs.mjs
```

MSRV is `rust-version` in `Cargo.toml` (CI job `msrv`). Database suites need
`ADAM_TEST_POSTGRES_URL` and `ADAM_TEST_MONGODB_URI` (see `README.md`, "Testing"). The CI jobs are
the reference: `.github/workflows/ci.yml` and `.github/workflows/coder.yml`.

## Commits and pull requests

The history uses Conventional Commits (`feat(scope): ...`, `fix`, `docs`, `test`, `chore(deploy):
bump coder to sha-<7>` by the workflow). No tool here enforces it and the repository has no pull
request template (*verified 2026-10-03*: no `tools/commit-lint*`, no `.github/PULL_REQUEST_TEMPLATE*`,
no commit-lint workflow). Work on a branch, open a pull request against `main`; never edit
`deploy/coder/values.yaml` `image.tag` by hand (the coder workflow bumps it).

## Skills

Skills live in `.agents/skills/` (symlinked into `.claude/skills`, `.goose/skills` and
`.kiro/skills`). The vendored ones come from `addyosmani/agent-skills`,
`actionbook/rust-skills`, `leonardomso/rust-skills` and `docker/skills`, pinned in
`skills-lock.json`; update them with the skills CLI through `update-vendored-skills`, never by
hand-editing their files. Licences: `third-party-notices.md`.

**Precedence when they disagree:** this file's *Rules* → first-party skills (`adam-*`) →
vendored skills. Start with `using-agent-skills` if unsure which applies.

| When you are… | Use |
|---|---|
| Writing or changing an agent that is only a folder | **`adam-agent-folder`** |
| Hosting adam agents in a Rust program, adding a `#[tool]` | **`adam-embed`** |
| Implementing or changing a `Store` or `Notifier` | **`adam-store-adapter`** |
| Touching A2A extensions (the card, activation) | **`adam-a2a-extensions`** |
| Changing the image or the chart, releasing | **`adam-coder-deploy`** |
| Moving a consumer between adam-rs revisions | **`adam-upgrade`** |
| Updating the vendored skills | **`update-vendored-skills`** (internal) |
| Recording a decision | `documentation-and-adrs` for the reasoning; the ADR format is *Rules* 8 |
| Turning a vague request into a design or spec | `idea-refine`, `spec-driven-development`, `planning-and-task-breakdown` |
| Checking a claim about a library or protocol | `source-driven-development` (mark it *verified*) |
| Designing a trait, a port or a public API | `api-and-interface-design`; `m04-zero-cost`, `m05-type-driven` |
| Implementing anything | `incremental-implementation`, `test-driven-development` |
| Rust, first stop | `rust-router` (dispatches to `m01`…`m15`), `rust-skills`, `coding-guidelines` |
| Rust errors, async, workspace and features | `m06-error-handling`, `m07-concurrency`, `m11-ecosystem` |
| HTTP, A2A, MCP adapters | `domain-web`, `security-and-hardening` |
| Containers, Kubernetes, probes | `domain-cloud-native`, `docker-build-strategies`, `docker-compose-patterns` |
| CI workflows | `ci-cd-and-automation` |
| Logs, metrics, traces | `observability-and-instrumentation` |
| Something broke | `debugging-and-error-recovery` |
| Before opening or merging a pull request | `code-review-and-quality`, `code-simplification`, `git-workflow-and-versioning` |
| Editing this file or other agent context | `context-engineering` |

Off-domain here: `domain-cli`, `domain-embedded`, `domain-fintech`, `domain-iot`, `domain-ml`.
Not for direct use: the `core-*` helpers, `meta-cognition-parallel`, `rust-skill-creator`,
`rust-daily`, `m14-mental-model`.

## Skills this repo provides

Other repositories install adam-rs's first-party skills with the skills CLI, as they do any
vendored skill:

```sh
npx skills add vymalo/another-adam-rs --list                       # the six public skills
npx skills add vymalo/another-adam-rs --skill adam-agent-folder --skill adam-upgrade -a claude-code -y
npx skills update -p -y                                            # later, in the consumer
```

The public skills are `adam-agent-folder`, `adam-embed`, `adam-store-adapter`,
`adam-a2a-extensions`, `adam-coder-deploy` and `adam-upgrade`. The consumer's `skills-lock.json`
pins them, and each skill says to read adam-rs files at the revision the consumer pins.
`update-vendored-skills` is `metadata.internal: true` and is not listed. The CLI lists only these
six because it hides skills that are in the repository's own `skills-lock.json` (the vendored
ones) and internal ones (*verified 2026-10-03* in `skills@1.7.0`, `dist/cli.mjs`
`discoverSkills`). Skills here are written for a reader outside this repository: no relative
links, adam-rs paths as code spans plus an absolute GitHub URL. Never list a first-party skill in
`skills-lock.json`. `tools/docs-check` checks their frontmatter, mirrors and cited paths.
