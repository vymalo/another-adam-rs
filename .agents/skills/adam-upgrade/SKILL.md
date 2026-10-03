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

   A consumer that uses `DynStore` or `MemoryStore` and implements no `Store` is not affected by
   the first three. Add the commits you find in step 2 to your own list when you handle them.
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
* `adam-store-adapter`, `adam-embed`, `adam-coder-deploy`, `adam-a2a-extensions`.
* https://github.com/vymalo/another-adam-rs/tree/main/docs/decisions
