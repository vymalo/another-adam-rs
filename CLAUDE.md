# Agent guide — another-adam-rs

`AGENTS.md` is a symlink to this file. Edit `CLAUDE.md` only.

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
| `README.md`, `docs/` | the human docs: `docs/README.md` (index), `architecture.md`, `guides/`, `reference/`, `roadmap.md`, `decisions/NNNN-*.md` (ADRs) |
| `crates/`, `bin/adam-coder`, `bin/adam-agent` | libraries, one per concern, and the two agents (library and binary each); each has a `README.md` |
| `deploy/coder/`, `docker/coder/` | the Helm chart (`bump-tag.sh`, `tests/`: renders, goldens, schemas) and the image with its smoke tests |
| `dev/`, `compose.yaml` | local stack: databases, WireMock mocks, a git server, example agent folders `dev/agents/*/agent`, e2e scripts `dev/*-e2e.sh` |
| `tools/docs-check/` | diagram, link, anchor, crate-README and skill checker (also in CI) |
| `.agents/skills/` | first-party (`adam-*`, `update-vendored-skills`) and vendored skills (`skills-lock.json`); `.claude/skills`, `.goose/skills`, `.kiro/skills` are symlinks |
| `.github/workflows/` | `ci.yml` (Rust, docs, compose), `coder.yml` (image, chart, e2e, tag bump), `nightly.yml` (live services) |

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
   `Status: **Accepted** (date)`. Change a past decision with a dated *Amended* note, never silently.
9. **Mark facts** about third parties *verified* (date and source) or *unverified*.
10. **Documentation**: keep the human docs short and current, and comments rare (below).

## Documentation

Humans read `README.md` and `docs/`: colleagues who test the system need short, targeted pages, clear
architecture with pictures, and deploy instructions. Agent-only context lives here and in skills, never in `docs/`.

* **A change that alters behaviour updates its page in the same PR:**

  | You change | Update |
  |---|---|
  | an environment variable, default or exit code | `docs/reference/environment.md`, `docs/reference/errors.md`, the crate README |
  | a process, lifecycle, port, role or the store schema | `docs/architecture.md` (diagram and tables) |
  | the agent file format, validation, `#[tool]` | `docs/reference/agent-files.md`, `docs/guides/write-an-agent.md` |
  | the chart or the image | `deploy/coder/README.md`, `docs/guides/deploy-the-coder.md` |
  | compose, a mock, a scripted model, an e2e script | `docs/reference/dev-stack.md`, `docs/guides/run-locally.md`, `docs/guides/testing.md` |
  | a path a first-party skill cites | the skill (docs-check fails on a missing path) |

* **Short.** Short sentences, tables, no repeated explanations: link instead of copying, never copy a crate
  README. `README.md` stays about 100 lines; a guide covers one task.
* **Pictures.** A process gets a Mermaid pair, a `sequenceDiagram` for the interaction and a
  `stateDiagram-v2` for the lifecycle; prose says only what they cannot. Every node, edge, state and call
  must exist in the code; cite files (`crates/adam-runtime/src/runtime.rs`).
* **Comments in code** only where the reason is not obvious: no restating the code, no essays, no history
  ("added in slice N"). What a user needs belongs in a README or `docs/`.
* `node tools/docs-check/check-docs.mjs` parses every diagram, resolves every relative link and `#heading`,
  requires a README per crate and checks the first-party skills.

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
```

MSRV is `rust-version` in `Cargo.toml` (CI job `msrv`). Database suites need `ADAM_TEST_POSTGRES_URL` and
`ADAM_TEST_MONGODB_URI`. The CI jobs are the reference: `.github/workflows/ci.yml` and `coder.yml`.

## Commits and pull requests

Conventional Commits (`feat(scope): ...`, `fix`, `docs`, `test`; the workflow writes `chore(deploy): bump
coder to sha-<7>`). Nothing here enforces it and there is no pull request template (*verified 2026-10-03*).
Work on a branch, open a pull request against `main`; never edit `deploy/coder/values.yaml` `image.tag` by
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

**Skills this repo provides.** Other repositories install the six public ones (`adam-agent-folder`,
`adam-embed`, `adam-store-adapter`, `adam-a2a-extensions`, `adam-coder-deploy`, `adam-upgrade`) with the CLI:

```sh
npx skills add vymalo/another-adam-rs --list
npx skills add vymalo/another-adam-rs --skill adam-agent-folder --skill adam-upgrade -a claude-code -y
npx skills update -p -y                   # later, in the consumer
```

The consumer's `skills-lock.json` pins them and each skill says to read adam-rs files at the pinned revision.
`update-vendored-skills` is `metadata.internal: true` and not listed; the CLI hides internal skills and those in
this repository's own `skills-lock.json` (*verified 2026-10-03* in `skills@1.7.0`, `dist/cli.mjs`
`discoverSkills`), so never list a first-party skill there. Skills are written for a reader outside this
repository: no relative links, adam-rs paths as code spans plus an absolute GitHub URL. docs-check checks
their frontmatter, mirrors and cited paths.
