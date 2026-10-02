# The base image of the default environment of the dev stack (one pull serves both): the small
# base image of the devcontainers.dev project, 2.2.1-trixie, by digest only (the devcontainer CLI cannot
# parse a reference with both a tag and a digest). The coder's own image and the workspace image do not
# have devbox-tool either.
FROM mcr.microsoft.com/devcontainers/base@sha256:1f851004adcd3dff3776b4a1da86727cf52280b0fdc9d5b0568c9c7d74274286
COPY devbox-tool /usr/local/bin/devbox-tool
