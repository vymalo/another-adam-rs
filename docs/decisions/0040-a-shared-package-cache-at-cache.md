# 0040. A shared package cache at /cache

Status: **Accepted** (2026-10-11), an owner decision: one shared ReadWriteMany volume across the owner's deployments
now, a pull-through proxy later. Builds on [ADR 0019](0019-a-runs-processes-in-a-pod-of-their-own.md) (run pods),
[ADR 0010](0010-a-run-works-in-its-repositorys-devcontainer.md) (devcontainers) and
[ADR 0029](0029-adam-rs-has-an-operator.md) (the operator), and on the workspace image of `vymalo/another-agentic-images`
that keeps its package caches under `/cache` (its pull request 12).

## Context

A pod starts with an empty home. Every `npm ci`, `pnpm install` and `cargo build` of a run downloads again what the run
before it fetched, and the number of pods grows: workers (`replicaCount`, `workspace.placement`), a pod for each active run
(ADR 0019), and the pods of a worker that a pending decision of the orchestration layer adds. A cold build of a Rust or
Node project is minutes of downloads before any work starts.

Two things are easy to confuse, and this record decides only the first:

* **Package caches**: the downloads of the package managers (the npm cache, the pnpm store, cargo's registry and git
  sources). Content-addressed, the same for every project, safe to share.
* **Installed dependency trees**: `node_modules`, `target/`. Specific to a checkout. Whether a worker installs its own
  (`isolated`) or shares one (`shared`) is the pending decision's `spec.workers.dependencies`; the cache here makes
  `isolated` fast and is **deployment-level and always shared** whichever it is chosen.

*Verified 2026-10-11 in the image's config (an anonymous ghcr.io manifest request for `1.98.1-e6a528f`, digest
`sha256:d2aae762ecad…0797`) and in `toolchains/install.sh` and `workspace/Dockerfile` at `e6a528f` of
`vymalo/another-agentic-images`:* `/cache` is created `1777`, owned by uid and gid 10001; the global npmrc says
`cache=/cache/npm`; pnpm's two config files say `/cache/pnpm-store` (pnpm 10 reads `rc`, pnpm 12 `config.yaml`);
`CARGO_HOME=/cache/cargo`; `CARGO_INSTALL_ROOT=/home/agent/.cargo`, so what `cargo install` and cargo-binstall put in
`bin/` stays private (cargo's `$CARGO_HOME/bin` is first on the PATH of everything cargo starts, so a shared one would let
one run plant a fake `cargo` or `git` for every later run). A volume mounted over `/cache` hides what the image put there,
so only the directory exists in the image and each tool makes its own subdirectory on first use.

## Decision

1. **One volume, mounted at `/cache`.** It is the deployer's: the chart and the operator reference a claim by name
   (`cache.existingClaim`, `environment.cache.existingClaim`), which the deployer makes ReadWriteMany and shares among
   the releases and agents that trust each other. For one release the chart can make the claim itself
   (`cache.create`, ReadWriteMany by default, deleted with the release). Neither set: nothing changes, `/cache` is a
   directory of the container's file system. Its root must be writable by uid and gid 10001: the pods set `fsGroup: 10001`
   (`OnRootMismatch`), which does it where the storage driver supports it, and otherwise the deployer sets it once.
2. **Mounted where toolchains run, and nowhere else.**

   | Pod | Mount |
   |---|---|
   | the coder, `combined`, and the `split` worker | the chart's StatefulSet; the operator's workload of a service that runs workers |
   | a run pod (ADR 0019) | the pod template the chart renders (a ConfigMap, `RUN_POD_TEMPLATE_FILE`); the admission policy lets a run pod mount the work claim and this one, and no other claim |
   | the `split` front, a control plane, the GitHub MCP sidecar | none: they run no toolchain |
   | a repository's devcontainer (Podman, ADR 0010) | `DEVCONTAINER_CACHE_DIR`, a bind of that directory at `/cache` |

   Run pods are made from the deployment's template (ADR 0019), so the coder's code needs no change: the volume is in the
   template. The operator makes no run pods yet (ADR 0029), so it has nothing to pass on; when it does, its template
   carries the same claim.
3. **What is in it**: exactly what the image puts there: `/cache/npm` (npm's `_cacache`), `/cache/pnpm-store`,
   `/cache/cargo` (`registry/`, `git/`, cargo's `.package-cache` lock files and its `.global-cache` database). Not in it:
   installed binaries (`CARGO_INSTALL_ROOT` is private), `node_modules`, `target/`, a checkout, a credential.
4. **No credential is ever written into `/cache`.** It is ensured in four ways, and the residual risk is in the
   consequences:
   * The chart and the operator give a pod **no registry credential**: no `NPM_TOKEN`, no `.npmrc` with a token, no
     `CARGO_REGISTRY_TOKEN`, no `credentials.toml`. The only secrets of a worker are the model key, the GitHub token or
     key and the A2A tokens, and none is read by npm, pnpm or cargo. A chart render with the cache on is checked for the
     names (`deploy/coder/tests/render-check.sh`).
   * Both tools persist a token only when asked to, and not in the cache: npm writes `npm login` to the user's `.npmrc`
     in the home, and a project's `.npmrc` is in the checkout under `/work`; cargo writes `cargo login` to
     `$CARGO_HOME/credentials.toml`, which **is** `/cache/cargo/credentials.toml`. So a run never runs `cargo login` or a
     credential provider (the coder's tools do not), and a deployment that must give a run a cargo registry token gives
     it as `CARGO_REGISTRY_TOKEN` in the environment, which cargo reads without persisting.
   * npm's HTTP cache stores a fixed list of request headers that does not include `Authorization`, **plus the headers a
     response names in `Vary`** (*verified 2026-10-11*, `make-fetch-happen` v14.0.3, `lib/cache/entry.js`,
     `getMetadata`): a private registry that answers `Vary: Authorization` makes npm write the token into
     `/cache/npm`. A deployment that gives runs an npm token must not share the cache; it sets `npm_config_cache` to a
     directory of its own in the pod's environment.
   * `config.extraEnv` is the deployer's: the chart refuses `CARGO_HOME` and the npm cache variable there while the cache
     is on, because they would send a tool's cache somewhere the volume does not reach.
5. **Fail closed.** A claim name that is not a name, `cache.existingClaim` beside `cache.create`, an access mode a cache
   cannot use, or a claim without ReadWriteMany for several pods, stops the chart's render. The operator refuses the same
   name as `ConfigInvalid`; a malformed `DEVCONTAINER_CACHE_DIR` is exit 78.
6. **Devcontainers get the volume and one variable.** With `DEVCONTAINER_CACHE_DIR` set, the directory (visible at the
   same path to the coder and to the Podman service, like the workspace root) is bound read-write at `/cache`, and
   commands get `npm_config_cache=/cache/npm` unless the caller sets it. Nothing else, because a repository's image is
   foreign:
   * **`CARGO_HOME` is not set.** A devcontainer image that has Rust keeps its rustup proxies, its `config.toml`
     (mirrors, linker, `[net]` settings) and its installed tools in its own `CARGO_HOME`; pointing cargo at an empty one
     hides all of it. The cargo cache of a devcontainer stays in the container until the proxy below.
   * **The pnpm store is not set.** pnpm reads its store from `store-dir` in its own config, and from
     `npm_config_store_dir`, which npm also reads and then warns about on every run ("Unknown env config"); the image
     avoids that by writing pnpm's own files, which a foreign image cannot be given.
   * The bind is the one `bind` the policy lets through besides the ones the crate adds (the repository's own `bind`
     mounts are still refused, and so is a repository `volume` or `tmpfs` over `/cache`).
7. **It is a cache.** Nothing depends on a hit; every tool downloads again on a miss. Deleting the volume only makes the next
   builds slow. Nothing prunes it: sizing and clean-up are the deployer's (the volume's size, `npm cache verify`,
   `pnpm store prune`; cargo deletes what it has not used for a month or three on its own, see below).

## Concurrency of each tool on a shared ReadWriteMany file system

Several pods write the same tree. What each tool does about it, and what we could not check:

| Tool | What it does | Status |
|---|---|---|
| **cargo** | Takes advisory file locks on `$CARGO_HOME` (`.package-cache`, `.package-cache-mutate`): an exclusive one to download and unpack, a shared one while it builds, an exclusive one to delete. **On an NFS mount it takes none** (`is_on_nfs_mount`: it would otherwise block forever), and it ignores a file system that reports locking unsupported. | *Verified 2026-10-11*, cargo source at tag `rust-1.94.0` (`src/cargo/util/flock.rs`, `cache_lock.rs`). The image's cargo is 1.98.1: the same files, *unverified*. |
| cargo on an NFS-backed volume | With no lock, two pods may download and unpack the same crate at once. `unpack_package` trusts a directory whose `.cargo-ok` parses and otherwise removes it and unpacks again, so a second pod can remove a directory a first is still reading. The result is a failed build that a retry fixes, or worse. | The missing lock is *verified* (above); the consequence is reasoned from `unpack_package` and *unverified*. **Use a class that honours flock(2)** (CephFS does, *unverified* here) or accept retries. Longhorn's ReadWriteMany is NFS (*unverified* here). |
| cargo's database | `.global-cache` is SQLite, opened without WAL on purpose ("shared-memory requirements cargo can't depend on due to things like network mounts") and only under cargo's own lock. | *Verified 2026-10-11*, `src/cargo/core/global_cache_tracker.rs`, `rust-1.94.0` |
| cargo's automatic clean | Deletes downloaded files not used for 3 months and generated ones not used for 1, at most once a day, under the exclusive lock, and skips when another cargo holds the lock. | *Verified 2026-10-11*, cargo `config.md` and `global_cache_tracker.rs`, `rust-1.94.0` |
| **npm** | `cacache` is "lockless, high-concurrency": content is written to a temporary file and moved into place, and the index is an append-only file of lines that each carry their own hash; a line that fails its hash is ignored ("Corruption or malice? Doesn't matter!"). | *Verified 2026-10-11*, `cacache` v19.0.1 (`README.md`, `lib/content/write.js`, `lib/entry-index.js`). The npm in the image (Node 24's) may use another release: *unverified*. |
| npm over NFS | The index is written with `appendFile` (`O_APPEND`), which "may lead to corrupted files on NFS filesystems if more than one process appends data to a file at once". A torn line fails its hash and is a cache miss, not a wrong answer. | *Verified 2026-10-11*, `open(2)` of man-pages (`man2/open.2`) and `entry-index.js` above. |
| **pnpm** | The store is content-addressed. Its locks are kept **outside** the store, in `$XDG_RUNTIME_DIR` or `/tmp`, so "pnpm processes only wait for each other when they share the lock directory": two pods never do. Its documentation promises parallel installs "on a local filesystem on the same machine". From v11 the store holds a SQLite `index.db` in WAL mode (the docs name the `-shm` files); SQLite says "WAL does not work over a network filesystem". | *Verified 2026-10-11*, `pnpm.io` docs (`settings/store.md`, `settings/node-modules.md`) and `sqlite.org/wal.html`. Which pnpm release a project uses (corepack picks it) and whether its store breaks across pods are *unverified*: **pnpm's store is the least safe part of the cache**. |
| pnpm, `/cache` and `/work` | Two volumes are two file systems: pnpm cannot hard-link from the store into `node_modules` and copies instead (documented `EXDEV` fallback). | *Verified 2026-10-11*, `pnpm.io` docs (`settings/store.md`, `storeDir`). |

If the owner's cluster shows `database disk image is malformed` from pnpm, or cargo failures on first fetch, the first
remedy is a storage class with real locks and the second is to turn the cache off (one value) until the proxy below
serves it. A switch that takes one tool off the volume would be a variable the image reads: a follow-up in
`vymalo/another-agentic-images` if the owner needs it. None of this is a reason to stop caching npm.

## The proxy, later

A pull-through proxy for the registries (npm, crates.io's sparse index, GitHub's git) is the owner's next step. It does
the same job without a shared writable tree: one process writes, every pod reads over HTTP, and the integrity checks
below are repeated at the proxy. It needs only configuration: `registry` for npm, a `[source]` replacement for cargo, and
egress to it (the run pods' NetworkPolicy blocks private ranges: `runPods.networkPolicy.extraEgress`). The volume then
stays as a per-node or per-release local cache, or goes. Products are not chosen here (*unverified*).

## Alternatives

* **Nothing** (the state before): every pod and run downloads everything. Simple, and slow and rate-limited.
* **A cache per pod** (ReadWriteOnce, or `emptyDir`): no sharing between workers or run pods, which is where the time goes.
* **Baking dependencies into the image**: stale on the first new dependency, and a very large image.
* **sccache with an S3 backend** (the image has the binary): caches compile outputs, not downloads. Complementary, not
  a replacement.
* **The proxy now**: right in the end, but a service to run and secure first. The volume is a mount.
* **One volume per tool**, so that the one with the least safe locking can be left out: three claims for the deployer to
  make. The subdirectories already allow it later.

## Consequences

* **A run can poison a cache another run reads.** The integrity checks defend against corruption and against a swapped
  package that a lockfile pins; they do not defend against a hostile writer.
  * npm and pnpm: *verified 2026-10-11* that npm checks "all data that passes through the cache" against its integrity
    hash (npm docs, `npm-cache`), and that pnpm checks a modified file's content before linking it. Both hashes are
    stored beside the content, in the cache. pnpm's documentation says what follows: `verifyStoreIntegrity` "does not
    make a store that is writable by untrusted users safe, because an attacker who can write to the store can alter
    both cached package contents and the metadata used to verify them", and the store "is intended to be shared only
    between mutually trusted users, jobs, and processes". A lockfile's integrity (`package-lock.json`, `pnpm-lock.yaml`)
    is checked against what the cache holds, so a poisoned entry for a pinned package is a miss: the residual risk is a
    project without a lockfile (*unverified* per npm client and flow).
  * cargo: `Cargo.lock` carries each crate's checksum and the index entry must agree, but `unpack_package` **does not
    verify an already unpacked source** (*verified 2026-10-11*, `src/cargo/sources/registry/mod.rs`, `rust-1.94.0`): a
    changed file under `registry/src` is compiled into the next build. Whatever a run writes to `$CARGO_HOME`
    (`config.toml` too: `build.rustc-wrapper`, `[source]`, `[alias]`) is read by every later cargo.
  * So the volume is **one trust domain**: every run on it is the owner's, and the volume is not shared with a deployment
    the owner does not trust. This is the decision of the image (its pull request 12) and the owner's. The proxy removes
    it; until then it is the price.
* **Cargo has no lock on NFS** (above): the storage class matters, and the chart cannot check it for an `existingClaim`.
* **A pod needs the volume to start.** A claim that does not exist or cannot be bound keeps the pod `Pending`: that is
  the deployer's to fix, and turning the cache off is one value.
* **No workaround for old images.** An image that predates its `/cache` configuration mounts an empty volume that no
  tool uses; the chart's image is pinned to one that has it (`WORKSPACE_TAG` `1.98.1-e6a528f`, and `runPods.image`).
* **The operator's field is a name, not a claim.** It never creates the volume or reads the namespace's claims (it has no
  right to); a missing claim is the pod's `Pending`.

## Amended

Nothing yet.
