# syntax=docker/dockerfile:1
#
# A throwaway git remote for local testing: bare repositories over smart HTTP
# (nginx + fcgiwrap + git-http-backend), seeded with every repository under
# `seed/<owner>/<name>/` (`local/sandbox.git`), and creating an empty repository on
# first use for the owners of AUTO_CREATE_OWNERS (cgi.sh). NO AUTHENTICATION: it
# accepts any credentials (or none), so it must only ever be reachable from your own
# machine. Build context: this directory (see compose.yaml).
#
# Base image tag verified 2026-09-29 against registry-1.docker.io
# (library/alpine tags list: 3.22.6 is the newest 3.22.x). The Alpine packages
# (git, git-daemon which ships git-http-backend, nginx, fcgiwrap) were checked
# in the v3.22 main/community APKINDEX the same day.
FROM alpine:3.22.6

# hadolint ignore=DL3018
RUN apk add --no-cache git git-daemon nginx fcgiwrap \
 && addgroup -g 10001 -S git \
 && adduser -u 10001 -S -G git -h /srv/git -s /bin/sh git \
 && install -d -o git -g git /srv/git

COPY nginx.conf /etc/nginx/nginx.conf
COPY cgi.sh /usr/local/bin/cgi.sh
COPY entrypoint.sh /usr/local/bin/entrypoint.sh
COPY seed /seed

# Repositories live in a volume, so pushes survive `docker compose stop`.
VOLUME /srv/git
USER 10001
WORKDIR /srv/git
EXPOSE 8080

HEALTHCHECK --interval=2s --timeout=3s --start-period=5s --retries=30 \
  CMD ["wget", "-q", "-O", "/dev/null", "http://127.0.0.1:8080/local/sandbox.git/info/refs?service=git-upload-pack"]

ENTRYPOINT ["/bin/sh", "/usr/local/bin/entrypoint.sh"]
