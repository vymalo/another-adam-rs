# syntax=docker/dockerfile:1
#
# The image of the `run-pods` CI job (deploy/coder/tests/kind-run-pods.sh), which plays two parts that the chart
# gives to two images: the run container (a shell, tini, coreutils) and the coder's image as the init container's
# source (/opt/adam/bin/adam-exec and an opencode). It is small on purpose: the real run image is the workspace
# image of another-agentic-images (2.92 GB compressed, verified 2026-10-09) and the real coder image takes a Rust
# build, and the job tests the chart's objects and the environment, not those images. The opencode here is a stub;
# adam-exec is the real script (one source with the devcontainer crate). Build from the REPOSITORY ROOT:
#
#   docker build -f docker/coder/test/run-pod.Dockerfile -t adam-run-test:ci .
#
# debian:trixie-slim by digest of its index (verified 2026-10-05: HEAD on registry-1.docker.io/v2/library/debian/
# manifests/trixie-slim, OCI index accept header).
FROM debian:trixie-slim@sha256:a99cfc517144bc59b1978475ec53b46ecabec7e43635402ee5b77cc54cd1b20a

# hadolint ignore=DL3008
RUN apt-get update \
 && apt-get install -y --no-install-recommends tini \
 && rm -rf /var/lib/apt/lists/*
COPY --chmod=0755 crates/adam-devcontainer/src/adam-exec.sh /opt/adam/bin/adam-exec
RUN printf '#!/bin/sh\necho opencode-stub\n' > /opt/adam/bin/opencode && chmod 0755 /opt/adam/bin/opencode
# The pod's own user, numeric, as the chart's template and its admission policy require.
USER 10001
WORKDIR /work
ENTRYPOINT ["tini", "--"]
