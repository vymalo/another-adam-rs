# 0028. The card says which build answers

Status: **Accepted** (2026-10-07)
Builds on [ADR 0004](0004-agent-folders-at-run-time.md) (agent folders at run time: the files can change without a build)
and [ADR 0006](0006-a2ui-and-the-vymalo-extensions-in-adam-rs.md) (the vymalo extensions are declared on the card and
detected from it).

## Context

The owner exported production threads of Adam and shared them. Nothing in an export says which adam build answered:
the card's `version` was `CARGO_PKG_VERSION`, the same string for every commit of the workspace, and the agent's files
(the prompt, the limits, the skills, the subagents) can be a folder mounted over the embedded copy (ADR 0004), so even
the build does not say what the agent was told. A report of a wrong behaviour could not be tied to a commit or to a prompt.
The image is tagged `sha-<7>` by CI, but a tag is not on the card, and a thread export is made from the card and the
stream.

## Decision

1. **The build's revision is baked in at compile time.** Both binaries read `option_env!("ADAM_BUILD_REVISION")`
   (`adam_coder::BUILD_REVISION`, `adam_agent::BUILD_REVISION`). `docker/coder/Dockerfile` has `ARG ADAM_BUILD_REVISION`
   before the `cargo build` and `.github/workflows/coder.yml` passes `github.sha`; `compose.yaml` passes
   `${ADAM_BUILD_REVISION:-}` to its two `build:` sections. A build without one says `unknown`; nothing fails.
2. **The card's `version` carries it as semver build metadata**: `<CARGO_PKG_VERSION>+<first 7 characters>`, for example
   `0.1.0+6478fbc`, `0.1.0+unknown` for a local build (`adam_a2a::build_version`). Semver 2.0.0 says build metadata
   does not take part in precedence, so a client that compares versions is unaffected. Only letters, digits and `-`
   are kept (`revision_of`), so a hostile or careless build argument cannot put anything else on a card.
3. **A new optional extension carries the whole picture**: `https://agents.vymalo.com/a2a/extensions/build/v1`
   (`BUILD_EXTENSION`), with `params` `{"revision": <the revision, whole, or "unknown">, "folderDigest":
   "sha256:..."}`. `folderDigest` is the digest `adam-agent-fs` computes over the manifest and the files the skills bundle
   (`AgentFiles::describe().digest` for the coder, `AgentFolder::digest` for `adam-agent`): the same digest the
   `agent files` line of the startup log shows, the same whether the files were embedded or read from a folder.
   The extension is **optional and informational**: `required` is `false`, no request header activates it and no message
   metadata carries it, so a client that does not know it ignores it, and it is removable without breaking plain A2A, as
   every adam extension is. A client detects it as it detects the others: by reading the card's
   `capabilities.extensions` for the URI.
4. **Why a new extension and not `params` of an existing one.** `steps/v1`, `text-stream/v1` and the others are
   contracts written in the orchestration layer's repository, each about one behaviour; a parameter of a behaviour that has
   nothing to do with it would make a client that reads `steps/v1` params learn about builds, and a contract change
   in another repository for each. A separate URI is a separate, additive contract. The URI follows the others
   (`.../a2a/extensions/<name>/v1`); the shorter `.../a2a/build/v1` was considered and not used, so that all adam
   URIs have one shape. The version string alone was not enough: it cannot say the folder.
5. **The contract is written here first** (`docs/reference/a2a-server.md`, "Which build answers"); the orchestration layer's
   `docs/api/` page is to follow, and until it exists the extension is *unverified* there.
6. **The image's smoke test checks it.** `docker/coder/test/http-smoke.sh` takes `EXPECT_REVISION` (CI sets it to the
   commit): the card's version must end with `+<first 7>` and `build/v1` must say the revision whole; without it the
   version must still carry build metadata, and `folderDigest` must be a `sha256:` digest either way.

## Consequences

* No new required trait method. `adam_a2a` gains `BUILD_EXTENSION`, `UNKNOWN_REVISION`, `build_version`, `revision_of`
  and `ExtensionConfig::build`. `adam_agent::card_of` keeps its signature and its version now has the metadata;
  `adam_agent::card_of_folder` adds the extension (a definition does not know the digest of the folder it came from).
  `adam_coder::agent_card_from` adds it itself.
* Any test or client that compared the card's version with `CARGO_PKG_VERSION` must compare with
  `build_version()` (`bin/adam-agent/tests`, `bin/adam-coder` golden).
* `option_env!` is tracked by cargo: changing the variable rebuilds the two crates, so CI's cached `target` is not stale.
  The `ARG` sits after the `COPY`s of the sources so it invalidates nothing before the compile.
* A folder whose files were merged with `ADAM_EXTRA_MCP_FILE` still reports the folder's own digest: the extra file is the
  deployment's, not the agent's.
* A thread export can now say `0.1.0+6478fbc`, folder `sha256:...`, and the person reading it can check out the commit and
  compare the digest with the one `adam-coder` logs.

## Alternatives rejected

* **The image tag in the card.** The binary does not know its tag; the tag is made after the build.
* **A `/version` route.** Another surface to authenticate and document, and not in the card a client already reads.
* **The revision in `description`.** Free text that a prompt change can overwrite.
