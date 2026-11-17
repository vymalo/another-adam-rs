# The image of a self-test task: the base image the dry run names (--build-arg BASE_IMAGE), the repository in
# repo/ as a git repository at /app, and one ignored dependency directory, so that the dependency links of the
# base (bench/lib/base.mjs) are exercised. The dry run makes the build context (bench/lib/dryrun.mjs).
ARG BASE_IMAGE
FROM ${BASE_IMAGE}
USER root
COPY repo/ /app/
RUN mkdir -p /app/node_modules/marker \
 && echo ok > /app/node_modules/marker/index.js \
 && chown -R 10001:10001 /app \
 && git config --system --add safe.directory '*'
WORKDIR /app
USER 10001
