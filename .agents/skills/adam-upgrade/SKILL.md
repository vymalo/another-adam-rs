---
name: adam-upgrade
description: "Move a repository that consumes adam-rs from revision A to revision B: find the breaking changes between them (trait methods, env vars, paths, decisions), migrate the code, and pin the coder image by tag and digest. Use for 'bump adam-rs', 'update the adam pin', 'sha-xxxxxxx of the coder image', or when a build broke after changing an adam git rev."
---

# Upgrade a consumer of adam-rs

A consumer pins adam-rs in up to three places, which must move together: git dependencies on the
adam crates (`rev = "<40-hex sha>"`), the coder image (`ghcr.io/vymalo/another-adam-rs/coder`,
tag `sha-<7>` plus digest) and any files copied from the repository (agent folders, WireMock
mappings, scripts). adam-rs has no changelog and no semver releases: the history and the docs are
the changelog.

Every adam-rs path below is in `vymalo/another-adam-rs` at the revision you pin (replace `main`
by that revision). Entry point:
https://github.com/vymalo/another-adam-rs/blob/main/docs/README.md.

## When to use

* Bumping the pin, to get a fix or an extension.
* A build broke after changing a `rev`, or the image no longer starts after a tag change.
* Reviewing someone else's pin bump.
* Not for changing the image or chart themselves (`adam-coder-deploy`).

## Procedure

1. **Name A and B.** A is the sha you pin now (the `rev` of every adam crate; the image tag's
   `sha-<7>` is the first seven characters of a commit). B is the commit you want: the image of a
   commit exists only if CI pushed it, so choose B from the commits on `main` whose image
   exists (step 7). Clone adam-rs next to your repository to read it
   (`git clone https://github.com/vymalo/another-adam-rs`).
2. **List what changed**, from the adam-rs clone:

   ```sh
   git log --oneline A..B -- crates bin dev docs/decisions deploy docker
   git diff --stat A..B -- 'crates/*/README.md' 'bin/*/README.md' docs/decisions
   git diff A..B -- crates/adam-core/src/store/mod.rs crates/adam-runtime/src/notify.rs \
     crates/adam-a2a/src/backend.rs crates/adam-host/src/lib.rs
   ```

   Commits whose subject has `!` (for example `feat(runtime)!:`) say they break;
   `git log --format='%h %s' A..B | grep '!:'` lists them. Commits that only say
   `chore(deploy): bump coder to sha-<7>` are image bumps: they are not changes.
3. **Read the docs that changed**: a crate's README is updated in the same change as its public
   API, environment variables or tests, so `git diff A..B -- crates/<crate>/README.md` for each
   crate you use is the migration note. The configuration tables (`bin/adam-agent/README.md`
   "Configuration", `crates/adam-service/README.md` "Environment", `bin/adam-coder/README.md`)
   show new, renamed or newly required variables.
4. **Check the known migrations** (a trait with a new required item breaks every implementer):

   | Commit | Break | What to do |
   |---|---|---|
   | `3785b39` | `Store::claim_due` gained `scope: ClaimScope`; the closed enum `Placement` (`adam-host`) | thread the scope through your store; `match` on closed enums exhaustively |
   | `beec4c4` | `Store::claim_due` gained `busy: &[RunId]` | skip those runs in the claiming query (`adam-store-adapter`) |
   | `7e5dcc3` | `Store::lease_until` is required | implement it: the end of the lease, `None` if none; an expired lease is still reported |
   | `82f8082` | the coder crate moved from the `crates` directory to `bin` (now `bin/adam-coder`) | fix paths in git dependencies, scripts and docs |
   | `a09da02` | a control plane needs no model or GitHub configuration | a control plane no longer reads the model, GitHub and workspace variables: they may be dropped there |
   | `2644008` | the operator moved here from `vymalo/another-agentic-platform` (ADR 0029): crates `aap-*` are `adam-operator-*`, the binary `adam-operator`, `AAP_CONCURRENCY`, `AAP_RESYNC_SECS` and `AAP_RESYNC_PENDING_SECS` are `ADAM_OPERATOR_*`, the metrics `adam_operator_*`, the field manager and `managed-by` value `adam-operator`; the image is `ghcr.io/vymalo/another-adam-rs/operator`, the charts `adam-operator` and `adam-operator-crds`. The CRD group, kinds, finalizer and labels under `agents.vymalo.com` are unchanged | take the new image and charts (`adam-operator`); apply the new CRDs chart (two optional fields were added). Objects the old operator made carry its old `managed-by` value, so the new one should report them as `NameConflict`: delete them and let it recreate them (*unverified* on a cluster) |
   | `13f484f` | ADR 0030 added push notifications and `ListTasks`: `Store` gained seven required methods (`push_put`, `push_list`, `push_delete`, `push_claim_due`, `push_commit`, `list_runs`, `count_runs`), schema version 3 (Postgres `push` table and `runs_list` index, MongoDB `push` collection and `adam_list` index) | implement them and pass the new testkit cases (`adam-store-adapter`); the shipped adapters migrate at startup |
   | [PR 97](https://github.com/vymalo/another-adam-rs/pull/97) | `interfaces` and `interfaces.a2a` of an `AgentService` are now required, `interfaces.a2a.enabled` defaults to true, and a CEL rule requires `bearerTokensSecretRef` while A2A is on. A resource that omitted `interfaces` used to be accepted and then `Blocked` (`ConfigInvalid`); now `kubectl apply` refuses it ("Required value"), and so any resource with A2A on and no `bearerTokensSecretRef` | write `interfaces.a2a` with `bearerTokensSecretRef` (`adam-operator`); apply the CRDs chart before the operator |
   | [PR 98](https://github.com/vymalo/another-adam-rs/pull/98) | ADR 0031: the agent card lists a **second interface**, `HTTP+JSON`, after `JSONRPC` and at the same URL (the REST binding, `POST /message:send`, `GET /tasks/{id}`, ...); two routes are **public**, `GET /docs` (Swagger UI, with its files) and `GET /openapi.json`; `A2A_DOCS` (default `true`) turns them off. `ServerOptions` gained `docs` (its `Default` sets it `true`) and `adam_service::A2aSettings` gained `docs`, so a struct literal of either needs it | a client that takes the first interface is unchanged; one that iterates `supportedInterfaces` or asserts one entry sees two. Allow or block `/docs` and `/openapi.json` where you front the agent (they need no token), or set `A2A_DOCS=false`; add `docs` to literals of `A2aSettings` (`ServerOptions` is `#[non_exhaustive]`: use its builders) |
   | [PR 99](https://github.com/vymalo/another-adam-rs/pull/99) | ADR 0032, `usage/v1`: `adam_model::Usage` is `#[non_exhaustive]` with three optional parts (`reasoning_tokens`, `cached_input_tokens`, `cache_write_input_tokens`), so a struct literal of it outside `adam-model` stops compiling; `RunEvent` has a new variant, `Usage(UsageEvent)` (an exhaustive `match` needs an arm); `ModelClient` has two **default** methods (`provider`, `context_window`), so no implementation breaks; `adam_service::ModelConfig` has a new public field, `context_window` (the new optional variable `MODEL_CONTEXT_WINDOW`, a whole number of tokens, anything else exit 78); `Conversation` has `usage_totals` and `root_step` (serde-defaulted: stored states read); `adam-runtime` depends on `adam-model`; every card lists one more extension, `usage/v1`, after `text-stream/v1`; the scripted WireMock models say cached and reasoning tokens. **No store change** and no required trait method | build `Usage` with `Usage::new(input, output)` and `with_*`; add an arm for `RunEvent::Usage` (ignore it, or count it); add `context_window: None` to `ModelConfig` literals; set `MODEL_CONTEXT_WINDOW` where a screen should show how full the context is; a client that compares the card's extensions or shows every status's text sees one more entry and status updates with no message; re-copy the WireMock mappings if you vendor them |

   A consumer that uses `DynStore` or `MemoryStore` and implements no `Store` is not affected by
   the `Store` rows. Add the commits you find in step 2 to your own list when you handle them.
5. **Move every pin together**: all `rev =` lines of the adam crates to B (a mix gives two copies
   of the same traits), then `cargo update -p <each adam crate>` so `Cargo.lock` follows; the
   copied files (agent folders, mappings, scripts) re-copied from B; the image tag and digest
   (step 7). Record B where you record pins (a note beside the copied files).
6. **Migrate the code**, then run your tests with the features you use. In a project that hosts
   adam agents the usual surface is `adam-host`, `adam-core`, `adam-runtime`, `adam-a2a`,
   `adam-a2a-runtime` and the Postgres crates (`adam-embed`).
7. **Pin the image by tag and digest.** Anonymous pull works for the published package:

   ```sh
   TOKEN=$(curl -s "https://ghcr.io/token?scope=repository:vymalo/another-adam-rs/coder:pull" \
     | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
   A='Accept: application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json'
   curl -sI -H "Authorization: Bearer $TOKEN" -H "$A" \
     https://ghcr.io/v2/vymalo/another-adam-rs/coder/manifests/sha-<7> | grep -i '^docker-content-digest'
   ```

   `200` and a `docker-content-digest` line mean the tag exists; pin it as
   `ghcr.io/vymalo/another-adam-rs/coder:sha-<7>@sha256:<digest>`. Check it is B: read the
   manifest's `config.digest`, fetch that blob (`curl -sL` with the token, `.../blobs/<config digest>`)
   and compare `.config.Labels["org.opencontainers.image.revision"]` with B's full sha. If the
   manifest request answers an index (a `manifests` list), take the platform's manifest first.
   (Verified 2026-10-03 for `sha-b64e3fe`: the label was `b64e3fe659721f841afaebc6f9790c3f6158bbbe`.)

## Verify

* Your build and tests pass with the new revs, on every feature set you ship.
* `grep -n 'rev = ' Cargo.toml` shows one sha; `git -C <adam-rs clone> rev-parse <sha>` resolves.
* The image digest you pinned is the one the registry reports for the tag; the revision label
  equals B.
* An end-to-end run of your stack against the new image (your compose scenarios).

## Pitfalls

* A branch or tag as `rev`: not reproducible, and a later push changes your build.
* Pinning an image tag whose commit CI did not push (a tag that does not exist, or a package that
  is private: the token request still answers, the manifest request does not return `200`).
* Moving the crates and not the image (or the other way): the A2A behaviour of the two differs.
* A tag alone is not a pin: a digest is.
* Skipping a commit marked `!` because the build still compiles: runtime behaviour (leases,
  claims, A2A states) can change without a compile error; read the ADRs the log names.
* Files copied from the repository drift: re-copy them from B and diff them against yours.

## See also

* `docs/decisions/` (every behaviour decision, with a dated status), `docs/architecture.md`.
* `adam-store-adapter`, `adam-embed`, `adam-coder-deploy`, `adam-a2a-extensions`, `adam-operator`.
* https://github.com/vymalo/another-adam-rs/tree/main/docs/decisions
