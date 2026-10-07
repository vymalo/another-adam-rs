# Testing

```sh
docker compose up -d --wait        # postgres, mongodb and the mocks
export ADAM_TEST_POSTGRES_URL=postgres://postgres:postgres@127.0.0.1:5432/adam_test
export ADAM_TEST_MONGODB_URI=mongodb://127.0.0.1:27017
export ADAM_TEST_MOCK_OPENAI_URL=http://127.0.0.1:8081      # the root of the mock, no /v1
export ADAM_TEST_MOCK_GITHUB_URL=http://127.0.0.1:8082
cargo test --workspace
```

Each database suite is **skipped when its variable is unset**, so `cargo test` works with no databases (only the
in-memory store runs). The same holds for the two tests that check the real clients against the compose mocks
(`adam-model-openai/tests/wiremock_compose.rs`, `adam-workspace/tests/wiremock_compose.rs`). Suites isolate
their cases by agent name, so they run in parallel on one shared database with no cleanup between runs.

**CI must not pass by skipping.** `ADAM_TEST_REQUIRE_DB=1` makes a suite whose variable is unset **fail**; CI sets
it in every job that has the databases. Gate a new database test with
`adam_core::testing::test_env("ADAM_TEST_...")`, which honours the flag.

## The commands CI runs

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --workspace --all-targets --locked --features adam-llm-agent/schema,adam-assembly/dev,adam/dev,adam-assembly/mcp,adam/mcp -- -D warnings
cargo test --workspace --doc --locked
cargo nextest run --workspace --locked --features adam-llm-agent/schema,adam-assembly/dev,adam/dev,adam-assembly/mcp,adam/mcp   # CI adds --profile ci
cargo test -p adam --test ui                                  # #[tool] compile tests (trybuild)
npm --prefix tools/docs-check ci && node tools/docs-check/check-docs.mjs   # diagrams, links, anchors, crate READMEs, skills
```

CI also runs the MSRV build (`rust-version` in `Cargo.toml`), `cargo deny`, line coverage
(`cargo llvm-cov nextest --workspace`), and a nightly live-model and live-OpenCode smoke
(`.github/workflows/nightly.yml`, needs secrets). The CI jobs are the reference:
`.github/workflows/ci.yml` and `coder.yml`.

## Suites that need a service of their own

They are gated apart, so `ADAM_TEST_REQUIRE_DB=1` does not turn them on.

| Suite | Switch | Needs |
|---|---|---|
| devcontainer | `ADAM_TEST_DEVCONTAINER=1` (`ADAM_TEST_REQUIRE_DEVCONTAINER=1` to fail instead of skip) | a rootless Podman service |
| Kubernetes run pods (`adam-env-kubernetes`) | `ADAM_TEST_KUBECONFIG` (`ADAM_TEST_REQUIRE_KUBERNETES=1`) | a cluster; CI's `run-pods` job makes a `kind` cluster with `deploy/coder/tests/kind-run-pods.sh`, which you can run with `kind` |
| `#[tool]` compile errors worded by rustc | `ADAM_TRYBUILD=1` | the CI toolchain (the `ui` job); the errors the macro writes itself always run ([`adam` README](../../crates/adam/README.md#tests)) |
| live model, live OpenCode | `ADAM_TEST_OPENAI_*`, `ADAM_TEST_OPENCODE=1` | secrets |

All test variables: [Environment](../reference/environment.md#tests).

## What the store suite covers

27 cases: exact JSON round trip (unicode, `i64` bounds, floats, special keys), CAS conflicts, 16-way concurrent
commits with one winner, journal ordering, first-writer-wins and 16-way races, non-determinism detection, due
rules, agent filtering and limits, busy runs left unclaimed, 8 workers claiming 60 runs with no double lease,
lease expiry and takeover, renew and release, pinned claims (an owned run never goes to another worker, 4 workers
racing), one open run per conversation including a 16-way race, and purging.

## Adding a backend

Implement `Store` and add one line: `adam_store_testkit::store_conformance!(make_store);`. A new `Notifier` runs
`adam_notify_testkit::notifier_conformance!(make_pair);` (the Postgres one needs `ADAM_TEST_POSTGRES_URL`, a
superuser, for its reconnect test). See [Store adapters](../reference/store-adapters.md#adding-a-backend).

## End-to-end scenarios

Scripts in `dev/` drive the compose stack through A2A and assert the whole chain on the mocks. Each script's
header lists its steps and variables. CI runs them in `.github/workflows/coder.yml` ("Compose e2e") on the image
it has just built. Start the stack first:

```sh
docker compose --profile app up -d --build --wait postgres mock-openai mock-github mock-github-mcp git-server coder
```

| Script | What it proves |
|---|---|
| `sh dev/coder-e2e.sh` | one task ends in a pushed branch and one pull request; the `checks`, `branch` and `pull_request` artifacts; GitHub read over MCP with the coder's credentials of each call. Variants: `NO_OPENCODE=1` (the check command makes the change), `SCENARIO=files` (the coder edits files itself), `scratch` (no repository named, then published), `second-repo` and `create-repo` (each with `ANSWER=yes` or `no`: consent is asked and honoured), `GITHUB_AUTH=app` (with `-f dev/compose.github-app.yaml`) |
| `SCENARIO=devcontainer\|default-env\|broken-env\|no-runtime sh dev/coder-e2e.sh` | the repository's own devcontainer is the environment, a repository without one gets the default image, a broken file is refused and the person decides, a stopped Podman service is a step that says so. Needs `-f dev/compose.devcontainer.yaml` |
| `sh dev/greeting-e2e.sh` | "hi" gets a greeting built from the folder's persona lines; the same task goes on to a pull request; a restart on an edited folder changes the answer |
| `sh dev/coder-choices-e2e.sh` | three questions asked as one A2UI form, one action answers them; a screen that cannot read it gets the options as text |
| `sh dev/agent-e2e.sh` | the general agent answers "hi" in role; a restart on an edited folder changes the card and the answer |
| `sh dev/agent-cards-e2e.sh` | the researcher folder answers with a surface of cards and a graph on a screen that has them, words only on one that has not |

How the scripted models and mocks work, and how to change a script: [the local stack](../reference/dev-stack.md).
Exit and assertion details are in each script's header; `PRELOAD_FROM_DOCKER=1` and `COMPOSE_CMD` are
documented in `dev/coder-e2e.sh`.

## Other checks

| Check | Command |
|---|---|
| chart render guarantees | `sh deploy/coder/tests/render-check.sh` (also kubeconform and golden renders in CI), `sh deploy/coder/tests/bump-tag-test.sh` |
| image smoke tests | `sh docker/coder/test/container-smoke.sh <image>`, `sh docker/coder/test/agent-smoke.sh <image>`; with `EXPECT_REVISION=<commit>` (the image's `ADAM_BUILD_REVISION`) they also check the card's version and `build/v1`. Both run `docker/coder/test/http-smoke.sh`, which also checks the card's two interfaces, that `/docs` and `/openapi.json` are public and that the HTTP+JSON binding needs the token |
| shell scripts | `shellcheck deploy/coder/bump-tag.sh deploy/coder/tests/*.sh docker/coder/test/*.sh dev/*.sh` |
| a model mock's streamed twin | `cargo test -p adam-model-openai --test wiremock_compose` (plays every script both ways) |
