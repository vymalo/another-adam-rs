# 0089. The coder knows six more languages: the files every repository has, and C and C++

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
