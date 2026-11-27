# Releases: the procedure, the files and the evidence

For people: [Releases](../../../docs/guides/releases.md). That page decides; this one gives the files, the invariants
to check, the commands and the evidence. The decision is [ADR 0084](../../../docs/decisions/0084-releases-are-cut-by-release-please-and-production-pins-a-version.md).
Written at `ce99d98` plus this change on 2026-11-27. The model is vaam-apps/vpay at `master` on 2026-11-27
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
   **The run must survive later pushes.** `coder.yml`'s `concurrency.group` is `coder-<ref>` for a pull request and
   `coder-<sha>` for everything else: a shared group holds one pending run and a newer pending run cancels the older
   one even with `cancel-in-progress: false` (v0.6.0, run 46041005870, ANR-1195). The `bump` job has its own group
   `coder-bump` and skips when `main` has a newer non-bump commit. A hand `workflow_dispatch` on the tag also passes the
   gate (it filters `head_sha`, not the event); re-run `release.yml` after it.
7. **No `latest`, no tag on main.** `release.yml` runs on `push: tags: ["v*"]` only.

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

## Unverified until a tag is pushed

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
