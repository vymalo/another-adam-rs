# syntax=docker/dockerfile:1
#
# ghcr.io/vymalo/another-adam-rs/coder: the coder agent (bin/adam-coder) on
# the workspace toolchain image, and beside it the general agent binary
# (bin/adam-agent, which serves any agent folder), the egress proxy for run pods
# (bin/adam-egress, entrypoint `tini -- adam-egress`), the loopback sidecar that signs a run's
# model requests with its own certificate (bin/run-signer, ADR 0086) and the git sandbox
# (crates/adam-git-sandbox: the server `adam-git-sandbox`, entrypoint `tini -- adam-git-sandbox`, and the
# relay `adam-git` that stands in for git). Build from the REPOSITORY ROOT:
#
#   docker buildx build -f docker/coder/Dockerfile .
#
# The entrypoint is adam-coder. A service that serves an agent folder runs the
# same image with the entrypoint `tini -- adam-agent` and the folder mounted at
# ADAM_AGENT_DIR (compose.yaml, service `agent`): one image and one package, so
# there is nothing else to publish or to make public.
#
# Stage 1 compiles the seven binaries (and `adam-kube-exec`, the client a run's commands use to run in a pod of
# their own, ADR 0019, `adam-egress`, the HTTPS-only proxy those pods reach the internet through,
# `run-signer`, the sidecar that adds a run's `Turn-Signature` in front of the gateway, and
# `adam-git-sandbox` with `adam-git`, which run the coder's git where no secret is readable)
# for `x86_64-unknown-linux-musl`, statically linked (ADR 0053 decision 11), so they start in any x86-64
# Linux image whatever its C library: the bench starts the coder from each task image (ADR 0052
# decision 10). Stage 2 is the workspace image (Rust, Flutter/Dart, Node, git, tini) with the binaries
# added; the base stays a glibc image because the binaries no longer care, and the tools in it do.

# Rust builder, pinned by tag and digest.
# Verified 2026-11-24: HEAD on registry-1.docker.io/v2/library/rust/manifests/1.98-trixie
# (OCI index accept header) returned this docker-content-digest; the tag
# 1.98.1-trixie has the same digest. The workspace's rust-version is 1.98.
# Written inline in FROM (not through an ARG) so hadolint can see the tag.

# The official GitHub MCP server (github.com/github/github-mcp-server), pinned by tag and digest of
# its image index: the coder reads GitHub through it, read-only (bin/adam-coder/agent/mcp.json,
# ADR 0009). Only its binary is taken: a static Go binary (CGO_ENABLED=0, no libc), at /server/ in
# the upstream image (distroless). Verified 2026-10-01: an anonymous ghcr.io token and a manifest
# request for the tag v1.12.2 (also 1.12.2) returned this index digest (amd64 and arm64), whose
# labels say version 1.12.2, revision 85598ba. v1.13.0 was published the same day and has not been
# verified; move the tag, the digest and the claims in ADR 0009 together.

# Tag of the runtime base image, of the form <rust toolchain>-<sha7>: immutable,
# published by vymalo/another-agentic-images (merged in #7 as bc97d51). Deployments
# pin an immutable tag, never `latest`. Bump it when that image changes (the tag
# of its newest build on main), together with anything here that depends on it.
# Anonymous pull verified 2026-11-19: the manifest request for this tag with an
# anonymous ghcr.io token returned 200 and sha256:b465808bfb8a1baae5e59a429bf292726b021e3b37322f759dbda3ee2c57ed67
# (a single linux/amd64 manifest, 7 layers, 3.05 GB compressed), and the image's config has
# /opt/obscura/bin on PATH (obscura, a headless browser, vymalo/another-agentic-images#11) and
# the package caches under /cache (CARGO_HOME=/cache/cargo; npm and pnpm by their own config
# files, checked on 2026-10-11; vymalo/another-agentic-images#12, ADR 0040). This tag is the merge
# of vymalo/another-agentic-images#15 (commit 8c297d3, 2026-11-17), which adds rust-analyzer,
# gopls, typescript-language-server and pyright: the servers of the coder's language-server tools
# (ADR 0044, slice 17). The same build is the chart's run pod image
# (`deploy/coder/values.yaml`, by tag and digest): move both together.
ARG WORKSPACE_TAG=1.98.1-8c297d3

FROM rust:1.98-trixie@sha256:a8a5f0a1e5fe7dfe1d352591e4a1c7dd2c08fd70475cae872cf3458ba0df0546 AS build
WORKDIR /src
# The musl target and its C compiler: `musl-gcc` builds the C sources of aws-lc-sys and mimalloc, the
# global allocator of every binary (ADR 0053 decision 11). `protoc` is vendored and runs on this build
# host, so it is not cross-compiled. `file` checks the result below.
# hadolint ignore=DL3008
RUN apt-get update \
 && apt-get install -y --no-install-recommends file musl-tools \
 && rm -rf /var/lib/apt/lists/* \
 && rustup target add x86_64-unknown-linux-musl
ENV CC_x86_64_unknown_linux_musl=musl-gcc
# The workspace is `crates/*` and `bin/*`, so every member's manifest is needed to
# resolve the lockfile even though only the seven binaries are built. Only a C compiler is needed
# (aws-lc-sys uses its pregenerated sources; the vendored protoc covers a2a-pb), here the musl one.
COPY Cargo.toml Cargo.lock ./
COPY crates crates
COPY bin bin
# The commit the image is built from (CI passes the commit sha): `option_env!` bakes it into the seven binaries, which say
# it on their card as the version's build metadata (`0.1.0+<first 7>`) and in `build/v1` (ADR 0028). A build
# without it says `unknown`. Declared after the COPYs so that it never invalidates them.
ARG ADAM_BUILD_REVISION=
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --target x86_64-unknown-linux-musl \
      -p adam-coder --bin adam-coder -p adam-agent --bin adam-agent \
      -p adam-env-kubernetes --bin adam-kube-exec -p adam-egress --bin adam-egress \
      -p adam-run-signer --bin run-signer \
      -p adam-git-sandbox --bin adam-git-sandbox --bin adam-git \
 && install -D -m 0755 target/x86_64-unknown-linux-musl/release/adam-coder /out/adam-coder \
 && install -D -m 0755 target/x86_64-unknown-linux-musl/release/adam-agent /out/adam-agent \
 && install -D -m 0755 target/x86_64-unknown-linux-musl/release/adam-kube-exec /out/adam-kube-exec \
 && install -D -m 0755 target/x86_64-unknown-linux-musl/release/adam-egress /out/adam-egress \
 && install -D -m 0755 target/x86_64-unknown-linux-musl/release/run-signer /out/run-signer \
 && install -D -m 0755 target/x86_64-unknown-linux-musl/release/adam-git-sandbox /out/adam-git-sandbox \
 && install -D -m 0755 target/x86_64-unknown-linux-musl/release/adam-git /out/adam-git \
 && file /out/adam-coder /out/adam-agent /out/adam-kube-exec /out/adam-egress /out/run-signer /out/adam-git-sandbox /out/adam-git \
      > /tmp/binaries.file \
 && test "$(grep -Ec 'statically linked|static-pie linked' /tmp/binaries.file)" = 7

FROM ghcr.io/github/github-mcp-server:v1.12.2@sha256:508a0857ec762b1ab1cece29193345b501fab1dd9d1228a7b617062954cecac6 AS github-mcp

FROM ghcr.io/vymalo/another-agentic-images/workspace:${WORKSPACE_TAG}

LABEL org.opencontainers.image.source="https://github.com/vymalo/another-adam-rs" \
      org.opencontainers.image.description="adam-coder: coding task to verified pull request over A2A; adam-agent: any agent folder over A2A (entrypoint override)" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0"

# hadolint ignore=DL3066
USER root
# The seven binaries are static (x86_64-unknown-linux-musl): they carry their own C library and need no
# loader from this base or from a task image that runs them (ADR 0053 decision 11). The paths do not change.
COPY --from=build /out/adam-coder /out/adam-agent /out/adam-kube-exec /out/adam-egress /out/run-signer /out/adam-git-sandbox /out/adam-git /usr/local/bin/
COPY --from=github-mcp /server/github-mcp-server /usr/local/bin/github-mcp-server
# The tools a run pod gets (ADR 0019): the chart's init container copies /opt/adam/bin/* into an emptyDir that the
# run container mounts read-only at the same path. `adam-exec` is the script adam-env-kubernetes ships (one
# source); the pod's image need not carry it. Nothing else is there: the coder brings no agent binary into a run
# (ADR 0038). The OpenCode CLI of the base image stays on PATH, for people and for repositories that want it.
COPY --chmod=0755 crates/adam-env-kubernetes/src/adam-exec.sh /opt/adam/bin/adam-exec
# /work holds mirrors and worktrees; the deployment mounts a volume there. The
# base image already creates it owned by uid 10001, as does this line for a
# base that does not.
RUN install -d -o 10001 -g 10001 /work
# MCP_ALLOW_STDIO is **not** set here: the image carries two agent binaries, and only the coder may start
# local-process MCP servers if its deployment says so (the chart and compose.yaml still set it for one
# release); `adam-agent` from this image refuses such servers unless its own deployment opts in. The coder's
# shipped mcp.json no longer starts one: it reads `github-mcp-server http`, which the deployment runs as a
# sidecar from this same image (the chart's `githubMcp`, the `github-mcp` service of compose.yaml) and which
# holds no credential (ADR 0017, D4). The binary stays in the image for that, and for the stdio listing test.
ENV WORKSPACE_ROOT=/work \
    LISTEN_ADDR=0.0.0.0:8080 \
    RUST_LOG=info \
    OPENCODE_DISABLE_AUTOUPDATE=1

# The runtime user of the workspace image (uid/gid 10001, `agent`).
USER 10001
WORKDIR /work
EXPOSE 8080

# Smoke test in a login shell as uid 10001: the binaries start and reject an
# empty environment (they fail closed) instead of crashing, adam-agent names the
# folder it needs, adam-egress exits 78 for want of an allow-list, run-signer exits 78 for want of its
# upstream and its run certificate, adam-git-sandbox exits 78 for want of
# its socket and root, adam-git (the relay that stands in for git, and never runs it itself) says that it
# has no socket and exits 78, and the tools the agent
# shells out to are on PATH. The GitHub MCP server
# is there, runs, and lists the twelve read tools of the coder's allow-list over stdio with no
# credential (a missing name would stop the coder at startup). The run pod's tools are in
# /opt/adam/bin (adam-exec only: no OpenCode link, ADR 0038), and adam-kube-exec rejects an empty
# command line.
# obscura comes with the base; it is checked because the coder's instructions name it.
RUN --mount=type=bind,source=docker/coder/test/github-mcp-tools.sh,target=/tmp/github-mcp-tools.sh \
    bash -lc 'set -e; \
      test "$(id -u)" = 10001; \
      command -v adam-coder adam-agent adam-kube-exec adam-egress run-signer adam-git-sandbox adam-git git opencode tini github-mcp-server obscura; \
      github-mcp-server --version | grep -q "v1.12.2"; \
      test -x /opt/adam/bin/adam-exec; \
      test ! -e /opt/adam/bin/opencode; \
      test "$(ls /opt/adam/bin)" = adam-exec; \
      out="$(adam-kube-exec 2>&1 || true)"; \
      echo "$out" | grep -q "expected run or shell"; \
      sh /tmp/github-mcp-tools.sh; \
      out="$(env -i PATH="$PATH" adam-coder 2>&1 || true)"; \
      echo "$out" | grep -q "invalid configuration"; \
      out="$(env -i PATH="$PATH" adam-agent 2>&1 || true)"; \
      echo "$out" | grep -q "ADAM_AGENT_DIR is required"; \
      code=0; out="$(env -i PATH="$PATH" adam-egress 2>&1)" || code=$?; \
      test "$code" = 78; \
      echo "$out" | grep -q "invalid configuration"; \
      code=0; out="$(env -i PATH="$PATH" run-signer 2>&1)" || code=$?; \
      test "$code" = 78; \
      echo "$out" | grep -q "invalid configuration"; \
      code=0; out="$(env -i PATH="$PATH" adam-git-sandbox 2>&1)" || code=$?; \
      test "$code" = 78; \
      echo "$out" | grep -q "invalid configuration"; \
      code=0; out="$(env -i PATH="$PATH" adam-git status 2>&1)" || code=$?; \
      test "$code" = 78; \
      echo "$out" | grep -q "GIT_SANDBOX_SOCKET"'

# tini as PID 1 reaps the shells and git processes the agent spawns and
# forwards SIGTERM, which starts a graceful shutdown. `adam-agent` is run with
# `--entrypoint tini` (compose: `entrypoint: ["tini", "--", "adam-agent"]`).
ENTRYPOINT ["tini", "--", "adam-coder"]
