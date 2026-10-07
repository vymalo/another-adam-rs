# A stand-in for the adam-rs image, for the operator's kind end-to-end (`tests/cluster.rs`): the pods the
# operator makes need something that answers `/healthz` on port 8080 as `adam-agent` and `adam-coder`
# would, and the real image is not what is being tested.
#
# What it keeps of the real image, because the operator depends on it:
#   * `adam-coder` is the ENTRYPOINT (the operator sets no command for the coder);
#   * `tini -- adam-agent` is how the operator starts the folder agent, so `tini` and `adam-agent` exist
#     and `tini` drops the `--` and runs the rest;
#   * the agent listens on 8080 and serves `/healthz` (the probes), and a card at the usual path.
# It reads no environment variable, so a missing Secret is still the kubelet's to notice.
# busybox has no /usr/local/bin: the RUN makes it.
#
# busybox 1.37.0 from the public ECR mirror of Docker Official Images, pinned by digest (the index of the
# tag, the same on Docker Hub; verified 2026-10-05, the Docker-Content-Digest of
# registry-1.docker.io/v2/library/busybox/manifests/1.37.0 and public.ecr.aws/v2/docker/library/busybox/manifests/1.37.0).
FROM public.ecr.aws/docker/library/busybox:1.37.0@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e

RUN mkdir -p /www/.well-known /usr/local/bin \
 && echo ok > /www/healthz \
 && echo '{"name":"stub","description":"a stand-in for the adam-rs image"}' > /www/.well-known/agent-card.json \
 && printf '#!/bin/sh\nexec httpd -f -p 8080 -h /www\n' > /usr/local/bin/stub-agent \
 && printf '#!/bin/sh\nshift\nexec "$@"\n' > /usr/local/bin/tini \
 && chmod 0755 /usr/local/bin/stub-agent /usr/local/bin/tini \
 && ln -s stub-agent /usr/local/bin/adam-agent \
 && ln -s stub-agent /usr/local/bin/adam-coder

USER 10001:10001
ENTRYPOINT ["/usr/local/bin/adam-coder"]
