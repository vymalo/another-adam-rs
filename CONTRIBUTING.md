# Contributing to adam-rs

Thank you for helping. adam-rs is a Rust framework for **durable AI agents** and the home of the **coder**, the agent
that takes a coding task to a verified pull request. It is one of four repositories. The product roadmap, with every
open piece of work as a GitHub issue, is in
[`ROADMAP.md` of another-agentic-system](https://github.com/vymalo/another-agentic-system/blob/main/ROADMAP.md).
This repository's own library roadmap is [`docs/roadmap.md`](docs/roadmap.md).

## Find something to work on

1. Pick an issue from the roadmap: a `good first issue`, a `help wanted` ticket, or any ticket whose "Who and when"
   says anyone.
2. **Claim it** by commenting on the issue. If you stop, say so.
3. On a large or security ticket, **agree the approach in the issue** before you write code.

Read the ten rules in [`CLAUDE.md`](CLAUDE.md) first. Among them: every infrastructure boundary is a trait with a
conformance testkit, closed enums stay closed, and the coder fails closed. [`docs/architecture.md`](docs/architecture.md)
is the map.

## Set up, build and test

You need Rust (the MSRV is `rust-version` in `Cargo.toml`), `cargo-nextest`, Docker with Compose v2, and Node for the
docs checker.

```sh
docker compose up -d --wait               # the databases and mocks the tests use
export ADAM_TEST_POSTGRES_URL=postgres://postgres:postgres@127.0.0.1:5432/adam_test
export ADAM_TEST_MONGODB_URI=mongodb://127.0.0.1:27017
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --workspace --all-targets --locked --features adam-llm-agent/schema,adam-assembly/dev,adam/dev,adam-assembly/mcp,adam/mcp -- -D warnings
cargo test --workspace --doc --locked
cargo nextest run --workspace --locked --features adam-llm-agent/schema,adam-assembly/dev,adam/dev,adam-assembly/mcp,adam/mcp   # CI adds --profile ci
cargo test -p adam --test ui              # #[tool] compile tests; the rustc-worded half needs ADAM_TRYBUILD=1 on the CI toolchain
npm --prefix tools/docs-check ci          # once per clone
node tools/docs-check/check-docs.mjs      # diagrams, links, anchors, crate READMEs, first-party skills
```

A database suite is skipped when its variable is unset, so `cargo test` works with no databases; CI sets
`ADAM_TEST_REQUIRE_DB=1` so that it cannot pass by skipping. To run the coder on your machine against mocks, with no
credential: [Run it locally](docs/guides/run-locally.md). Every suite and end-to-end script:
[Testing](docs/guides/testing.md). CI is the reference: [`.github/workflows`](.github/workflows).

## Conventions

- **Conventional Commits:** `feat(scope): ...`, `fix`, `docs`, `test`, and so on. Nothing here enforces them (no hook,
  no CI check), but the history and the bump workflow use them. Please follow them.
- **Decisions are ADRs** in `docs/decisions/NNNN-kebab-title.md`, the next number, headed `# NNNN. Title` and
  `Status: **Accepted** (date)`. Add a row to the table in [`docs/README.md`](docs/README.md). Change a past decision
  with a dated *Amended* note, never silently. Use *Proposed* when the owner has yet to confirm it.
- **Mark facts** about third parties, and about what has been tried, as *verified* (date and source) or *unverified*.
- **Docs stay short and true.** Tables over prose, link instead of copying. A process gets a Mermaid pair: a
  `sequenceDiagram` and a `stateDiagram-v2`. A change that alters behaviour updates its page in the same pull request
  (the table in [`CLAUDE.md`](CLAUDE.md#documentation) says which). Comments in code only where the reason is not obvious.
- **Every crate has a `README.md`** next to its `Cargo.toml`. Update it in the same pull request as any change to the
  crate's public API, environment variables or tests. `docs-check` fails on a missing one.
- **A new required trait method breaks implementers.** Say so in the pull request and in the crate README.
- **Secrets** are never in files or logs. Do not edit `image.tag` in `deploy/coder/values.yaml` by hand: the coder
  workflow bumps it.

## Open a pull request

1. Work on a branch and open a pull request against `main`. Keep it to one concern.
2. There is no pull request template here. Say **what** changed and **why**, link the issue it solves, show **how you
   verified it** (the commands you ran and their results), name the **risks**, and declare any AI use (below).
3. The CI checks must pass: formatting, clippy, tests, the docs check, and the other jobs CI runs on your change.
4. A maintainer merges the pull request when CI is green.

## AI-assisted contributions

They are welcome. Say in the pull request description what the AI did, and confirm that you read, understand and
accept responsibility for every change, generated tests included. AI output is not evidence: run the checks yourself.
If you use a coding agent, point it at [`CLAUDE.md`](CLAUDE.md) (`AGENTS.md` is a link to it).

## Security

Do not report a vulnerability in a public issue. See [`SECURITY.md`](SECURITY.md).

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.
Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this work by you, as
defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
