# Releases: the procedure, the files and the evidence

For people: [Releases](../../../docs/guides/releases.md). That page decides; this one gives the files, the invariants
to check, the commands and the evidence. The decision is [ADR 0084](../../../docs/decisions/0084-releases-are-cut-by-release-please-and-production-pins-a-version.md).
Written at `ce99d98` plus this change on 2026-11-27, amended the same day for ANR-1195. The model is vaam-apps/vpay at `master` on 2026-11-27
(`release-please-config.json`, `.release-please-manifest.json`, `.github/workflows/release-please.yml`,
`.github/workflows/release.yml`; *verified* by reading them).

## The files

| File | Does |
|---|---|
| `release-please-config.json` | vpay's settings (`release-type: simple`, `include-v-in-tag`, `bump-minor-pre-major`, its changelog sections), `bootstrap-sha` `ce99d98b355af8ac5b4f1c164386e66b18452565`, and `extra-files`: the 16 files with a marked line |
| `.release-please-manifest.json` | `{".": "0.5.0"}`: the version the next release counts from |
| `.github/workflows/release-please.yml` | the App token (`:55-65`), release-please (`:67-73`), then on the release branch `tools/release/sync.sh` (`:94-96`) and one commit of what it moved (`:98-`) |
| `.github/workflows/release.yml` | on `v*`: `gate` (`:53-122`), `build` (`:125-188`), `publish` (`:192-`), the only job with `id-token: write` |
| `tools/release/check-versions.sh` | the marks are where a release needs them; with `vX.Y.Z`, every marked line says `X.Y.Z`. `ci.yml` job `fmt` runs it (`:58-59`), `release.yml`'s gate runs it with the tag |
| `tools/release/sync.sh` | `cargo metadata` on every tracked `Cargo.lock` outside `.agents/`, then `deploy/coder/tests/render-goldens.sh` |
| `tools/release/registry-digest.sh` | the digest of a ghcr.io reference by a `HEAD` of its manifest: exit 0 and the digest, 3 for no such tag or package, 1 otherwise |
| `deploy/coder/tests/render-goldens.sh` | writes the four goldens with the values `render-check.sh` compares each against |
| `deploy/coder/bump-tag.sh` | accepts `sha-<7 hex>` (coder.yml's bump, unchanged) or `X.Y.Z` (the copy `release.yml` packages) |
| `.github/workflows/release.yml`, jobs `adam-agent-preflight` (outputs `assets`: `none` or `complete`; a partial set fails), `adam-agent-build`, `adam-agent-publish`, `adam-agent-smoke`, `adam-agent-tap` | after `gate`, in this order: the release exists, is published and is the App's bot's; four native builds; sign `SHA256SUMS` and attach to the release; Homebrew install on four runners; the tap pull request. `adam-agent-publish` is the second job with `id-token: write` |
| `tools/release/write-homebrew-formula.mjs` | writes `Formula/adam-agent.rb` (four `on_macos`/`on_linux` x `on_arm`/`on_intel` stanzas, a sha256 each from `SHA256SUMS`, `license` from `[workspace.package]`, `bin.install "adam-agent"`, `test do system bin/"adam-agent", "--version" end`) and upserts the tap README's row between `<!-- cli-packages:begin/end -->`; node standard library only |
| `tools/release/update-homebrew-tap.mjs` | rebuilds `bot/adam-agent` from the tap's default branch, refuses a version older than the base's or the open branch's (`stale`), commits as `release-please[bot]`, pushes with `--force-with-lease`, creates or edits the one pull request |
| `tools/release/homebrew-smoke.sh` | a throwaway local tap (`brew tap-new --no-git local/adam-smoke`), the formula built from the release's `SHA256SUMS`, `brew install`, `brew test`, `adam-agent --version` says `adam-agent X.Y.Z`, and no configuration exits 78 |
| `tools/release/check-postgres-pin.sh` | every `image: postgres...` of `.github/workflows/*.yml` and `compose.yaml` is compose.yaml's line (PostgreSQL 18.7 by tag and digest, ADR 0082 decision 11) and there are at least five; `ci.yml` job `fmt` runs it. Broken once in each of coder.yml, release.yml and ci.yml on 2026-11-27 |
| `tools/release/test/` | `homebrew-formula.test.mjs` and its goldens (`golden/adam-agent.rb`, `golden/README.after.md`; `UPDATE_GOLDEN=1` rewrites them); `ci.yml` job `release-tools` runs it with `ruby -c` on the golden |
| `.github/actionlint.yaml` | adds the labels `macos-26` and `macos-26-intel`, and ignores one message (`unexpected key "queue"`) for `release.yml` alone: the pinned actionlint 1.7.7 knows neither, and no release of it parses `queue` |
| `tools/commit-lint.sh`, `.github/workflows/commit-lint.yml`, `.githooks/commit-msg` | Conventional Commits on the title and every commit of a pull request, and locally |

## Invariants to check on a change

1. **Every version a release moves is marked, and its file is listed.** The workspace `version` (`Cargo.toml:8`), every
   path dependency that carries a `version` (40 lines in `Cargo.toml`, 28 in 14 member manifests), `deploy/coder/Chart.yaml:7-8`.
   A new crate with a path dependency and a version needs `# x-release-please-version` on that line and, for a new
   manifest, a row in `extra-files`: `sh tools/release/check-versions.sh` fails otherwise. release-please's generic
   updater replaces the **first** `X.Y.Z` of a marked line, so a marked line carries only one.
2. **A marker changes no render.** The marks are YAML comments: `helm show chart deploy/coder` still says `version:
   0.5.0` and `appVersion: 0.1.0`, and `render-check.sh` passes against the unchanged goldens (below).
3. **`values.yaml`'s `image.tag` is never set to a release on main.** Only `coder.yml`'s bump writes it
   (`sha-<7>`); `release.yml` sets `X.Y.Z` in a copy under `$RUNNER_TEMP` and checks that the packaged chart renders
   only `ghcr.io/vymalo/another-adam-rs/coder:X.Y.Z`.
4. **A tag names only a signed, smoke-tested digest.** `publish` signs before `imagetools create`, uses
   `--prefer-index=false` (a carbon copy of the index, *verified* in the buildx reference on 2026-11-27) and reads
   both tags back with `registry-digest.sh`: another digest fails the run.
5. **Nothing published is overwritten.** An existing `X.Y.Z` of the image with another digest stops the run; the chart
   is probed first (published and signed: stop; published and unsigned: sign only; 404: push; anything else: stop),
   because `helm push` overwrites silently (vpay measured it against `registry:2`; *unverified* here).
6. **The release image is built from the tag's commit with coder.yml's build arguments** (`ADAM_BUILD_REVISION`, the
   revision label) and only after `coder.yml`'s job `build, smoke-test and push the image` concluded `success` on that
   commit, any event (`gate`, `:87-122`). Renaming that job breaks the gate.
   **The run must survive later pushes.** `coder.yml`'s `concurrency.group` is `coder-<ref>` for a pull request,
   `coder-<sha>` for a `workflow_dispatch` and for a commit whose message starts `chore: release` (release-please's
   squash), and `coder-<ref>` for every other push to main: a group holds one pending run and a newer pending run
   cancels the older even with `cancel-in-progress: false` (v0.6.0, run 46041005870, ANR-1195). The `bump` job has no
   group (a group ordered by finish time can lose the newest bump) and skips when `main` has a newer non-bump commit.
   The gate takes any `coder.yml` run on the commit (`head_sha`, any event, any run conclusion) in which the job named
   above concluded `success`, and waits while a run has not finished that job. A hand `workflow_dispatch` on the tag
   passes it; re-run `release.yml` after it.
   **`release-please.yml` follows the same rule (ANR-1457):** its group is the commit sha on a push, because
   release-please must *see every merge* to open or update the release pull request, and a per-ref group replaced the
   pending run of one merge with the next. Its `cancel-in-progress: false` still keeps a running one from being cut
   off, which it needs when it force-pushes the release branch. `docs-site.yml` is the deliberate opposite: one group
   per ref with `cancel-in-progress: true`, because the site publishes the **newest** commit and an older commit's
   build is meant to be dropped.
7. **No `latest`, no tag on main.** `release.yml` runs on `push: tags: ["v*"]` only.
8. **One trigger, one gate, for the binary too** (ANR-1203). The Homebrew jobs are in `release.yml` and `need: gate`: a
   `release: published` workflow would be a second trigger beside the tag push (both follow the App's release), would not
   wait for `gate`, and would sign under another workflow file, so the identity above would not cover the binaries.
   The cost is a wait: the tag's push can arrive before the release exists, so `adam-agent-preflight` polls
   `gh release view` for up to 30 minutes and requires: not a draft or prerelease, and the author `<app-slug>[bot]`, the slug being the `app-slug` output of
   `actions/create-github-app-token` (a read-only token minted in the job; fail closed: no variable, no default author).
   A tag pushed by hand has no release and stops there.
9. **The archives are named `adam-agent_<version>_<os>_<arch>.tar.gz`**, os `linux`/`darwin`, arch `amd64`/`arm64`, and hold
   `adam-agent`, `LICENSE-MIT` and `LICENSE-APACHE` at the top level. `adam-agent-build` checks the Linux binaries with `file`
   (`statically linked` or `static-pie linked`), the macOS ones with `otool -L` (only `/usr/lib` and `/System/Library`),
   and runs each: `--version` prints exactly `adam-agent X.Y.Z` (main's `bin/adam-agent/src/main.rs`, no `+<revision>`) and no configuration exits 78. The Linux targets are native
   (`ubuntu-24.04`, `ubuntu-24.04-arm`; 90 minutes, no build cache: a signed artifact is built from a clean checkout) with `CC_<target>=musl-gcc`; macOS cannot be linked from Linux, so `macos-26` and
   `macos-26-intel` build natively. `adam-agent --version` exists since #496 (`bin/adam-agent/src/main.rs`,
   `tests/binary.rs`): before it, `--version` started the server and exited 78. The macOS `tar` runs with `COPYFILE_DISABLE=1`.
   **A published asset is never replaced**: `adam-agent-publish` uploads without `--clobber` and runs only when `assets` is
   `none`; `adam-agent-build` likewise. With `complete`, `adam-agent-smoke` (which verifies the signature itself) and
   `adam-agent-tap` run on what is published, so a re-run cannot move a checksum under a merged formula. If GitHub's
   immutable releases are switched on, attaching after publish fails and the upload must move to a draft release.
10. **The tap is reached only after the signature and the install.** `adam-agent-tap` needs `adam-agent-smoke`, verifies
    `SHA256SUMS` with `cosign verify-blob` against this workflow's identity at the tag, and only then writes the formula
    from those checksums. Its token is the release App's, scoped to `another-adam-rs,homebrew-tap`; its concurrency
    group `homebrew-tap-adam-agent` is `queue: max` with `cancel-in-progress: false` (GitHub's docs, *verified 2026-11-27*: the default
    `single` replaces a pending run with the one that *arrives* later, whatever its version; `max` keeps up to 100 pending). Arrival
    order is not version order, so `update-homebrew-tap.mjs` refuses a version older than the base's or the branch's: an older run
    that comes last is a no-op.

## Read a release's digests

```sh
sh tools/release/registry-digest.sh ghcr.io/vymalo/another-adam-rs/coder X.Y.Z          # the image index
sh tools/release/registry-digest.sh ghcr.io/vymalo/another-adam-rs/charts/coder X.Y.Z   # the chart manifest
```

Anonymous for a public package; `GHCR_USER` and `GHCR_TOKEN` for a private one. A package that does not exist gets no
token from `ghcr.io/token` (`null`, *verified 2026-11-27* anonymously for `another-adam-rs/charts/coder`), which the
script reports as exit 3.

## Rehearse a release locally

What was run for this change, at `0.6.0`, from the repository root, then undone with `git checkout -- .`:

```sh
for f in $(jq -r '.packages["."]["extra-files"][].path' release-please-config.json); do
  awk '/x-release-please-version/ { sub(/[0-9]+\.[0-9]+\.[0-9]+/, "0.6.0") } { print }' "$f" > /tmp/f && cat /tmp/f > "$f"
done
sh tools/release/check-versions.sh v0.6.0    # 16 files, every line at 0.6.0
sh tools/release/sync.sh                     # both Cargo.lock files move 0.1.0 -> 0.6.0 for the workspace crates, nothing else
sh deploy/coder/tests/render-check.sh        # render checks passed
cargo metadata --offline --locked --format-version 1 > /dev/null   # the rewritten lock is consistent
```

The goldens' diff was 52 `helm.sh/chart` and 52 `app.kubernetes.io/version` lines, plus `checksum/run-pod` (two) and
`checksum/egress-allow` (two) in `run-pods.yaml` and `cache.yaml`. Rewriting only the two labels left those
checksums stale and `render-check.sh` failed three checks, which is why `sync.sh` renders the goldens again.

## The gates of this change (2026-11-27)

```sh
actionlint -shellcheck "$(command -v shellcheck)"           # 1.7.7 and 1.7.12, all nine workflows: clean
for s in tools/release/*.sh tools/commit-lint.sh .githooks/commit-msg deploy/coder/bump-tag.sh deploy/coder/tests/*.sh; do sh -n "$s"; done
sh deploy/coder/tests/render-check.sh                       # helm v4.3.2: render checks passed, goldens unchanged
sh deploy/coder/tests/bump-tag-test.sh                      # bump-tag tests passed
sh deploy/coder/tests/render-goldens.sh && git diff --exit-code deploy/coder/tests/golden   # byte for byte
node tools/docs-check/check-docs.mjs
```

## v0.6.0 was never published

Its tag exists (run 46041005870, job 140721911064 `build and smoke-test the release image`): `release.yml` had `postgres:17` as the
container smoke test's service, and `adam-coder` exits 78 on a server older than 18 (ADR 0082 decision 11). `release.yml` has no
`workflow_dispatch` and runs from the tag's copy, so the tag cannot be re-run green; the next release supersedes it. The pin
is now the one of coder.yml, ci.yml and compose.yaml, held by `tools/release/check-postgres-pin.sh`.

## Unverified until a tag is pushed

- the Homebrew path (ANR-1203), *verified locally 2026-11-27* only for what has no runner: `node --test "tools/release/test/*.test.mjs"`
  (4 tests, the golden formula and README), `ruby -c` on the golden, `update-homebrew-tap.mjs` against a local bare
  clone of the tap with a fake `gh` (created, unchanged on a second run, `stale` for an older version), `shellcheck` on the
  smoke script, `actionlint` 1.7.7 on every workflow. *Unverified:* the four builds (musl on arm64, mimalloc and bundled
  SQLite on the macOS runners), `brew tap-new --no-git`
  and `brew install` of a local-tap formula (Homebrew itself works on `ubuntu-24.04-arm` and `macos-26-intel`: the system's
  `homebrew-smoke.yml` runs there and the tap's merged `another-agentic` 0.6.0, vymalo/homebrew-tap#5, passed it), `cosign sign-blob --bundle` and `verify-blob` with the workflow identity,
  the `gh release view --json author` login being `<slug>[bot]`, `gh release upload` onto a release the App made, and
  the App's token reaching the tap;

- the App token (`client-id` and the two `permission-*` inputs of `actions/create-github-app-token` v3.2.0, as vpay
  uses them) and release-please v5.0.0 opening the pull request on this repository;
- `cargo metadata` on the runner's preinstalled toolchain (nothing installs one; the workspace's `rust-version` is 1.98,
  and `cargo metadata` does not enforce it);
- the release build reading main's `type=gha` cache from a tag ref, and the smoke tests on the pulled digest;
- `imagetools create --prefer-index=false` keeping the digest of an index with attestation manifests (the read-back
  fails the run if not);
- `GITHUB_TOKEN` with `packages: write` creating the new package `another-adam-rs/charts/coder`, and its answer for a
  version that is not there (the probe treats 404 and "no token" as not published, anything else as a stop);
- `cosign verify` of the image and the chart with the identity on the page for people.
