#!/bin/sh
# Seed the repositories under /seed/<owner>/<name>/ (each once: the volume keeps them), then
# serve every repository under /srv/git, creating an empty one on first use for an owner of
# AUTO_CREATE_OWNERS (see cgi.sh).
set -eu

ROOT=/srv/git
SEED=/seed

seed_repo() { # seed_repo <owner> <name> <directory of the files>
  owner=$1
  name=$2
  repo="$ROOT/$owner/$name.git"
  if [ -d "$repo" ]; then
    return
  fi
  echo "git-server: seeding $repo"
  work=$(mktemp -d)
  cp -R "$3/." "$work/"
  (
    cd "$work"
    git init -q -b main
    git add -A
    git -c user.name="git-server seed" -c user.email="seed@git-server.invalid" \
      -c commit.gpgsign=false commit -q -m "Seed the $owner/$name repository"
  )
  mkdir -p "$ROOT/$owner"
  git clone -q --bare "$work" "$repo"
  # Allow pushes over HTTP without authentication (local testing only).
  git --git-dir="$repo" config http.receivepack true
  git --git-dir="$repo" config http.uploadpack true
  rm -rf "$work"
}

for owner_dir in "$SEED"/*/; do
  [ -d "$owner_dir" ] || continue
  owner=$(basename "$owner_dir")
  for repo_dir in "$owner_dir"*/; do
    [ -d "$repo_dir" ] || continue
    seed_repo "$owner" "$(basename "$repo_dir")" "$repo_dir"
  done
done

# The owners whose repositories appear on first use (a comma or space list, empty = none): what
# the GitHub mock's "create a repository" answers with a clone URL here. cgi.sh reads this file,
# because fcgiwrap does not promise to pass its own environment on.
owners=$(printf '%s' "${AUTO_CREATE_OWNERS:-}" | tr ',' ' ')
printf '%s\n' "$owners" > /tmp/auto-create-owners

# fcgiwrap executes cgi.sh (git-http-backend) for nginx; the socket is in /tmp.
rm -f /tmp/fcgiwrap.sock
fcgiwrap -s unix:/tmp/fcgiwrap.sock &

echo "git-server: listening on :8080; auto-created owners: ${owners:-none}"
exec nginx -g 'daemon off;'
