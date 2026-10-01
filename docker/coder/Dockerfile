# syntax=docker/dockerfile:1
#
# ghcr.io/vymalo/another-adam-rs/coder: the coder agent (bin/adam-coder) on
# the workspace toolchain image. Build from the REPOSITORY ROOT:
#
#   docker buildx build -f docker/coder/Dockerfile .
#
# Stage 1 compiles the binary on a Debian trixie Rust image, so its glibc
# matches the runtime image (debian:trixie-slim). Stage 2 is the workspace
# image (Rust, Flutter/Dart, Node, git, tini, OpenCode) with the binary added.

# Rust builder, pinned by tag and digest.
# Verified 2026-09-29: HEAD on registry-1.docker.io/v2/library/rust/manifests/1.94-trixie
# (OCI index accept header) returned this docker-content-digest; the tag
# 1.94.1-trixie has the same digest. The workspace's rust-version is 1.94.
# Written inline in FROM (not through an ARG) so hadolint can see the tag.

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
# resolve the lockfile even though only adam-coder is built. Only a C compiler is needed
# (aws-lc-sys uses its pregenerated sources; the vendored protoc covers a2a-pb).
COPY Cargo.toml Cargo.lock ./
COPY crates crates
COPY bin bin
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p adam-coder --bin adam-coder \
 && install -D -m 0755 target/release/adam-coder /out/adam-coder

FROM ghcr.io/vymalo/another-agentic-images/workspace:${WORKSPACE_TAG}

LABEL org.opencontainers.image.source="https://github.com/vymalo/another-adam-rs" \
      org.opencontainers.image.description="adam-coder: coding task to verified pull request over A2A" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0"

# hadolint ignore=DL3066
USER root
COPY --from=build /out/adam-coder /usr/local/bin/adam-coder
# /work holds mirrors and worktrees; the deployment mounts a volume there. The
# base image already creates it owned by uid 10001, as does this line for a
# base that does not.
RUN install -d -o 10001 -g 10001 /work

ENV WORKSPACE_ROOT=/work \
    LISTEN_ADDR=0.0.0.0:8080 \
    RUST_LOG=info \
    OPENCODE_DISABLE_AUTOUPDATE=1

# The runtime user of the workspace image (uid/gid 10001, `agent`).
USER 10001
WORKDIR /work
EXPOSE 8080

# Smoke test in a login shell as uid 10001: the binary starts and rejects an
# empty environment (it fails closed) instead of crashing, and the tools the
# agent shells out to are on PATH.
RUN bash -lc 'set -e; \
      test "$(id -u)" = 10001; \
      command -v adam-coder git opencode tini; \
      out="$(env -i PATH="$PATH" adam-coder 2>&1 || true)"; \
      echo "$out" | grep -q "invalid configuration"'

# tini as PID 1 reaps the shells and git processes the agent spawns and
# forwards SIGTERM, which starts a graceful shutdown.
ENTRYPOINT ["tini", "--", "adam-coder"]
