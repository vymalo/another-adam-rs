# syntax=docker/dockerfile:1
#
# ghcr.io/vymalo/another-adam-rs/coder: the coder agent (bin/adam-coder) on
# the workspace toolchain image, and beside it the general agent binary
# (bin/adam-agent, which serves any agent folder). Build from the REPOSITORY ROOT:
#
#   docker buildx build -f docker/coder/Dockerfile .
#
# The entrypoint is adam-coder. A service that serves an agent folder runs the
# same image with the entrypoint `tini -- adam-agent` and the folder mounted at
# ADAM_AGENT_DIR (compose.yaml, service `agent`): one image and one package, so
# there is nothing else to publish or to make public.
#
# Stage 1 compiles both binaries on a Debian trixie Rust image, so their glibc
# matches the runtime image (debian:trixie-slim). Stage 2 is the workspace
# image (Rust, Flutter/Dart, Node, git, tini, OpenCode) with the binaries added.

# Rust builder, pinned by tag and digest.
# Verified 2026-09-29: HEAD on registry-1.docker.io/v2/library/rust/manifests/1.94-trixie
# (OCI index accept header) returned this docker-content-digest; the tag
# 1.94.1-trixie has the same digest. The workspace's rust-version is 1.94.
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
# Anonymous pull verified 2026-10-01: the manifest request for this tag with an
# anonymous ghcr.io token returned 200, and the image's config has the
# `devcontainer.metadata` label (it is a dev container base: the default environment of the
# coder's devcontainers, ADR 0010). The same build is the default of DEVCONTAINER_DEFAULT_IMAGE,
# by its digest alone (bin/adam-coder/src/config.rs: the devcontainer CLI cannot parse a tag together
# with a digest); a test keeps this tag equal to the tag that digest was published as.
ARG WORKSPACE_TAG=1.98.1-ee2273e

FROM rust:1.94-trixie@sha256:652612f07bfbbdfa3af34761c1e435094c00dde4a98036132fca28c7bb2b165c AS build
WORKDIR /src
# The workspace is `crates/*` and `bin/*`, so every member's manifest is needed to
# resolve the lockfile even though only the two binaries are built. Only a C compiler is needed
# (aws-lc-sys uses its pregenerated sources; the vendored protoc covers a2a-pb).
COPY Cargo.toml Cargo.lock ./
COPY crates crates
COPY bin bin
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p adam-coder --bin adam-coder -p adam-agent --bin adam-agent \
 && install -D -m 0755 target/release/adam-coder /out/adam-coder \
 && install -D -m 0755 target/release/adam-agent /out/adam-agent

FROM ghcr.io/github/github-mcp-server:v1.12.2@sha256:508a0857ec762b1ab1cece29193345b501fab1dd9d1228a7b617062954cecac6 AS github-mcp

FROM ghcr.io/vymalo/another-agentic-images/workspace:${WORKSPACE_TAG}

# The devcontainer CLI (MIT, no runtime dependencies, needs node 20 or newer: the base has 24), which
# makes and enters a repository's devcontainer, and Podman's remote client, which it drives against
# the rootless Podman service CONTAINER_HOST names (ADR 0010). Both are only clients: nothing in this image
# runs containers. Verified 2026-10-01: @devcontainers/cli 0.89.0 is the newest release (npm registry).
# The client must be the **service's own release** (5.8.7, dev/podman/Containerfile): CI's real-Podman job
# found that a distribution's older client (Ubuntu's 4.9.3) pulls and inspects but then hangs in the CLI's
# image build against the 5.x service. So it is the static client of containers/podman v5.8.7, pinned by
# the release's sha256 (verified 2026-10-01), and not a distribution's package. Bump both together with
# the service.
ARG DEVCONTAINERS_CLI_VERSION=0.89.0
ARG PODMAN_REMOTE_VERSION=5.8.7
ARG PODMAN_REMOTE_SHA256=01b5ac597de2f7d55a7d52ba82c8e59df9e90355c5331849d6b7dbecf2345e7e

LABEL org.opencontainers.image.source="https://github.com/vymalo/another-adam-rs" \
      org.opencontainers.image.description="adam-coder: coding task to verified pull request over A2A; adam-agent: any agent folder over A2A (entrypoint override)" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0"

# hadolint ignore=DL3066
USER root
COPY --from=build /out/adam-coder /out/adam-agent /usr/local/bin/
COPY --from=github-mcp /server/github-mcp-server /usr/local/bin/github-mcp-server
# /work holds mirrors and worktrees; the deployment mounts a volume there. The
# base image already creates it owned by uid 10001, as does this line for a
# base that does not.
RUN install -d -o 10001 -g 10001 /work
# The static client of the service's own release; `sha256sum -c` stops the build when the download is not
# the pinned file. (Not `ADD --checksum`: the hadolint of the `lint Dockerfile` job cannot parse the flag.)
RUN curl -fsSL -o /tmp/podman-remote.tar.gz \
      "https://github.com/containers/podman/releases/download/v${PODMAN_REMOTE_VERSION}/podman-remote-static-linux_amd64.tar.gz" \
 && printf '%s  %s\n' "${PODMAN_REMOTE_SHA256}" /tmp/podman-remote.tar.gz > /tmp/podman-remote.sha256 \
 && sha256sum -c /tmp/podman-remote.sha256 \
 && tar -xzf /tmp/podman-remote.tar.gz -C /tmp \
 && install -m 0755 /tmp/bin/podman-remote-static-linux_amd64 /usr/local/bin/podman-remote \
 && rm -rf /tmp/podman-remote.tar.gz /tmp/podman-remote.sha256 /tmp/bin \
 && npm install -g --no-fund --no-audit "@devcontainers/cli@${DEVCONTAINERS_CLI_VERSION}"

# MCP_ALLOW_STDIO is **not** set here: the image carries two binaries, and only the coder may start
# local-process MCP servers if its deployment says so (the chart and compose.yaml still set it for one
# release); `adam-agent` from this image refuses such servers unless its own deployment opts in. The coder's
# shipped mcp.json no longer starts one: it reads `github-mcp-server http`, which the deployment runs as a
# sidecar from this same image (the chart's `githubMcp`, the `github-mcp` service of compose.yaml) and which
# holds no credential (ADR 0017, D4). The binary stays in the image for that, and for the stdio listing test.
ENV WORKSPACE_ROOT=/work \
    LISTEN_ADDR=0.0.0.0:8080 \
    RUST_LOG=info \
    OPENCODE_DISABLE_AUTOUPDATE=1 \
    DEVCONTAINER_CLI=devcontainer \
    DEVCONTAINER_PODMAN=podman-remote

# The runtime user of the workspace image (uid/gid 10001, `agent`).
USER 10001
WORKDIR /work
EXPOSE 8080

# Smoke test in a login shell as uid 10001: both binaries start and reject an
# empty environment (they fail closed) instead of crashing, adam-agent names the
# folder it needs, and the tools the agent shells out to are on PATH. The GitHub MCP server
# is there, runs, and lists the twelve read tools of the coder's allow-list over stdio with no
# credential (a missing name would stop the coder at startup). The devcontainer CLI and Podman's client
# run, and the OpenCode that is mounted into every devcontainer is a native executable (an ELF file, not
# the npm package's script: DEVCONTAINER_RUNTIME=podman refuses to start otherwise).
RUN --mount=type=bind,source=docker/coder/test/github-mcp-tools.sh,target=/tmp/github-mcp-tools.sh \
    bash -lc 'set -e; \
      test "$(id -u)" = 10001; \
      command -v adam-coder adam-agent git opencode tini github-mcp-server devcontainer podman-remote; \
      test "$(devcontainer --version)" = "0.89.0"; \
      podman-remote --version | grep -q "5\.8\.7"; \
      test "$(head -c 4 "$(readlink -f "$(command -v opencode)")" | tail -c 3)" = ELF; \
      github-mcp-server --version | grep -q "v1.12.2"; \
      sh /tmp/github-mcp-tools.sh; \
      out="$(env -i PATH="$PATH" adam-coder 2>&1 || true)"; \
      echo "$out" | grep -q "invalid configuration"; \
      out="$(env -i PATH="$PATH" adam-agent 2>&1 || true)"; \
      echo "$out" | grep -q "ADAM_AGENT_DIR is required"'

# tini as PID 1 reaps the shells and git processes the agent spawns and
# forwards SIGTERM, which starts a graceful shutdown. `adam-agent` is run with
# `--entrypoint tini` (compose: `entrypoint: ["tini", "--", "adam-agent"]`).
ENTRYPOINT ["tini", "--", "adam-coder"]
