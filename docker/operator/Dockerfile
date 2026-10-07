# syntax=docker/dockerfile:1.7
#
# ghcr.io/vymalo/another-adam-rs/operator: the operator binary (bin/adam-operator, crate adam-operator),
# which is all the image holds. Build from the REPOSITORY ROOT:
#
#   docker buildx build -f docker/operator/Dockerfile .
#
# Dependencies are compiled in their own layer (cargo-chef), so a change to the sources does not rebuild
# them. Builder and runtime are both Debian 13 (trixie, glibc 2.41): the dynamically linked binary runs
# unchanged on the distroless `cc` image, which holds libc, libgcc and the CA certificates and nothing else
# (no shell, no package manager). TLS is rustls on aws-lc-rs, as in the rest of this workspace (root Cargo.toml), so
# no libssl; only a C compiler is needed to build (aws-lc-sys uses its pregenerated sources).
#
# The default features of the binary are the ones the chart assumes: `runtime-kubernetes`, `store-cnpg` and
# `registry` (bin/adam-operator/README.md, "Features").
#
# Pins, by tag and digest of the image index (verified 2026-10-05: an anonymous token and a manifest request
# with the index Accept header, from the registry named, returned the digest below for the tag):
#   * lukemathwalker/cargo-chef 0.1.77-rust-1.94.1-trixie: cargo-chef 0.1.77 on Rust 1.94.1 (Docker Hub). The
#     workspace's `rust-version` is 1.94 and CI builds with 1.94.1 (`dtolnay/rust-toolchain@1.94` in ci.yml).
#   * gcr.io/distroless/cc-debian13:nonroot: Debian 13; its configuration says User 65532, which is what
#     the chart's `runAsUser` is (verified 2026-10-05, the image's config blob).
# A newer tag is a deliberate change: move the tag, the digest and these lines together.

FROM lukemathwalker/cargo-chef:0.1.77-rust-1.94.1-trixie@sha256:d5a1cca12f21de999e5b221b5c6ff5635080f4b8d5e58053241ff169e0fc60f6 AS chef
WORKDIR /src

# The recipe is the dependency graph (the manifests and the lockfile), so it only changes with them.
FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY crates crates
COPY bin bin
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS build
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json --package adam-operator
COPY Cargo.toml Cargo.lock ./
COPY crates crates
COPY bin bin
RUN cargo build --release --locked --package adam-operator --bin adam-operator
# Smoke test of what was built, in the build stage because the final image has no shell: the binary starts,
# prints its usage, and `crdgen` prints both CRDs. (CI runs the same two commands on the final image.)
RUN /src/target/release/adam-operator --help > /dev/null \
 && /src/target/release/adam-operator crdgen > /tmp/crds.yaml \
 && test "$(grep -c '^kind: CustomResourceDefinition$' /tmp/crds.yaml)" = 2

FROM gcr.io/distroless/cc-debian13:nonroot@sha256:e792ab3d241a468a4fd7519ddbbebe66b49b5f365771716ea688ad40b6c6f1c2
LABEL org.opencontainers.image.source="https://github.com/vymalo/another-adam-rs" \
      org.opencontainers.image.description="The adam-rs operator: AgentService and AgentConfig on native Kubernetes, and the agent registry" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0"
COPY --from=build /src/target/release/adam-operator /adam-operator
# Numeric, so Kubernetes can verify `runAsNonRoot` (this is distroless's `nonroot` user).
USER 65532:65532
# 8080 the registry (only with a token), 8081 health, 9090 metrics.
EXPOSE 8080 8081 9090
ENV RUST_LOG=info
ENTRYPOINT ["/adam-operator"]
CMD ["run"]
