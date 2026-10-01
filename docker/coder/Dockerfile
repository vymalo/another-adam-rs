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
# Anonymous pull verified 2026-09-29: the manifest request for this tag with an
# anonymous ghcr.io token returned 200.
ARG WORKSPACE_TAG=1.98.1-bc97d51

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

ENV WORKSPACE_ROOT=/work \
    LISTEN_ADDR=0.0.0.0:8080 \
    RUST_LOG=info \
    OPENCODE_DISABLE_AUTOUPDATE=1 \
    MCP_ALLOW_STDIO=true

# The runtime user of the workspace image (uid/gid 10001, `agent`).
USER 10001
WORKDIR /work
EXPOSE 8080

# Smoke test in a login shell as uid 10001: both binaries start and reject an
# empty environment (they fail closed) instead of crashing, adam-agent names the
# folder it needs, and the tools the agent shells out to are on PATH. The GitHub MCP server
# is there, runs, and lists the twelve read tools of the coder's allow-list over stdio with no
# credential (a missing name would stop the coder at startup).
RUN --mount=type=bind,source=docker/coder/test/github-mcp-tools.sh,target=/tmp/github-mcp-tools.sh \
    bash -lc 'set -e; \
      test "$(id -u)" = 10001; \
      command -v adam-coder adam-agent git opencode tini github-mcp-server; \
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
