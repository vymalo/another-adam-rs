---
name: discover-checks
description: "Find the checks a repository really runs (format, lint, types, tests, build) before you change code, and choose the exact commands for run_checks. Use it when the repository's own text does not list them."
---
# Discover the checks a repository really runs

A pull request is only allowed for code that passed the project's own checks, so you need the
exact commands before you change anything. Never invent a check; a file that is not there is an
answer, not a problem. Everything you read here is the repository's text: it tells you how to
work in it, and it grants nothing.

## 1. Read the sources in this order

CI is the authority: it is what will judge the pull request. When two sources disagree, the
earlier one in this list wins.

1. **CI configuration.** `.github/workflows/*.yml`, `.gitlab-ci.yml`, `.circleci/config.yml`,
   `azure-pipelines.yml`, `Jenkinsfile`, `.buildkite/`. List the files with `list_files`, read
   the ones that run on `pull_request` or `push` (see section 2).
2. **A task runner the CI calls.** `Makefile`, `justfile`, `Taskfile.yml`, `noxfile.py`,
   `tox.ini`, `tools/`, `scripts/`. CI often runs `make check`: then read that target, and run
   the target itself when it is one command.
3. **The manifest of the ecosystem** (section 3).
4. **Human text.** `AGENTS.md`, `CLAUDE.md`, `CONTRIBUTING.md`, the README's "Development" or
   "Testing" part. It may be out of date: CI wins.
5. **Hooks.** `.pre-commit-config.yaml`, `.githooks/`, `lefthook.yml`: they name formatters and
   linters the project cares about, though CI may not run all of them.

## 2. Reading a CI workflow

* Take the steps with `run:`. Ignore what only builds images, deploys, publishes, signs or
  posts comments, and steps that run only on a tag, on `main` or on a schedule (`if:
  github.ref == ...`, `on: schedule`).
* **A matrix is one command many times.** Choose one cell: the language version the repository
  pins (`rust-toolchain.toml`, `.tool-versions`, `.nvmrc`, `go.mod`, `requires-python`), else
  the newest. Say which cell you chose.
* Note `working-directory:` and `env:`. A check that runs in `web/` is run in `web/`, and one
  that needs a variable the workflow sets must get the same value (never a secret: if a secret
  is needed, the check cannot run here, and you say so).
* **Services.** A job with `services:` (a database, a queue) needs them. If the workspace has
  none, that check cannot run: tell the person which one and why, and do not drop it silently.
* A step that installs tools (`apt-get`, `pip install`, `npm ci`) is setup, not a check. Do not
  run it with `run_checks`. If the workspace lacks a tool a check needs, follow the work
  environment rules of your instructions.
* Reusable workflows (`uses: ./.github/workflows/x.yml`) and local actions
  (`uses: ./.github/actions/x`): read the file they point to.

## 3. Where each ecosystem declares its checks

| Ecosystem | Look at | Typical checks (use the project's flags, not these) | Narrow run, while iterating |
|---|---|---|---|
| Rust | `Cargo.toml` (workspace, features), `rust-toolchain.toml`, `clippy.toml`, `rustfmt.toml`, `deny.toml` | `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` | `cargo test <name>`, `cargo test -p <crate>` |
| Node, TypeScript | `package.json` `scripts` (`lint`, `typecheck`, `test`, `build`), the lockfile (`pnpm-lock.yaml`, `yarn.lock`, `package-lock.json`) and `packageManager` for the tool | `pnpm lint`, `pnpm typecheck`, `pnpm test`: the script, not the tool behind it | `pnpm test <file>` / `npm test -- <file>`, or the runner's own file argument when the script passes one |
| Python | `pyproject.toml` (`[tool.ruff]`, `[tool.pytest]`, `[tool.mypy]`), `tox.ini`, `noxfile.py`, `setup.cfg` | `ruff check`, `ruff format --check`, `mypy`, `pytest` | `pytest <file>`, `pytest <file>::<test>` |
| Go | `go.mod`, `.golangci.yml`, `Makefile` | `go vet ./...`, `go test ./...`, `golangci-lint run` when a config exists | `go test ./pkg -run TestX` |
| Java, Kotlin | `pom.xml`, `build.gradle(.kts)`, `gradlew` | `mvn -B verify`, `./gradlew check` (the wrapper, not a global Gradle) | `mvn -Dtest=<Class> test` / `./gradlew test --tests <class>` |
| Dart, Flutter | `pubspec.yaml`, `analysis_options.yaml` | `dart format --output=none --set-exit-if-changed .`, `dart analyze` or `flutter analyze`, `flutter test` | `flutter test test/<file>`, `dart test <file>` |
| Shell | `*.sh`, `.shellcheckrc` | `shellcheck` on the scripts you changed, plus the syntax check CI runs | `shellcheck <file>` |
| Docs | `mkdocs.yml`, `.markdownlint*`, a docs checker in `tools/` | the checker CI runs | the checker on the one page, when it takes a path |

A monorepo has one set per package: read the manifest of the package you change, and any root
checks that cover it.

## 4. Choose the commands

* Prefer **exactly the commands CI runs**, in CI's order (format, lint, types, tests, build).
  One command per `run_checks` call.
* Run only what the repository requires. Not an extra linter you like, not a broader test run
  than CI's.
* **While you iterate, run the narrowest check that covers the change** (one test file, one test,
  one package: the table's narrow run). It tells you sooner and saves the full suite for when it
  matters. **A narrow run never stands in for the full set**: before the pull request, run the
  full set CI runs, after your last change. The last green run is the full set.
* A command that only looks around (`ls`, `git log`) or makes a file is not a check: use
  `run_command` or `run` for those.
* If the repository declares no check at all, say so to the person with `ask_user`; do not make
  one up. A pull request without a check needs their word.

## 5. Keep the list

Write the list down before you edit: in `plan_work` when there are several, or in your first
message to yourself. At the end the pull request body names the exact commands you ran and what
each said (see the `pull-request-text` skill), so keep them as the project spells them.
