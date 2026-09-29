#!/bin/sh
# Seed the sandbox repository (once: the volume keeps it), then serve it.
set -eu

ROOT=/srv/git
REPO="$ROOT/local/sandbox.git"

if [ ! -d "$REPO" ]; then
  echo "git-server: seeding $REPO"
  work=$(mktemp -d)
  cp -R /seed/sandbox/. "$work/"
  (
    cd "$work"
    git init -q -b main
    git add -A
    git -c user.name="git-server seed" -c user.email="seed@git-server.invalid" \
      -c commit.gpgsign=false commit -q -m "Seed the sandbox repository"
  )
  mkdir -p "$ROOT/local"
  git clone -q --bare "$work" "$REPO"
  # Allow pushes over HTTP without authentication (local testing only).
  git --git-dir="$REPO" config http.receivepack true
  git --git-dir="$REPO" config http.uploadpack true
  rm -rf "$work"
fi

# fcgiwrap executes git-http-backend for nginx; the socket is in /tmp.
rm -f /tmp/fcgiwrap.sock
fcgiwrap -s unix:/tmp/fcgiwrap.sock &

echo "git-server: listening on :8080, repository local/sandbox.git"
exec nginx -g 'daemon off;'
