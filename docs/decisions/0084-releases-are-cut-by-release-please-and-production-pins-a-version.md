# 0084. Releases are cut by release-please, and production pins a version

Status: **Accepted** (2026-11-27). The owner decided on 2026-11-27: *"Cheat how I'm doing
[here](https://github.com/vaam-apps/vpay/pull/417) to have automatic commits collections for building versions ... so
that we have a similar flow for all of them. In prod we'll deploy only fixed versions like that"*. The flow is
vaam-apps/vpay's; decisions 2, 4 and 5 are this ADR's choices for the owner to confirm.

## Context

adam-rs had no release. Every commit of `main` that passes `coder.yml` pushes
`ghcr.io/vymalo/another-adam-rs/coder:sha-<7>` and commits `chore(deploy): bump coder to sha-<7>` to
`deploy/coder/values.yaml`. A deployment that renders the chart from `main` moves whenever that commit lands; one that
pins picks a `sha-<7>` and reads the history to learn what changed. The crates said `0.1.0`, the chart `0.5.0` with
`appVersion` `0.1.0`, and nothing moved either. Nothing checked that a commit's subject was a Conventional Commit.

vaam-apps/vpay releases with release-please (*verified 2026-11-27* from its `master`: `release-please-config.json`,
`.release-please-manifest.json`, `.github/workflows/release-please.yml`, `.github/workflows/release.yml`; its pull
request 417 is a release pull request, `chore: release master`, 0.19.0): `release-type: simple`, `include-v-in-tag`,
`bump-minor-pre-major`, every version line marked `x-release-please-version` and listed in `extra-files`, a GitHub App
token so the tag release-please creates starts the release workflow, and on a `v*` tag images tagged `X.Y.Z` and
`X.Y`, never `latest`, with buildx provenance (`mode=max`) and an SBOM, signed with cosign keyless, and the chart
pushed to GHCR as OCI and signed.

## Decision

1. **release-please, as vpay runs it.** `release-please.yml` on every push to `main` opens or updates one release
   pull request (`chore: release main`) with the changelog of the Conventional Commits since the last release and every
   marked version moved. Merging it tags `vX.Y.Z` and makes the GitHub release. The configuration is vpay's:
   `release-type: simple`, `include-v-in-tag: true`, `include-component-in-tag: false`, `bump-minor-pre-major: true`,
   `bump-patch-for-minor-pre-major: false`, and vpay's changelog sections. The token is a GitHub App's, minted by
   `actions/create-github-app-token` from the secrets `RELEASE_PLEASE_APP_CLIENT_ID` and
   `RELEASE_PLEASE_APP_PRIVATE_KEY` (vpay's names), with `contents: write` and `pull-requests: write` only. There is
   no fallback to `GITHUB_TOKEN`: a tag it creates starts no workflow, so a release would publish nothing, silently.
2. **One version for the repository, seeded at 0.5.0.** The workspace's `version`, every path dependency's `version`
   (a 0.x caret range does not cross a minor, so one left behind stops the workspace resolving) and the chart's
   `version` and `appVersion` are marked and move together. The manifest starts at `0.5.0`, the chart's version when
   this landed, so the chart's version never goes down; the crates and `appVersion` join it at the first release, which
   is `0.5.1` (fixes and chores only) or `0.6.0` (a feature). `bootstrap-sha` is `main` at adoption (`ce99d98`), so the
   first changelog starts there. A `Release-As: X.Y.Z` footer picks another number.
3. **What cannot be marked moves on the release branch.** After release-please, the same job runs `cargo metadata` on
   every `Cargo.lock` and renders the chart's four goldens again: every object carries `helm.sh/chart: coder-<version>`
   and `app.kubernetes.io/version`, and the checksums of the ConfigMaps a pod reads follow them. The release pull
   request's own CI (`render-check.sh`, the Rust jobs with `--locked`) proves both. A pull request that adds an
   unmarked path dependency, or a marked file that is not listed, fails `ci.yml`.
4. **On a `v*` tag, the image is built again, not retagged.** `coder.yml` keeps pushing the image it smoke-tested,
   from the runner's Docker, with `docker push`: that carries no provenance and no SBOM, and buildx makes them only
   when it builds and pushes the image itself. So `release.yml` (a) waits until `coder.yml`'s image job has passed on
   the tag's commit (the smoke tests and every compose scenario), (b) builds that commit with the same build arguments
   and main's cache, with `provenance: mode=max` and `sbom: true`, and pushes it by digest with no tag, (c) runs the
   container smoke tests on that digest, (d) signs it with cosign keyless, and only then (e) names it `X.Y.Z` and
   `X.Y`. A release tag only ever names a signed, smoke-tested digest; it is not the digest of `sha-<7>`. A published
   `X.Y.Z` is never moved; `X.Y` follows the newest patch. No `latest`.
5. **The chart is `oci://ghcr.io/vymalo/another-adam-rs/charts/coder`.** Beside the image, under the repository's
   own namespace, and under `charts/` so the chart and the image (`.../coder`) are not two artifacts of one name. The
   packaged chart is `deploy/coder` at the tag with `image.tag` set to `X.Y.Z` in the copy that is packaged;
   `values.yaml` on `main` keeps `coder.yml`'s `sha-<7>`. A version already published and signed stops the release;
   published and unsigned (a run that died between push and sign) is signed and not pushed again.
6. **`sha-<7>` stays for development.** `coder.yml`, its image, its tag and its bump of `values.yaml` do not change.
7. **Production pins `X.Y.Z@sha256:<digest>`**, the image by tag and digest (`image.tag` accepts the pair), the chart
   by version. A consumer that follows `main` can keep a `sha-<7>`.
8. **Every subject is a Conventional Commit.** `tools/commit-lint.sh` is the rule, from vymalo/another-agentic-system,
   with the types of the changelog (`deps` included) and a comma between two scopes; CI checks the pull request's title
   (a squash merge makes it the commit) and every commit, and `.githooks/commit-msg` checks locally.
9. **Pinned actions.** Every action the new workflows use is pinned by its full commit sha, with its version in a
   comment.

## Consequences

- **The owner sets up a GitHub App** with `contents: write` and `pull-requests: write`, installs it on this
  repository, and adds the secrets `RELEASE_PLEASE_APP_CLIENT_ID` (the Client ID, not the App ID) and
  `RELEASE_PLEASE_APP_PRIVATE_KEY`. Until then `release-please.yml` fails at its first step and nothing is released.
- **The chart's package is new and private** until its owner makes it public; check an anonymous pull before a
  deployment points at it. The image package is public already.
- **A release changes two labels of every object and two checksums** in the chart's render. The platform renders this
  chart for its parity goldens at a pinned revision (`tools/adam-parity`); moving that pin across a release moves those
  lines and nothing else. The markers added here are comments: no render changes.
- **A release builds the image twice** (once in `coder.yml` on the merge, once here); the second reads main's cache.
- **Titles like `ANR-123: ...` are refused** from now on; the task id goes in the scope or the body.
- **Unverified until the first tag:** the App token minting and the release pull request, the release image's build
  and smoke tests on a tag, the carbon copy (`imagetools create --prefer-index=false`) keeping the signed digest,
  creating the chart's package with `GITHUB_TOKEN`, and `cosign verify` of either. `release.yml` checks each in the run.

## Amended 2026-11-27: the tag's commit keeps its `coder.yml` run (ANR-1195)

The first release, v0.6.0, failed its gate: no `coder.yml` run on the tag's commit had passed its image job. The cause
was not `cancel-in-progress` (already off for `main`) but the concurrency group. A group runs one workflow and holds
one pending; a newer pending run **cancels the older pending run even with `cancel-in-progress: false`**. Seven docs
pull requests merged within minutes of the release commit, so its run was replaced while it waited. The fix:

1. **The release commit has a group of its own, its sha** (`coder-<sha>`), and so has a manual dispatch; a pull request
   keeps `coder-<ref>` and keeps cancelling; every other push to main shares `coder-refs/heads/main` and skips to the
   newest. The release commit is special because release.yml's gate needs exactly its run, and it is recognised by
   its subject, `chore: release main (#N)`. Giving every merge a group would run a 90-minute build and e2e for each
   merge of a burst in parallel (about 11 for v0.6.0's). A release cut from another commit, or a gate that finds its
   run replaced, is recovered by a `workflow_dispatch` of `coder.yml` on the tag.
2. **The gate accepts any `coder.yml` run on the commit whose image job concluded `success`**, whatever the run's
   status or conclusion (a later job, such as the bump, may be cancelled), a manual dispatch on the tag included, and
   keeps waiting while a run of the commit has not finished its image job.
3. **`bump the tag in the chart` has no concurrency group.** A queue ordered by finish time lets an older commit's
   pending bump cancel the newest commit's pending bump and then skip. It cannot write a stale tag without the group:
   it skips when `main` has a non-bump commit newer than its own, and its fetch, reset and retry loop keeps the newest
   writing. If the newer commit's image job fails, the chart keeps the older tag until the next good build.
4. `ci.yml` is not read by the gate; its group is unchanged. `coder.yml` has no tag trigger: a tag's commit is reached
   by the main push or by a dispatch.

## Amended 2026-11-27: the release also ships `adam-agent` through Homebrew (ANR-1203)

The owner: *"We'll release adam agent and aapctl via homebrew. ... We'll release only when release-please's PR merge."*
A release is now also a binary. The decisions:

1. **The jobs are in `release.yml`, behind `gate`; there is no second workflow on `release: published`.** The tag push and
   the release event both follow the one App-made release; a second workflow would run beside the gate and not behind it,
   and cosign's identity is the workflow file, so the binaries would need a second identity to verify. The price is that
   the tag's push can arrive before the release is published, so `adam-agent-preflight` waits for it (30 minutes) and
   requires a published release authored by the release App itself (`<app-slug>[bot]`, the slug read from the App's token,
   so there is no variable to forget and nothing is accepted by default). A tag pushed by hand has no release and ships
   no binary. A published asset is never replaced: the archives and the signed checksums all present means a re-run builds
   and attaches nothing and tests what is there; some present stops the run for a person.
2. **Four native builds**, not cross-compiled: `x86_64` and `aarch64` `unknown-linux-musl` (static, `file` checked) on
   `ubuntu-24.04` and `ubuntu-24.04-arm`; `aarch64` and `x86_64` `apple-darwin` on `macos-26` and `macos-26-intel`,
   GitHub-hosted, because Apple's targets cannot be linked from Linux. Archives are `adam-agent_X.Y.Z_<os>_<arch>.tar.gz`.
   macOS binaries are not notarized (out of scope until the owner asks); Linux's are static, macOS's link system libraries only.
3. **`SHA256SUMS` is signed**, `cosign sign-blob` keyless with a bundle, under the identity of the image
   (`release.yml@refs/tags/vX.Y.Z`); the archives are checked through it.
4. **The formula is generated and proposed, never pushed**: the release App opens or updates one pull request on
   `vymalo/homebrew-tap` (`bot/adam-agent`), after Homebrew installed the formula on four platforms. A person merges it.
   Monotonic: a version older than the tap's is not proposed. The job's concurrency is `queue: max` with
   `cancel-in-progress: false`: no pending update is dropped, and an older release that arrives last is a no-op.
5. **`adam-agent` gains `--version` and `--help`**, answered before any configuration is read. Before, any argument
   was ignored and the process tried to start (exit 78 without a folder), so the formula's `test do` could not pass.
6. `aapctl` (another-agentic-platform) takes the same shape in its own repository; it is not decided here.

Unverified until a release: every runner step (see the agent page).
