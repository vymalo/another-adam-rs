# 0091. The coder knows six more languages: the files every repository has, and C and C++

Status: **Accepted** (2026-11-28). The owner asked on 2026-11-27: *"We need more language servers."* The owner then
said *"Take decisions yourself"* (2026-11-28), so the batch, its order and decision 4 are this ADR's choices for the
owner to confirm. Builds on [ADR 0044](0044-paged-tools-a-stable-prompt-prefix-a-plan-skills-commands-and-project-context.md)
slice 17 (diagnostics from a language server).

## Context

The coder's three language-server tools (`file_diagnostics`, `find_definition`, `find_references`) know five
languages, in a closed table that is code, never a setting (`bin/adam-coder/src/tools/lsp/table.rs`, *verified
2026-11-28*):

| Language | Server | Files |
|---|---|---|
| Rust | `rust-analyzer` | `.rs` |
| TypeScript, JavaScript | `typescript-language-server` | `.ts .tsx .mts .cts .js .jsx .mjs .cjs` |
| Python | `pyright-langserver` | `.py .pyi` |
| Go | `gopls` | `.go` |
| Dart | `dart language-server` | `.dart` |

The servers are installed by the workspace image of vymalo/another-agentic-images (its #15 added the first four,
`docker/coder/Dockerfile`). A file is matched by its extension only. A project root is the nearest directory with one
of the language's marker files, else the slot's root. Before a call the coder checks the program is installed
(`command -v`); a missing one is answered as a missing toolchain, not an error of the run.

What a coding task touches beside its main language is mostly the same few file kinds: the manifest (`Cargo.toml`,
`pyproject.toml`), CI and deployment files (YAML), JSON, shell scripts and the Dockerfile. A mistake in one of them is
caught today only when a command fails. The owner's first stack is Rust with CrateStack, where `Cargo.toml` and the CI
YAML are part of almost every change.

## Decision

1. **Batch 1 adds six languages to the closed table:**

   | Language | Server (program, launch) | Files | Root markers |
   |---|---|---|---|
   | TOML | `taplo` (`taplo lsp stdio`) | `.toml` | `.taplo.toml`, `taplo.toml` |
   | YAML | `yaml-language-server` (`--stdio`) | `.yaml .yml` | none: the slot's root |
   | JSON | `vscode-json-language-server` (`--stdio`) | `.json .jsonc` | none |
   | Shell | `bash-language-server` (`start`) | `.sh .bash` | none |
   | Dockerfile | `docker-langserver` (`--stdio`) | a file named `Dockerfile` or `Containerfile`, or `*.Dockerfile` | none |
   | C, C++ | `clangd` | `.c .h .cc .cpp .cxx .hpp .hh` | `compile_commands.json`, `compile_flags.txt`, `.clangd` |

   The program names and flags are *unverified* (written from each project's documentation as known on 2026-11-28);
   the gated real-server suite (`ADAM_TEST_LANGUAGE_SERVERS=1`) is how they are checked, as for the first five.
2. **A file is matched by its name too**, not only its extension, so `Dockerfile` has a language. The match stays
   closed: a name is in the table or it has no language.
3. **The modes stay as they are.** None of the six runs project code to analyse a file: a safe server is the same
   program with no extra setting, and the slot is still compared before and after each call. The YAML and JSON servers
   are told **not to fetch schemas from the network** (`schemaStore.enable: false`, no `$schema` download), so a call
   never reaches a host the deployment did not allow. `clangd` is told not to index in the background beyond the
   project (`--background-index=false` in a safe server).
4. **The image change is the images repository's.** The coder learns the six languages now; until the workspace image
   installs the programs, a call on such a file says the server is not installed, which is the behaviour for any
   missing server. The follow-up is an issue in vymalo/another-agentic-images: the four Node servers through `npm
   install -g` (Node is already in the image), `taplo` as a static binary, `clangd` from Debian. `docker/coder/Dockerfile`
   moves its `WORKSPACE_TAG` when that image is published.
5. **Batch 2 is named, not built:** Java (`jdtls`), Kotlin, C# and Ruby each bring a runtime (a JVM, .NET, Ruby) and
   hundreds of megabytes to the image. They wait for a measured need.

## Slices

| Slice | What |
|---|---|
| L1 | The six `Language` variants, matching by name, markers, launch lines and settings (no network schemas); the scripted-server tests for each; `docs/reference/coder-agent.md`. |
| L2 | The images repository installs the programs; `WORKSPACE_TAG` moves; the gated real-server suite runs on the six. |
| L3 | Batch 2, when measured. |

## Consequences

* A change to `Cargo.toml`, a workflow or a Dockerfile can be checked before a command fails.
* The tool descriptions list more languages, which moves the prompt's stable prefix once.
* Until L2, the six answer "not installed" in the shipped image, as a missing toolchain does.
* No `Store`, `Notifier` or `ModelClient` method changes.

## Amended 2026-11-28: L1 is built

The six variants are in the closed table (`Language::Toml`, `Yaml`, `Json`, `Shell`, `Dockerfile`, `C`), with the programs,
launch lines, files and markers of decision 1, and every `match` on a language names them. The scripted-server tests run
the production launch lines (`tests/language_servers.rs`), and the gated suite has a case for each
(`tests/real_language_servers.rs`), which the servers the image lacks skip, even under `ADAM_TEST_REQUIRE_LANGUAGE_SERVERS=1`,
until L2 adds them to its `IN_THE_IMAGE`. What differs from the decision as written:

* **taplo fetches no schema either.** Its default catalog is the Schema Store's (*verified* 2026-11-28 in the strings of
  @taplo/cli 0.7.0), so decision 3 is applied to it too: it is told `evenBetterToml.schema.enabled: false`. A `Cargo.toml` is
  then checked as TOML, not against a schema.
* **How the JSON server is told.** `vscode-json-language-server` has no `schemaDownload` setting (VS Code's client reads
  `json.schemaDownload.enable`); it is started with `handledSchemaProtocols: ["file"]` and asks the coder for any other
  schema, which answers that it has none (*verified* 2026-11-28 in vscode-langservers-extracted 4.10.0).
* **A YAML file's own `$schema` comment is still fetched** by `yaml-language-server`: no setting turns that off (*verified*
  2026-11-28 in 2.0.0). It is the deployment's network that decides, as the decision says.
* **clangd's background index is off in both modes**, in its launch line (`clangd --background-index=false`), because the
  index is written under the project root, which is the slot (*unverified*). References are then those of the files clangd
  has parsed.
* **The Dockerfile matches no other name**: `Dockerfile.prod` has no language, as the closed match of decision 2 says.

### The follow-up for vymalo/another-agentic-images (L2)

To paste as an issue there (the package names were *verified* on the npm registry on 2026-11-28; the rest is *unverified*):

> **Install the six language servers of adam-rs ADR 0091 in the workspace image.** adam-coder now knows TOML, YAML, JSON,
> shell, Dockerfile and C/C++, and answers "not on PATH" until the image has their programs:
>
> | Program | Install |
> |---|---|
> | `yaml-language-server` | `npm install -g yaml-language-server` (2.0.0 on 2026-11-28) |
> | `vscode-json-language-server` | `npm install -g vscode-langservers-extracted` (4.10.0; it also brings the CSS, HTML, ESLint and Markdown servers) |
> | `bash-language-server` | `npm install -g bash-language-server` (5.8.1), with `shellcheck` from Debian (`apt-get install -y shellcheck`), which it runs for its diagnostics |
> | `docker-langserver` | `npm install -g dockerfile-language-server-nodejs` (0.15.0) |
> | `taplo` | the static binary of a release of tamasfe/taplo (`taplo-linux-x86_64.gz`, `taplo-linux-aarch64.gz`), checked against its published checksum, into `/usr/local/bin`; the npm package `@taplo/cli` is a WebAssembly build |
> | `clangd` | `apt-get install -y clangd` (Debian's) |
>
> Node is already in the image. Check with `command -v` for each, then adam-rs moves `WORKSPACE_TAG` in
> `docker/coder/Dockerfile` and adds the six to `IN_THE_IMAGE` in `bin/adam-coder/tests/real_language_servers.rs`, so the
> `image` job runs them with `ADAM_TEST_REQUIRE_LANGUAGE_SERVERS=1`.
