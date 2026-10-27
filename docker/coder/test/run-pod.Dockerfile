# syntax=docker/dockerfile:1
#
# The image of the `run-pods` CI job (deploy/coder/tests/kind-run-pods.sh), which plays four parts that the chart gives to
# two images: the run container (a shell, tini, coreutils, curl and npm), the coder's image as the init container's source
# (/opt/adam/bin/adam-exec and an opencode), the coder's image as the egress proxy's (`adam-egress`, ADR 0042), and the coder's
# image as the coder pod's own two containers, the coder and its git sandbox (`adam-git-sandbox` and `adam-git`, ADR 0041
# open decision 6; the coder container's command is replaced by a sleep, because the real coder needs a database and a model).
# It is small on purpose: the real run image is the workspace image of another-agentic-images (2.92 GB compressed, verified
# 2026-10-09) and the real coder image takes a Rust build, and the job tests the chart's objects and the environment, not those
# images. The opencode here is a stub; adam-exec is the real script (one source with the devcontainer crate); adam-egress, and
# adam-git-sandbox with its relay adam-git, are the real binaries, built by the script with cargo and passed as the named build
# contexts `egress` (a directory holding the file `adam-egress`) and `gitsandbox` (holding `adam-git-sandbox` and `adam-git`).
# It has git because the isolation check (ADR 0041) runs git in a worktree whose mirror is mounted read-only, and the script
# seeds the volume with it, and because the git sandbox runs it; curl and npm because the network checks (ADR 0042) run both
# in a run pod, through the proxy, and ca-certificates because both speak TLS. Build from the REPOSITORY ROOT:
#
#   cargo build --release -p adam-egress -p adam-git-sandbox
#   mkdir -p /tmp/egress /tmp/gitsandbox
#   cp target/release/adam-egress /tmp/egress/ && cp target/release/adam-git-sandbox target/release/adam-git /tmp/gitsandbox/
#   docker build -f docker/coder/test/run-pod.Dockerfile --build-context egress=/tmp/egress \
#     --build-context gitsandbox=/tmp/gitsandbox -t adam-run-test:ci .
#
# debian:trixie-slim by digest of its index (verified 2026-10-05: HEAD on registry-1.docker.io/v2/library/debian/
# manifests/trixie-slim, OCI index accept header).
FROM debian:trixie-slim@sha256:a99cfc517144bc59b1978475ec53b46ecabec7e43635402ee5b77cc54cd1b20a

# hadolint ignore=DL3008
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl git npm tini \
 && rm -rf /var/lib/apt/lists/*
COPY --chmod=0755 crates/adam-devcontainer/src/adam-exec.sh /opt/adam/bin/adam-exec
COPY --from=egress --chmod=0755 adam-egress /usr/local/bin/adam-egress
COPY --from=gitsandbox --chmod=0755 adam-git-sandbox adam-git /usr/local/bin/
RUN printf '#!/bin/sh\necho opencode-stub\n' > /opt/adam/bin/opencode && chmod 0755 /opt/adam/bin/opencode
# The pod's own user, numeric, as the chart's template and its admission policy require.
USER 10001
WORKDIR /work
ENTRYPOINT ["tini", "--"]
