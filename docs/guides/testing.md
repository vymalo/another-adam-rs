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
(`adam-model-openai/tests/wiremock_compose.rs`, `adam-workspace/tests/wiremock_compose.rs`, `adam-mcp/tests/wiremock_compose.rs`, which also reads `ADAM_TEST_MOCK_GITHUB_MCP_URL` and `ADAM_TEST_MOCK_IMAGE_MCP_URL`). Suites isolate
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
sh tools/check-workflow-pins.sh   # every action a workflow uses is at or above the floor recorded in tools/workflow-pins.txt```

CI also runs the MSRV build (`rust-version` in `Cargo.toml`), `cargo deny`, line coverage
(`cargo llvm-cov nextest --workspace`, on main: the two conformance arms run the suite on every pull request),
and a nightly live-model smoke and a live test of `adam-acp` against OpenCode
(`.github/workflows/nightly.yml`, needs secrets). The CI jobs are the reference:
`.github/workflows/ci.yml` and `coder.yml`.

Each push to `main` that triggers `ci` gets a run with a concurrency group of its own (the pushed head sha), so a
later push does not replace it; a pull request's runs share one group per PR (`refs/pull/N/merge`), where a new
head cancels the run it supersedes. GitHub cancels a *pending* run as soon as a newer one joins its group, even
with `cancel-in-progress: false`, which is why a group shared by a busy `main` left merges without a verdict
([ADR 0084](../decisions/0084-releases-are-cut-by-release-please-and-production-pins-a-version.md)). The
`coverage` job, which opens and closes one issue on `main`, runs one at a time for the same reason. `coder.yml`
uses a per-sha group only for release commits and manual dispatches; its other pushes to `main` still share a
group and skip to the newest.

## Tests that start a binary

The git sandbox's tests start the real server (`adam-git-sandbox`) as a process and run `git` through it: the crate's own tests use
`CARGO_BIN_EXE_*`; the tests of `adam-workspace` and `adam-coder` build the binary first with `cargo build --locked -p
adam-git-sandbox` in the target directory and profile of the running test binary (`adam_workspace::testing_sandbox`, as
`adam-coder`'s tests build the fake ACP agent), so the first of them takes a moment and a build in progress makes the others wait. They
need `git`, a Unix socket and `/proc`, and nothing else; they prove that the commands go where they should and that what a repository
runs runs under the server, **not** the container isolation, which is the chart's and is shown on a cluster by the `run-pods` job (`deploy/coder/tests/kind-run-pods.sh`, below) and,
on the render, by `deploy/coder/tests/render-check.sh` ([`adam-git-sandbox`](../../crates/adam-git-sandbox/README.md#what-it-does-not-do)).

## Suites that need a service of their own

They are gated apart, so `ADAM_TEST_REQUIRE_DB=1` does not turn them on.

| Suite | Switch | Needs |
|---|---|---|
| Kubernetes run pods (`adam-env-kubernetes`) | `ADAM_TEST_KUBECONFIG` (`ADAM_TEST_REQUIRE_KUBERNETES=1`) | a cluster; CI's `run-pods` job makes a `kind` cluster (the node image `ci.yml` pins, whose network plugin enforces `NetworkPolicy`) and runs `deploy/coder/tests/kind-run-pods.sh`, which you can run with `kind`: the cluster test, then the run pods' network policy and the egress proxy ([ADR 0042](../decisions/0042-run-pods-reach-the-internet-through-an-egress-proxy.md); it fails where a policy is not enforced unless `ADAM_TEST_CNI_MAY_IGNORE_POLICIES=1`), then the git sandbox ([ADR 0041](../decisions/0041-a-run-pod-sees-only-its-own-run.md), open decision 6): a pod made from the chart's own StatefulSet, with canaries for the secrets of the coder container, shows that the sandbox container has no token, Secret or view of the coder's processes, that a commit through its socket works, and that a scratch project whose `.git/config` names a clean filter and an fsmonitor runs them in the sandbox, with a marker that holds no canary, where plain git in the coder container (the control) reads them all; it builds `adam-egress`, `adam-git-sandbox` and `adam-git` with cargo and needs `docker`, `kubectl`, `helm` and `python3` |
| git under a run pod's mounts (ADR 0041) | `ADAM_TEST_REQUIRE_MOUNTS=1` to fail instead of skip | root, `unshare` and `setpriv`: `sudo sh deploy/coder/tests/git-in-run-pod.sh` builds the pod's view with bind mounts (the read-only `HEAD`, `commondir`, `gitdir` and `logs` of the worktree's directory included: it tries to write, rename over, replace and remove them; and each slot as a mount of its own: it tries to rename, remove and exchange it and expects `EBUSY`) and runs real git in it |
| the real language servers (ADR 0044 slice 17) | `ADAM_TEST_LANGUAGE_SERVERS=1` (`ADAM_TEST_REQUIRE_LANGUAGE_SERVERS=1` to fail instead of skip) | `rust-analyzer`, `typescript-language-server`, `pyright-langserver`, `gopls` and `dart` on `PATH` (each language is skipped without its server): [below](#the-language-server-tools). The `image` job of `.github/workflows/coder.yml` runs it in the built coder image |
| OS limits on a command (ADR 0065 slice 3) | `ADAM_TEST_REQUIRE_SYSTEMD=1` to fail instead of skip the `scope` cases | a systemd user manager that delegates memory, cpu and pids (`XDG_RUNTIME_DIR` set): `cargo test -p adam-confine -- --test-threads 1`. It runs a memory hog, a process flood and a CPU spinner, and CI's `confine` job runs it inside a capped scope (`systemd-run --user --scope -p MemoryMax=4G -p TasksMax=4096`) so a limit that fails cannot take the machine; run it that way yourself. Without a user manager the `Rlimits` cases still run |
| `#[tool]` compile errors worded by rustc | `ADAM_TRYBUILD=1` | the CI toolchain (the `ui` job); the errors the macro writes itself always run ([`adam` README](../../crates/adam/README.md#tests)) |
| live model; `adam-acp` against a live OpenCode (the coder no longer uses it) | `ADAM_TEST_OPENAI_*`, `ADAM_TEST_OPENCODE=1` | secrets |

All test variables: [Environment](../reference/environment.md#tests).

## A process that is spoken to on stdin

A language server is a long-running process that is written to on stdin and answers on stdout with `Content-Length`
frames. `adam_workspace::testing_stdio` asks any environment whether it can host one, with `cat` as the server: a first
message read back, **silence with stdin held open**, a 64 KiB message read back (more than a pipe buffer), then **EOF**
(the process ends by itself with status 0) or a **kill** (the process group and the environment's `kill` by exec id; no
process carrying the run's marker is left). Before the end, the same probe must **see** the live process by its marker (step
`probe`), or its silence after a kill would prove nothing. A failure names the step. `ADAM_TEST_STDIO_SILENCE_SECS` sets the silence
(20 s by default; the kill case always uses 1 s).

| Environment | Test | Run it | CI |
|---|---|---|---|
| Local | `adam-workspace/tests/stdio_framing.rs` | `cargo test -p adam-workspace --test stdio_framing` (always runs, about 20 s) | the normal test job |
| Run pod | the last scenario of `adam-env-kubernetes/tests/cluster.rs`, through `adam-kube-exec` | `sh deploy/coder/tests/kind-run-pods.sh` (`ADAM_TEST_STDIO_SILENCE_SECS` to change its 90 s) | `run-pods` job |

A step that fails in the pod is a finding about that environment, not a flaky test.

## The language server tools

`bin/adam-coder/tests/language_servers.rs` runs the three tools over a **scripted** language server, `adam-lsp-script`
(`bin/adam-coder/src/bin/adam-lsp-script.rs`): a second binary of the crate that tests start through `CARGO_BIN_EXE_adam-lsp-script`
and that **no image builds** (`docker/coder/Dockerfile` builds the named binaries only). It speaks the protocol and does what a JSON script
(`ADAM_LSP_SCRIPT`) says: diagnostics pushed or pulled, an indexing that lasts so long, a `workspace/applyEdit` request, a crash on the
n-th request with a stderr text, framing that is not framing, a `Content-Length` of 99,999,999, a request it never answers, a file it
writes into the workspace, 100,000 references, the text it holds for a file (so a `didChange` is provable), a child and a grandchild,
another spelling of a file's address, garbage on stdout after the start, an error code for a method, a stall before it reads again (a server
whose input pipe fills); the table of keys is its module doc. It logs every message it receives, so a test reads what the client sent.

The tests are real processes, and **each stop point is proved by pid** (idle, park, the end of the run, a cancelled run reached by the
sweep, `use_repository_environment`, the worker's stop, a registry that is dropped, a server that did not answer `initialize`, a start whose call is
dropped, a start that ends after its run was stopped, a server whose output stopped being messages): nothing of the server, its
child and its grandchild is left. The registry is also driven directly (`Servers::get_or_start` with a recording `EnvSession`) for what a
call cannot show: the bound under two concurrent starts, a server that is being stopped still counting, a call dropped while it waits for a server. Run them with the whole suite or by name:

```sh
cargo test --workspace --locked --features adam-llm-agent/schema,adam-assembly/dev,adam/dev,adam-assembly/mcp,adam/mcp \
  --exclude adam-store-mongodb -- no_server_on_path a_server_that diagnostics_are the_columns
```

The real servers are a gated suite (`real_language_servers.rs`, `ADAM_TEST_LANGUAGE_SERVERS=1`): five tiny projects under
`bin/adam-coder/tests/fixtures/language-servers/` (Rust, TypeScript, Python, Go, Dart), each with one deliberate error, driven through the
tools with the production table: the error is reported at its line, the definition of `add` is found from its call, and its references hold
the call. A language whose server is not on `PATH` is skipped; `ADAM_TEST_REQUIRE_LANGUAGE_SERVERS=1` makes that a failure. **The `image`
job of `.github/workflows/coder.yml` runs it in the built coder image**, whose workspace base has the four servers
(vymalo/another-agentic-images#15), with `ADAM_TEST_REQUIRE_LANGUAGE_SERVERS=1`. Run on 2026-11-17 on a machine with `rust-analyzer`
1.97.0 and `pyright-langserver`, both passed; the other two were not installed.

::: in-depth What the store suite covers, and adding a backend

## What the store suite covers

49 cases: exact JSON round trip (unicode, `i64` bounds, floats, special keys), CAS conflicts, 16-way concurrent
commits with one winner, journal ordering, first-writer-wins and 16-way races, non-determinism detection, due
rules, agent filtering and limits, busy runs left unclaimed, 8 workers claiming 60 runs with no double lease,
lease expiry and takeover, renew and release, pinned claims (an owned run never goes to another worker, 4 workers
racing), one open run per conversation including a 16-way race, listing and counting (scoped, ordered, keyset
pages, filters), push configurations, and purging.

The worker-lease suite (`worker_leases_conformance!`) adds three cases for a store that implements
`release_worker_leases`: every run lease the worker holds goes and its runs are claimable at once,
and its owner is not cleared; another worker's lease is untouched; and a lease the owner does not
hold survives the owner's sweep. The method is defaulted to a no-op, so the
suite is separate from the required `store_conformance!`.

The write cost of a transcript-shaped run is measured **and asserted**: `counting::measure_transcript_write`
commits a 10 KB message 50 times through `CountingStore` in the contract shape (the tombstone, a head and the
body delta); `transcript_measurement!` emits the `measure_the_transcript_write` test in each adapter's test
file, which prints the bytes and asserts the S0 bound (under `50 × 20 KB` in all, under `20 KB` per commit),
and `dev/p2-measure.sh` quotes the disk side. The before/after numbers are in
[ADR 0055](../decisions/0055-a-runs-state-is-a-head-and-an-append-only-transcript.md).

## Adding a backend

Implement `Store` and add one line: `adam_store_testkit::store_conformance!(make_store);` (and
`adam_store_testkit::blob_conformance!(make_store);` when it keeps blobs,
`adam_store_testkit::body_conformance!(make_store);` when it keeps bodies, and
`adam_store_testkit::worker_leases_conformance!(make_store);` when it implements
`release_worker_leases`, which defaults to a no-op). A new `Notifier` runs
`adam_notify_testkit::notifier_conformance!(make_pair);` (the Postgres one needs `ADAM_TEST_POSTGRES_URL`, a
superuser, for its reconnect test). See [Store adapters](../reference/store-adapters.md#adding-a-backend).

:::

## End-to-end scenarios

Scripts in `dev/` drive the compose stack through A2A and assert the whole chain on the mocks. Each script's
header lists its steps and variables. CI runs them in `.github/workflows/coder.yml` ("Compose e2e") on the image
it has just built: a pull request runs the seven default coder scenarios and two App-mode ones (all 19 when it
carries the `e2e-full` label or changes `dev/`, `docker/coder/` or `compose.yaml`), main and a manual `workflow_dispatch` run all of
them. Start the stack first:

```sh
docker compose --profile app up -d --build --wait postgres mock-openai mock-github mock-github-mcp git-server coder
```

| Script | What it proves |
|---|---|
| `sh dev/coder-e2e.sh` | one task ends in a pushed branch and one pull request; the `checks`, `branch` and `pull_request` artifacts; GitHub read over MCP with the coder's credentials of each call. Variants: `SCENARIO=files` (the coder reads a file first), `scratch` (no repository named, then published), `second-repo` and `create-repo` (each with `ANSWER=yes` or `no`: consent is asked and honoured), `GITHUB_AUTH=app` (with `-f dev/compose.github-app.yaml`) |
| `sh dev/greeting-e2e.sh` | "hi" gets a greeting built from the folder's persona lines, streamed, and its one model call reported over `usage/v1` (the task carries the totals); the same task goes on to a pull request; a restart on an edited folder changes the answer |
| `sh dev/coder-choices-e2e.sh` | three questions asked as one A2UI form, one action answers them; a screen that cannot read it gets the options as text (the screen's capabilities are `a2uiRendererCapabilities["1.0"]` and its catalog is catalog 9, a `ui-catalog/v2` document) |
| `sh dev/agent-e2e.sh` | the general agent answers "hi" in role; a restart on an edited folder changes the card and the answer |
| `sh dev/agent-cards-e2e.sh` | the researcher folder answers with a surface of cards and a graph on a screen that has them, words only on one that has not (catalog 9, and the checklist subset, which has no `Cards`) |
| `sh dev/painter-e2e.sh` | the painter folder: the instructions of its image MCP server reach the model, the picture is a shared `image/png` file the words name, and the server's prompt is the command `images__scene` (it starts `mock-image-mcp` and restarts `agent` on a copy of the folder with `AGENT_MCP_ALLOW_INSECURE=true`) |

How the scripted models and mocks work, and how to change a script: [the local stack](../reference/dev-stack.md).
Exit and assertion details are in each script's header.

## Other checks

| Check | Command |
|---|---|
| chart render guarantees | `sh deploy/coder/tests/render-check.sh` (also kubeconform and golden renders in CI), `sh deploy/coder/tests/bump-tag-test.sh` |
| image smoke tests | `sh docker/coder/test/container-smoke.sh <image>`, `sh docker/coder/test/agent-smoke.sh <image>`; with `EXPECT_REVISION=<commit>` (the image's `ADAM_BUILD_REVISION`) they also check the card's version and `build/v1`. Both run `docker/coder/test/http-smoke.sh`, which also checks the card's two interfaces, that `/docs` and `/openapi.json` are public and that the HTTP+JSON binding needs the token |
| shell scripts | `shellcheck deploy/coder/bump-tag.sh deploy/coder/tests/*.sh docker/coder/test/*.sh dev/*.sh` |
| the transcript write measurement | `cargo test -p adam-store-testkit -p adam-store-postgres -p adam-store-mongodb measure_the_transcript_write -- --exact --nocapture` prints one `p2-measure:` line per adapter and asserts the S0 bound (under `50 × 20 KB` in all, under `20 KB` per commit); the before/after numbers are in [ADR 0055](../decisions/0055-a-runs-state-is-a-head-and-an-append-only-transcript.md) |
| what a transcript-shaped run costs | `ADAM_TEST_POSTGRES_URL=... ADAM_TEST_MONGODB_URI=... sh dev/p2-measure.sh` runs the 50-commit measurement against the stack and prints `pg_relation_size`/`pg_total_relation_size` of `runs` and `body`, the `pg_stat_wal` delta, `collStats` storage sizes and the WiredTiger block-manager and log deltas; a store whose variable is unset is skipped, so it is a no-op without the databases. Use a fresh database for absolute sizes |
| a model mock's streamed twin | `cargo test -p adam-model-openai --test wiremock_compose` (plays every script both ways) |

Two worktrees can run the stack at once: each gets its own compose project, but the host ports are shared, so override them for the second ([Two stacks side by side](run-locally.md#two-stacks-side-by-side)).
